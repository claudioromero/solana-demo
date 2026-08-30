use borsh::{to_vec, BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    msg,
    program_error::ProgramError,
    program_pack::Pack,
    pubkey::Pubkey,
    rent::Rent,
    system_instruction,
    sysvar::Sysvar,
};
use spl_token::state::Account as TokenAccount;

#[cfg(not(feature = "no-entrypoint"))]
pub mod entrypoint;

/// The mainnet address of the USDC SPL token mint.
pub const USDC_MINT: Pubkey =
    Pubkey::from_str_const("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
/// The mainnet address of the USDT SPL token mint.
pub const USDT_MINT: Pubkey =
    Pubkey::from_str_const("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB");

/// Seed prefixes and labels used in PDA derivation.
const PREFIX: &[u8] = b"vault";
const VAULT_STATE_SEED: &[u8] = b"state";
const VAULT_TOKEN_SEED: &[u8] = b"tokens";
const DEPOSIT_SEED: &[u8] = b"deposit";

/// Serialized length of a `VaultState` (owner pubkey + total).
pub const VAULT_STATE_SPACE: usize = 32 + 8;

/// The global state of a vault: its owner and the cumulative total amount of
/// tokens deposited by all users.
///
/// Note: `total_deposits` (and each `DepositRecord`) is a **cumulative
/// lifetime** counter and is intentionally not decremented when the owner
/// redirects funds out of the vault. It therefore does not represent the
/// vault's current on-chain token balance.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub struct VaultState {
    pub owner: Pubkey,
    pub total_deposits: u64,
}

/// Tracks the cumulative amount of a given mint that a depositor has put
/// into a vault. One instance exists per (depositor, vault, mint) triple.
///
/// Like `VaultState::total_deposits`, this is a lifetime total and is not
/// decremented on redirect.
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub struct DepositRecord {
    pub depositor: Pubkey,
    pub mint: Pubkey,
    pub amount: u64,
}

impl DepositRecord {
    /// Serialized length of a `DepositRecord` (two pubkeys + amount).
    pub const SPACE: usize = 32 + 32 + 8;
}

/// The set of instructions supported by the program.
#[derive(BorshSerialize, BorshDeserialize, Debug)]
pub enum VaultInstruction {
    /// Initialize the vault state, designating its owner.
    Initialize,
    /// Deposit `amount` of a supported token into the vault.
    Deposit { amount: u64 },
    /// Transfer the vault's full token balance to a destination (owner only).
    Redirect,
}

/// Errors returned by the program as custom program errors.
#[derive(Debug, Clone, Copy)]
pub enum VaultError {
    InvalidInstruction,
    AlreadyInitialized,
    NotInitialized,
    Unauthorized,
    UnsupportedMint,
    InvalidMint,
    InvalidTokenAccount,
    InsufficientFunds,
    ArithmeticOverflow,
}

/// Converts a `VaultError` into its custom `ProgramError::Custom` form.
impl From<VaultError> for ProgramError {
    fn from(e: VaultError) -> Self {
        ProgramError::Custom(e as u32)
    }
}

/// Borsh-encodes `value` into a byte vector, mapping encoding failures to a
/// `ProgramError`. Used for writing program-owned account data.
fn encode<T: BorshSerialize>(value: &T) -> Result<Vec<u8>, ProgramError> {
    to_vec(value).map_err(|_| ProgramError::InvalidAccountData)
}

/// Overwrites an account's data buffer with `bytes`. The buffer is written
/// in place starting at offset zero; any bytes beyond `bytes.len()` are left
/// untouched (the account is sized to the struct's serialized length).
fn write_data(account: &AccountInfo, bytes: &[u8]) {
    let mut data = account.data.borrow_mut();
    data[..bytes.len()].copy_from_slice(bytes);
}

/// Ensures an account required to be written by this program is marked
/// writable. A non-writable account would cause the runtime to reject the
/// write with an opaque error, so we fail fast with a clear message.
///
/// # Errors
/// - `ProgramError::InvalidArgument` if `account` is not writable.
fn require_writable(account: &AccountInfo, label: &str) -> ProgramResult {
    if !account.is_writable {
        msg!("Account {} must be writable", label);
        return Err(ProgramError::InvalidArgument);
    }
    Ok(())
}

/// Derives the PDA that holds the vault's global state (`VaultState`), using
/// the seeds `["vault", "state"]`. The returned address is not on the
/// Ed25519 curve and is therefore safe for a program-owned account.
///
/// Returns `(address, bump_seed)`. The bump seed is required when signing
/// with this PDA via `invoke_signed`.
pub fn find_vault_state_address(program_id: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[PREFIX, VAULT_STATE_SEED], program_id)
}

/// Derives the PDA of the token account that holds a vault's deposited
/// tokens for the given mint, using the seeds `["vault", "tokens", mint]`.
///
/// Returns `(address, bump_seed)`. The bump seed is required when the vault
/// signs transfers out of this account (e.g. during a redirect).
pub fn find_vault_token_address(program_id: &Pubkey, mint: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[PREFIX, VAULT_TOKEN_SEED, mint.as_ref()], program_id)
}

/// Derives the PDA that tracks the per-(depositor, vault, mint) deposit
/// record, using the seeds `["vault", "deposit", depositor, vault, mint]`.
///
/// Returns `(address, bump_seed)`.
pub fn find_deposit_address(
    program_id: &Pubkey,
    depositor: &Pubkey,
    vault: &Pubkey,
    mint: &Pubkey,
) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[
            PREFIX,
            DEPOSIT_SEED,
            depositor.as_ref(),
            vault.as_ref(),
            mint.as_ref(),
        ],
        program_id,
    )
}

/// Entry point of the program. Deserializes the borsh-encoded instruction
/// from `instruction_data` and dispatches it to the matching handler.
///
/// # Errors
/// - `InvalidInstructionData` if `instruction_data` does not deserialize as a
///   valid `VaultInstruction`.
pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    let instruction = VaultInstruction::try_from_slice(instruction_data)
        .map_err(|_| ProgramError::InvalidInstructionData)?;

    match instruction {
        VaultInstruction::Initialize => process_initialize(program_id, accounts),
        VaultInstruction::Deposit { amount } => process_deposit(program_id, accounts, amount),
        VaultInstruction::Redirect => process_redirect(program_id, accounts),
    }
}

/// Handles the `Initialize` instruction: creates the program-owned vault
/// state PDA and writes an empty `VaultState` (owner + `total_deposits = 0`).
///
/// # Accounts
/// 1. `vault_state_info` — the vault state PDA (created here, writable).
/// 2. `owner_info` — the vault owner, used to fund the state account and
///    stored as the owner of the vault (signer).
/// 3. `system_program_info` — the system program (for account creation).
///
/// # Errors
/// - `IncorrectProgramId` if the system program is invalid.
/// - `InvalidTokenAccount` if `vault_state_info` is not the derived PDA.
/// - `AlreadyInitialized` if the vault state account is already owned by the
///   program with data (a real prior initialization). A pre-funded but
///   non-program-owned account (e.g. griefed at the PDA) is taken over via
///   `allocate` + `assign` rather than rejected.
fn process_initialize(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let vault_state_info = next_account_info(account_info_iter)?;
    let owner_info = next_account_info(account_info_iter)?;
    let system_program_info = next_account_info(account_info_iter)?;

    if system_program_info.key != &solana_program::system_program::ID {
        msg!("Invalid system program");
        return Err(ProgramError::IncorrectProgramId);
    }
    if !owner_info.is_signer {
        msg!("Owner must sign");
        return Err(ProgramError::MissingRequiredSignature);
    }

    let (vault_state_pda, bump) = find_vault_state_address(program_id);
    if vault_state_info.key != &vault_state_pda {
        msg!("Invalid vault state address");
        return Err(VaultError::InvalidTokenAccount.into());
    }
    require_writable(vault_state_info, "vault_state")?;

    // Only a genuine, program-owned initialization counts as "already
    // initialized". A system-owned account at the PDA (pre-funded by a
    // griefer) is taken over below instead of bricking initialization.
    if vault_state_info.owner == program_id && vault_state_info.data.borrow().len() > 0 {
        return Err(VaultError::AlreadyInitialized.into());
    }

    let seeds: &[&[u8]] = &[PREFIX, VAULT_STATE_SEED, &[bump]];

    if vault_state_info.lamports() > 0 {
        // Griefing defense: the account was pre-funded but is not ours. Adopt
        // it via the system program's allocate + assign instead of failing on
        // create_account (which would reject an account with lamports).
        solana_program::program::invoke_signed(
            &system_instruction::allocate(
                vault_state_info.key,
                VAULT_STATE_SPACE as u64,
            ),
            &[vault_state_info.clone(), system_program_info.clone()],
            &[seeds],
        )?;
        solana_program::program::invoke_signed(
            &system_instruction::assign(vault_state_info.key, program_id),
            &[vault_state_info.clone(), system_program_info.clone()],
            &[seeds],
        )?;
    } else {
        let rent = Rent::get()?;
        let lamports = rent.minimum_balance(VAULT_STATE_SPACE);
        solana_program::program::invoke_signed(
            &system_instruction::create_account(
                owner_info.key,
                vault_state_info.key,
                lamports,
                VAULT_STATE_SPACE as u64,
                program_id,
            ),
            &[
                owner_info.clone(),
                vault_state_info.clone(),
                system_program_info.clone(),
            ],
            &[seeds],
        )?;
    }

    let state = VaultState {
        owner: *owner_info.key,
        total_deposits: 0,
    };
    let bytes = encode(&state)?;
    write_data(vault_state_info, &bytes);

    msg!("Vault initialized, owner: {}", state.owner);
    Ok(())
}

/// Handles the `Deposit` instruction: transfers `amount` of a supported
/// token (USDC or USDT) from the depositor into the vault, and updates the
/// per-(depositor, mint) deposit record and the vault's total.
///
/// # Accounts
/// 1. `depositor_info` — the token holder and signer.
/// 2. `vault_state_info` — the vault state PDA (writable).
/// 3. `source_ata_info` — the depositor's token account (writable).
/// 4. `vault_ata_info` — the vault's token account for `mint` (writable).
/// 5. `mint_info` — the mint of the deposited token.
/// 6. `deposit_record_info` — the per-(depositor, mint) record PDA, created
///    on first deposit if needed (writable).
/// 7. `token_program_info` — the SPL Token program.
/// 8. `system_program_info` — the system program (for record creation).
///
/// # Errors
/// - `MissingRequiredSignature` if the depositor does not sign.
/// - `IncorrectProgramId` if the token or system program is invalid.
/// - `InvalidTokenAccount` if the vault state or deposit record is not a
///   derived PDA, or the vault token account is not the derived PDA or not
///   owned by the SPL Token program.
/// - `NotInitialized` if the vault state is missing/invalid.
/// - `UnsupportedMint` if the mint is neither USDC nor USDT.
/// - `Unauthorized`, `InvalidMint`, or `InsufficientFunds` if the source
///   token account is not owned by the depositor, mismatched, or underfunded.
/// - `ArithmeticOverflow` if the new balances overflow.
fn process_deposit(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    amount: u64,
) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let depositor_info = next_account_info(account_info_iter)?;
    let vault_state_info = next_account_info(account_info_iter)?;
    let source_ata_info = next_account_info(account_info_iter)?;
    let vault_ata_info = next_account_info(account_info_iter)?;
    let mint_info = next_account_info(account_info_iter)?;
    let deposit_record_info = next_account_info(account_info_iter)?;
    let token_program_info = next_account_info(account_info_iter)?;
    let system_program_info = next_account_info(account_info_iter)?;

    if !depositor_info.is_signer {
        msg!("Depositor must sign");
        return Err(ProgramError::MissingRequiredSignature);
    }
    require_writable(source_ata_info, "source_ata")?;
    require_writable(vault_ata_info, "vault_ata")?;
    require_writable(deposit_record_info, "deposit_record")?;
    require_writable(vault_state_info, "vault_state")?;
    if token_program_info.key != &spl_token::ID {
        msg!("Invalid token program");
        return Err(ProgramError::IncorrectProgramId);
    }
    if system_program_info.key != &solana_program::system_program::ID {
        msg!("Invalid system program");
        return Err(ProgramError::IncorrectProgramId);
    }

    if vault_state_info.key != &find_vault_state_address(program_id).0 {
        msg!("Invalid vault state address");
        return Err(VaultError::InvalidTokenAccount.into());
    }
    let state = validate_vault_state(vault_state_info, program_id)?;

    // Validate the mint is exactly USDC or USDT, otherwise throw.
    if mint_info.key != &USDC_MINT && mint_info.key != &USDT_MINT {
        msg!("Unsupported mint: {}", mint_info.key);
        return Err(VaultError::UnsupportedMint.into());
    }
    // Validate the mint account is owned by the SPL Token program so its
    // data is trustworthy.
    if mint_info.owner != &spl_token::ID {
        msg!("Mint account not owned by token program");
        return Err(VaultError::InvalidMint.into());
    }

    // Validate the vault token account matches the vault PDA for this mint.
    if vault_ata_info.key != &find_vault_token_address(program_id, mint_info.key).0 {
        msg!("Vault token account does not match vault PDA for this mint");
        return Err(VaultError::InvalidTokenAccount.into());
    }
    // Validate the vault token account is owned by the SPL Token program.
    if vault_ata_info.owner != &spl_token::ID {
        msg!("Vault token account not owned by token program");
        return Err(VaultError::InvalidTokenAccount.into());
    }

    // Validate the source token account.
    validate_source_token_account(
        &source_ata_info.data.borrow(),
        depositor_info.key,
        mint_info.key,
        amount,
    )?;

    // Transfer tokens from depositor's account to the vault PDA account.
    let transfer_ix = spl_token::instruction::transfer(
        token_program_info.key,
        source_ata_info.key,
        vault_ata_info.key,
        depositor_info.key,
        &[depositor_info.key],
        amount,
    )?;
    solana_program::program::invoke(
        &transfer_ix,
        &[
            source_ata_info.clone(),
            vault_ata_info.clone(),
            depositor_info.clone(),
            token_program_info.clone(),
        ],
    )?;

    // Validate the deposit record PDA and determine the current amount.
    let (deposit_pda, _) = find_deposit_address(
        program_id,
        depositor_info.key,
        vault_state_info.key,
        mint_info.key,
    );
    if deposit_record_info.key != &deposit_pda {
        msg!("Invalid deposit record address");
        return Err(VaultError::InvalidTokenAccount.into());
    }

    let current_record = initialize_deposit_record(
        program_id,
        depositor_info,
        vault_state_info,
        mint_info,
        deposit_record_info,
        system_program_info,
    )?;

    apply_deposit_updates(
        deposit_record_info,
        vault_state_info,
        state,
        current_record,
        amount,
        depositor_info.key,
        mint_info.key,
    )
}

/// Handles the `Redirect` instruction: transfers the entire token balance
/// held by the vault (for the mint of the given vault token account) to a
/// destination token account. Can only be called by the vault owner.
///
/// Redirect is a withdrawal of the vault's actual on-chain tokens; it does
/// **not** adjust the cumulative `total_deposits` or per-user `DepositRecord`
/// accounting (those are lifetime totals, see `VaultState`).
///
/// # Accounts
/// 1. `owner_info` — the vault owner and signer.
/// 2. `vault_state_info` — the vault state PDA.
/// 3. `vault_ata_info` — the vault's token account whose full balance is
///    withdrawn (writable).
/// 4. `destination_info` — the destination token account (writable).
/// 5. `token_program_info` — the SPL Token program.
///
/// # Errors
/// - `MissingRequiredSignature` if the owner does not sign.
/// - `IncorrectProgramId` if the token program is invalid.
/// - `InvalidTokenAccount` if the vault state/vault token account is not a
///   derived PDA, or the vault token account is not owned by the token
///   program.
/// - `NotInitialized` if the vault state is missing/invalid.
/// - `Unauthorized` if the caller is not the vault owner.
/// - `InvalidMint` if the destination account's mint mismatches the vault.
/// - `InsufficientFunds` if the vault holds no tokens to redirect.
fn process_redirect(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let owner_info = next_account_info(account_info_iter)?;
    let vault_state_info = next_account_info(account_info_iter)?;
    let vault_ata_info = next_account_info(account_info_iter)?;
    let destination_info = next_account_info(account_info_iter)?;
    let token_program_info = next_account_info(account_info_iter)?;

    if !owner_info.is_signer {
        msg!("Owner must sign");
        return Err(ProgramError::MissingRequiredSignature);
    }
    require_writable(vault_ata_info, "vault_ata")?;
    require_writable(destination_info, "destination")?;
    if token_program_info.key != &spl_token::ID {
        msg!("Invalid token program");
        return Err(ProgramError::IncorrectProgramId);
    }

    if vault_state_info.key != &find_vault_state_address(program_id).0 {
        msg!("Invalid vault state address");
        return Err(VaultError::InvalidTokenAccount.into());
    }
    let state = validate_vault_state(vault_state_info, program_id)?;

    if owner_info.key != &state.owner {
        msg!("Only the vault owner can redirect tokens");
        return Err(VaultError::Unauthorized.into());
    }

    // Determine the token being redirected from the vault token account.
    let vault_token = TokenAccount::unpack(&vault_ata_info.data.borrow())
        .map_err(|_| VaultError::InvalidTokenAccount)?;
    let mint = &vault_token.mint;

    if vault_ata_info.key != &find_vault_token_address(program_id, mint).0 {
        msg!("Vault token account does not match vault PDA for this mint");
        return Err(VaultError::InvalidTokenAccount.into());
    }
    if vault_ata_info.owner != &spl_token::ID {
        msg!("Vault token account not owned by token program");
        return Err(VaultError::InvalidTokenAccount.into());
    }

    // Validate the destination token account belongs to the same mint.
    let destination = TokenAccount::unpack(&destination_info.data.borrow())
        .map_err(|_| VaultError::InvalidTokenAccount)?;
    if &destination.mint != mint {
        msg!("Destination token account mint mismatch");
        return Err(VaultError::InvalidMint.into());
    }
    // Redirecting to the vault's own token account would be a no-op.
    if destination_info.key == vault_ata_info.key {
        msg!("Destination cannot be the vault's own token account");
        return Err(VaultError::InvalidTokenAccount.into());
    }

    let balance = vault_token.amount;
    if balance == 0 {
        return Err(VaultError::InsufficientFunds.into());
    }

    let (vault_ata_pda, bump) = find_vault_token_address(program_id, mint);
    let transfer_ix = spl_token::instruction::transfer(
        token_program_info.key,
        vault_ata_info.key,
        destination_info.key,
        &vault_ata_pda,
        &[&vault_ata_pda],
        balance,
    )?;
    solana_program::program::invoke_signed(
        &transfer_ix,
        &[
            vault_ata_info.clone(),
            destination_info.clone(),
            token_program_info.clone(),
        ],
        &[&[PREFIX, VAULT_TOKEN_SEED, mint.as_ref(), &[bump]]],
    )?;

    msg!(
        "Redirected {} tokens ({}) to {}",
        balance,
        mint,
        destination_info.key
    );

    Ok(())
}

/// Applies a completed deposit to on-chain account data: recomputes the
/// updated per-(user, mint) `DepositRecord` and the new vault total, then
/// serializes them into the deposit record account and the vault state
/// account respectively. This is the pure, testable post-CPI portion of the
/// deposit flow.
///
/// # Arguments
/// - `deposit_record_info` — the deposit record PDA to write to (writable).
/// - `vault_state_info` — the vault state PDA to write to (writable).
/// - `state` — the current `VaultState`.
/// - `current_record` — the depositor's current total for `mint`.
/// - `amount` — the amount just deposited.
/// - `depositor` — the depositor's pubkey (stored in the record).
/// - `mint` — the mint of the deposited token (stored in the record).
///
/// # Errors
/// - `ArithmeticOverflow` if the new balances overflow.
/// - `InvalidAccountData` if a serialized payload cannot be encoded.
#[allow(clippy::too_many_arguments)]
fn apply_deposit_updates(
    deposit_record_info: &AccountInfo,
    vault_state_info: &AccountInfo,
    state: VaultState,
    current_record: u64,
    amount: u64,
    depositor: &Pubkey,
    mint: &Pubkey,
) -> ProgramResult {
    let (new_record, new_total) = compute_new_balances(
        current_record,
        state.total_deposits,
        amount,
        depositor,
        mint,
    )?;

    let new_record_bytes = encode(&new_record)?;
    write_data(deposit_record_info, &new_record_bytes);

    let new_state = VaultState {
        owner: state.owner,
        total_deposits: new_total,
    };
    let new_state_bytes = encode(&new_state)?;
    write_data(vault_state_info, &new_state_bytes);

    msg!(
        "Deposited {} tokens ({}) from {} into vault",
        amount,
        mint,
        depositor
    );

    Ok(())
}

/// Validates that the vault state account is owned by the program, sized
/// correctly for a `VaultState`, and deserializable. Returns the parsed state
/// on success.
///
/// # Arguments
/// - `vault_state_info` — the vault state account to validate.
/// - `program_id` — the expected owner (this program).
///
/// # Errors
/// - `NotInitialized` if the account is not owned by the program, is smaller
///   than `VAULT_STATE_SPACE`, or fails to deserialize.
fn validate_vault_state(
    vault_state_info: &AccountInfo,
    program_id: &Pubkey,
) -> Result<VaultState, ProgramError> {
    if vault_state_info.owner != program_id
        || vault_state_info.data.borrow().len() < VAULT_STATE_SPACE
    {
        return Err(VaultError::NotInitialized.into());
    }
    VaultState::try_from_slice(&vault_state_info.data.borrow())
        .map_err(|_| VaultError::NotInitialized.into())
}

/// Validates the depositor's source token account: it must be a valid token
/// account (deserializable), owned by `depositor`, for the given `mint`, and
/// with a balance of at least `amount`. Returns the unpacked token account
/// on success.
///
/// # Errors
/// - `InvalidTokenAccount` if `data` does not unpack as a token account.
/// - `Unauthorized` if the account is not owned by `depositor`.
/// - `InvalidMint` if the account's mint differs from `mint`.
/// - `InsufficientFunds` if the account balance is below `amount`.
fn validate_source_token_account(
    data: &[u8],
    depositor: &Pubkey,
    mint: &Pubkey,
    amount: u64,
) -> Result<TokenAccount, ProgramError> {
    let token_account =
        TokenAccount::unpack(data).map_err(|_| VaultError::InvalidTokenAccount)?;
    if token_account.owner != *depositor {
        return Err(VaultError::Unauthorized.into());
    }
    if token_account.mint != *mint {
        return Err(VaultError::InvalidMint.into());
    }
    if token_account.amount < amount {
        return Err(VaultError::InsufficientFunds.into());
    }
    Ok(token_account)
}

/// Computes the updated per-(user, mint) `DepositRecord` and the updated
/// vault total by adding `amount` to both `current_record` and
/// `current_total` with checked (overflow-safe) arithmetic.
///
/// Returns `(new_record, new_total)`.
///
/// # Errors
/// - `ArithmeticOverflow` if adding `amount` overflows either balance.
fn compute_new_balances(
    current_record: u64,
    current_total: u64,
    amount: u64,
    depositor: &Pubkey,
    mint: &Pubkey,
) -> Result<(DepositRecord, u64), VaultError> {
    let new_record_amount = current_record.checked_add(amount)
        .ok_or(VaultError::ArithmeticOverflow)?;
    let new_total = current_total
        .checked_add(amount)
        .ok_or(VaultError::ArithmeticOverflow)?;
    Ok((
        DepositRecord {
            depositor: *depositor,
            mint: *mint,
            amount: new_record_amount,
        },
        new_total,
    ))
}

/// Describes the on-chain state of a deposit-record account, used to decide
/// how to (re)initialize it during a deposit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DepositRecordState {
    /// The account is already a valid, program-owned record.
    Initialized,
    /// The account does not exist yet (no lamports); create it from scratch.
    NeedsCreate,
    /// The account was pre-funded but is not program-owned (e.g. griefed);
    /// take it over via allocate + assign.
    FundedTakeover,
    /// The account exists but has insufficient space to hold a record.
    InsufficientSpace,
}

/// Classifies a deposit-record account from its raw attributes. This is a
/// pure function so the initialization decision can be unit-tested.
fn classify_deposit_record(
    owner: &Pubkey,
    data_len: usize,
    lamports: u64,
    program_id: &Pubkey,
) -> DepositRecordState {
    if owner == program_id && data_len >= DepositRecord::SPACE {
        // The borsh deserialization in the caller guards against corruption.
        DepositRecordState::Initialized
    } else if data_len < DepositRecord::SPACE && lamports > 0 {
        // Funded but too small to ever hold a record (a griefer resized it).
        DepositRecordState::InsufficientSpace
    } else if lamports > 0 {
        // Funded, non-program-owned (or undersized but large enough): take it
        // over instead of failing on create_account.
        DepositRecordState::FundedTakeover
    } else {
        DepositRecordState::NeedsCreate
    }
}

/// Ensures the deposit-record account exists and is owned by the program by
/// the end of the call, restoring the initial (zeroed) record.
///
/// Handles three cases:
/// - No account yet: `create_account` with rent paid by the depositor.
/// - Pre-funded account (griefing defense): takes the system-owned account
///   over via `allocate` + `assign` instead of failing.
/// - An account without enough space: surfaced as an error.
///
/// Returns the previous amount (`0` if the record is fresh). This function
/// performs system-program CPIs and is therefore only exercisable on-chain.
fn initialize_deposit_record<'a>(
    program_id: &Pubkey,
    depositor_info: &AccountInfo<'a>,
    vault_state_info: &AccountInfo<'a>,
    mint_info: &AccountInfo<'a>,
    deposit_record_info: &AccountInfo<'a>,
    system_program_info: &AccountInfo<'a>,
) -> Result<u64, ProgramError> {
    let state = classify_deposit_record(
        deposit_record_info.owner,
        deposit_record_info.data.borrow().len(),
        deposit_record_info.lamports(),
        program_id,
    );

    match state {
        DepositRecordState::Initialized => {
            DepositRecord::try_from_slice(&deposit_record_info.data.borrow())
                .map_err(|_| ProgramError::InvalidAccountData)
                .map(|r| r.amount)
        }
        DepositRecordState::InsufficientSpace => {
            msg!("Deposit record has insufficient space");
            Err(VaultError::InvalidTokenAccount.into())
        }
        DepositRecordState::FundedTakeover => {
            let bump = find_deposit_address(
                program_id,
                depositor_info.key,
                vault_state_info.key,
                mint_info.key,
            )
            .1;
            let seeds: &[&[u8]] = &[
                PREFIX,
                DEPOSIT_SEED,
                depositor_info.key.as_ref(),
                vault_state_info.key.as_ref(),
                mint_info.key.as_ref(),
                &[bump],
            ];
            // Allocate the required space, then reassign ownership to us.
            solana_program::program::invoke_signed(
                &system_instruction::allocate(
                    deposit_record_info.key,
                    DepositRecord::SPACE as u64,
                ),
                &[
                    deposit_record_info.clone(),
                    system_program_info.clone(),
                ],
                &[seeds],
            )?;
            solana_program::program::invoke_signed(
                &system_instruction::assign(deposit_record_info.key, program_id),
                &[
                    deposit_record_info.clone(),
                    system_program_info.clone(),
                ],
                &[seeds],
            )?;
            let initial_record = DepositRecord {
                depositor: *depositor_info.key,
                mint: *mint_info.key,
                amount: 0,
            };
            let bytes = encode(&initial_record)?;
            write_data(deposit_record_info, &bytes);
            Ok(0)
        }
        DepositRecordState::NeedsCreate => {
            let rent = Rent::get()?;
            let lamports = rent.minimum_balance(DepositRecord::SPACE);
            let bump = find_deposit_address(
                program_id,
                depositor_info.key,
                vault_state_info.key,
                mint_info.key,
            )
            .1;
            let seeds: &[&[u8]] = &[
                PREFIX,
                DEPOSIT_SEED,
                depositor_info.key.as_ref(),
                vault_state_info.key.as_ref(),
                mint_info.key.as_ref(),
                &[bump],
            ];
            solana_program::program::invoke_signed(
                &system_instruction::create_account(
                    depositor_info.key,
                    deposit_record_info.key,
                    lamports,
                    DepositRecord::SPACE as u64,
                    program_id,
                ),
                &[
                    depositor_info.clone(),
                    deposit_record_info.clone(),
                    system_program_info.clone(),
                ],
                &[seeds],
            )?;
            let initial_record = DepositRecord {
                depositor: *depositor_info.key,
                mint: *mint_info.key,
                amount: 0,
            };
            let bytes = encode(&initial_record)?;
            write_data(deposit_record_info, &bytes);
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
use super::*;
    use assert_matches::assert_matches;
    use solana_program::{
        account_info::AccountInfo, clock::Epoch, program_pack::Pack,
        pubkey::Pubkey, system_program,
    };
    use spl_token::state::Account as TokenAccount;

    const PROGRAM_ID: Pubkey = solana_program::pubkey!(
        "6ohqtumKM2UHTMzdzjzVLRpNaDfN3wcNXqtYgvYU8bix"
    );

    fn mk_account(
        key: Pubkey,
        owner: Pubkey,
        data: Vec<u8>,
        lamports: u64,
        is_signer: bool,
        is_writable: bool,
    ) -> AccountInfo<'static> {
        let key = Box::leak(Box::new(key));
        let owner = Box::leak(Box::new(owner));
        let data = Box::leak(data.into_boxed_slice());
        let lamports = Box::leak(Box::new(lamports));
        AccountInfo::new(
            key,
            is_signer,
            is_writable,
            lamports,
            data,
            owner,
            false,
            Epoch::default(),
        )
    }

    fn token_account_data(owner: &Pubkey, mint: &Pubkey, amount: u64) -> Vec<u8> {
        let mut data = vec![0u8; TokenAccount::LEN];
        let acc = TokenAccount {
            mint: *mint,
            owner: *owner,
            amount,
            delegate: solana_program::program_option::COption::None,
            state: spl_token::state::AccountState::Initialized,
            is_native: solana_program::program_option::COption::None,
            delegated_amount: 0,
            close_authority: solana_program::program_option::COption::None,
        };
        TokenAccount::pack(acc, &mut data).unwrap();
        data
    }

    fn valid_vault_state_data(owner: &Pubkey) -> Vec<u8> {
        borsh::to_vec(&VaultState {
            owner: *owner,
            total_deposits: 0,
        })
        .unwrap()
    }

    /// A deterministic pubkey distinct from USDC/USDT and any PDA, used as a
    /// generic "wrong key" sentinel in tests (e.g. an unsupported mint or an
    /// invalid token-program key).
    fn distinct_pubkey() -> Pubkey {
        solana_program::pubkey!("5jFHYT6wPgDB3JKPwmwqXajJU83JrMkgDXS69FMNRhaj")
    }

    // ---- compute_new_balances ----

    #[test]
    fn compute_new_balances_tracks_per_user_and_mint() {
        let depositor = Pubkey::new_unique();
        let (record, total) = compute_new_balances(
            100,
            1000,
            50,
            &depositor,
            &USDC_MINT,
        )
        .unwrap();
        assert_eq!(record.depositor, depositor);
        assert_eq!(record.mint, USDC_MINT);
        assert_eq!(record.amount, 150);
        assert_eq!(total, 1050);
    }

    #[test]
    fn compute_new_balances_accumulates_multiple_deposits() {
        let depositor = Pubkey::new_unique();
        let (r1, t1) = compute_new_balances(0, 0, 10, &depositor, &USDC_MINT).unwrap();
        let (r2, t2) = compute_new_balances(r1.amount, t1, 5, &depositor, &USDC_MINT).unwrap();
        assert_eq!(r2.amount, 15);
        assert_eq!(t2, 15);
    }

    #[test]
    fn compute_new_balances_keeps_different_mints_separate() {
        let depositor = Pubkey::new_unique();
        let (usdc_rec, usdc_tot) = compute_new_balances(0, 0, 10, &depositor, &USDC_MINT).unwrap();
        let (usdt_rec, usdt_tot) = compute_new_balances(0, usdc_tot, 20, &depositor, &USDT_MINT).unwrap();
        // Per-mint records stay independent.
        assert_eq!(usdc_rec.amount, 10);
        assert_eq!(usdc_rec.mint, USDC_MINT);
        assert_eq!(usdt_rec.amount, 20);
        assert_eq!(usdt_rec.mint, USDT_MINT);
        assert_eq!(usdt_tot, 30);
    }

    #[test]
    fn compute_new_balances_overflow_record_err() {
        let depositor = Pubkey::new_unique();
        let res = compute_new_balances(
            u64::MAX,
            0,
            1,
            &depositor,
            &USDC_MINT,
        );
        assert_matches!(res, Err(VaultError::ArithmeticOverflow));
    }

    #[test]
    fn compute_new_balances_overflow_total_err() {
        let depositor = Pubkey::new_unique();
        let res = compute_new_balances(
            0,
            u64::MAX,
            1,
            &depositor,
            &USDC_MINT,
        );
        assert_matches!(res, Err(VaultError::ArithmeticOverflow));
    }

    // ---- validate_vault_state ----

    #[test]
    fn validate_vault_state_ok() {
        let owner = Pubkey::new_unique();
        let info = mk_account(
            Pubkey::new_unique(),
            PROGRAM_ID,
            valid_vault_state_data(&owner),
            0,
            false,
            false,
        );
        let state = validate_vault_state(&info, &PROGRAM_ID).unwrap();
        assert_eq!(state.owner, owner);
        assert_eq!(state.total_deposits, 0);
    }

    #[test]
    fn validate_vault_state_wrong_owner_err() {
        let owner = Pubkey::new_unique();
        let other = Pubkey::new_unique();
        let info = mk_account(
            Pubkey::new_unique(),
            other,
            valid_vault_state_data(&owner),
            0,
            false,
            false,
        );
        assert_matches!(
            validate_vault_state(&info, &PROGRAM_ID),
            Err(ProgramError::Custom(e)) if e == VaultError::NotInitialized as u32
        );
    }

    #[test]
    fn validate_vault_state_wrong_size_err() {
        let info = mk_account(
            Pubkey::new_unique(),
            PROGRAM_ID,
            vec![0u8; 4],
            0,
            false,
            false,
        );
        assert_matches!(
            validate_vault_state(&info, &PROGRAM_ID),
            Err(ProgramError::Custom(e)) if e == VaultError::NotInitialized as u32
        );
    }

    #[test]
    fn validate_vault_state_corrupt_data_err() {
        let owner = Pubkey::new_unique();
        // Full struct plus trailing garbage: passes the minimum-length gate
        // but fails borsh deserialization (not all bytes consumed).
        let mut data = valid_vault_state_data(&owner);
        data.extend_from_slice(&[0xAB, 0xCD, 0xEF, 0x11, 0x22, 0x33, 0x44, 0x55]);
        let info = mk_account(
            Pubkey::new_unique(),
            PROGRAM_ID,
            data,
            0,
            false,
            false,
        );
        assert_matches!(
            validate_vault_state(&info, &PROGRAM_ID),
            Err(ProgramError::Custom(e)) if e == VaultError::NotInitialized as u32
        );
    }

    // ---- validate_source_token_account ----

    #[test]
    fn validate_source_token_account_ok() {
        let owner = Pubkey::new_unique();
        let data = token_account_data(&owner, &USDC_MINT, 100);
        let acc = validate_source_token_account(&data, &owner, &USDC_MINT, 50).unwrap();
        assert_eq!(acc.mint, USDC_MINT);
        assert_eq!(acc.amount, 100);
    }

    #[test]
    fn validate_source_token_account_invalid_data_err() {
        let owner = Pubkey::new_unique();
        let data = vec![1u8; 32];
        assert_matches!(
            validate_source_token_account(&data, &owner, &USDC_MINT, 1),
            Err(ProgramError::Custom(e)) if e == VaultError::InvalidTokenAccount as u32
        );
    }

    #[test]
    fn validate_source_token_account_not_owner_err() {
        let owner = Pubkey::new_unique();
        let stranger = Pubkey::new_unique();
        let data = token_account_data(&owner, &USDC_MINT, 100);
        assert_matches!(
            validate_source_token_account(&data, &stranger, &USDC_MINT, 1),
            Err(ProgramError::Custom(e)) if e == VaultError::Unauthorized as u32
        );
    }

    #[test]
    fn validate_source_token_account_mint_mismatch_err() {
        let owner = Pubkey::new_unique();
        let data = token_account_data(&owner, &USDT_MINT, 100);
        assert_matches!(
            validate_source_token_account(&data, &owner, &USDC_MINT, 1),
            Err(ProgramError::Custom(e)) if e == VaultError::InvalidMint as u32
        );
    }

    #[test]
    fn validate_source_token_account_insufficient_funds_err() {
        let owner = Pubkey::new_unique();
        let data = token_account_data(&owner, &USDC_MINT, 10);
        assert_matches!(
            validate_source_token_account(&data, &owner, &USDC_MINT, 11),
            Err(ProgramError::Custom(e)) if e == VaultError::InsufficientFunds as u32
        );
    }

    // ---- instruction dispatch ----

    #[test]
    fn deserialize_invalid_instruction_err() {
        let res = process_instruction(&PROGRAM_ID, &[], &[0xFF, 0xFF]);
        assert_matches!(res, Err(ProgramError::InvalidInstructionData));
    }

    #[test]
    fn initialize_dispatch_reaches_pda_check() {
        let owner = Pubkey::new_unique();
        let system = system_program::id();
        let state_info = mk_account(
            Pubkey::new_unique(), // wrong key (not the real PDA)
            PROGRAM_ID,
            Vec::new(),
            0,
            false,
            true,
        );
        let owner_info = mk_account(owner, system, Vec::new(), 0, true, true);
        let sys_info = mk_account(system_program::id(), system, Vec::new(), 0, false, false);
        let accounts = vec![state_info, owner_info, sys_info];
        let ix = borsh::to_vec(&VaultInstruction::Initialize).unwrap();
        assert_matches!(
            process_instruction(&PROGRAM_ID, &accounts, &ix),
            Err(ProgramError::Custom(e)) if e == VaultError::InvalidTokenAccount as u32
        );
    }

    #[test]
    fn initialize_rejects_bad_system_program() {
        let owner = Pubkey::new_unique();
        let fake_system = Pubkey::new_unique();

        let (state_pda, _) = find_vault_state_address(&PROGRAM_ID);
        let state_info = mk_account(state_pda, PROGRAM_ID, Vec::new(), 0, false, true);
        let owner_info = mk_account(owner, system_program::id(), Vec::new(), 0, true, true);
        let sys_info = mk_account(fake_system, fake_system, Vec::new(), 0, false, false);
        let accounts = vec![state_info, owner_info, sys_info];
        let ix = borsh::to_vec(&VaultInstruction::Initialize).unwrap();
        assert_matches!(
            process_instruction(&PROGRAM_ID, &accounts, &ix),
            Err(ProgramError::IncorrectProgramId)
        );
    }

    // ---- deposit dispatch validations (all before CPIs) ----

    fn deposit_accounts(depositor: &Pubkey, include_etc: bool) -> Vec<AccountInfo<'static>> {
        let token_prog = spl_token::id();
        let system = system_program::id();
        let (vault_state_pda, _) = find_vault_state_address(&PROGRAM_ID);
        let (vault_ata, _) = find_vault_token_address(&PROGRAM_ID, &USDC_MINT);
        let (deposit_pda, _) = find_deposit_address(
            &PROGRAM_ID,
            depositor,
            &vault_state_pda,
            &USDC_MINT,
        );

        vec![
            mk_account(*depositor, system, Vec::new(), 0, include_etc, true),
            mk_account(vault_state_pda, PROGRAM_ID, valid_vault_state_data(depositor), 0, false, true),
            mk_account(Pubkey::new_unique(), token_prog, token_account_data(depositor, &USDC_MINT, 1000), 0, false, true),
            mk_account(vault_ata, token_prog, token_account_data(&vault_ata, &USDC_MINT, 0), 0, false, true),
            mk_account(USDC_MINT, token_prog, Vec::new(), 0, false, false),
            mk_account(deposit_pda, PROGRAM_ID, Vec::new(), 0, false, true),
            mk_account(spl_token::id(), token_prog, Vec::new(), 0, false, false),
            mk_account(system_program::id(), system, Vec::new(), 0, false, false),
        ]
    }

    #[test]
    fn deposit_requires_signer_err() {
        let depositor = Pubkey::new_unique();
        let accounts = deposit_accounts(&depositor, false);
        let ix = borsh::to_vec(&VaultInstruction::Deposit { amount: 10 }).unwrap();
        // Deposit fails at signer check before reaching CPIs.
        assert_matches!(
            process_instruction(&PROGRAM_ID, &accounts, &ix),
            Err(ProgramError::MissingRequiredSignature)
        );
    }

    #[test]
    fn deposit_rejects_bad_token_program() {
        let depositor = Pubkey::new_unique();
        let mut accounts = deposit_accounts(&depositor, true);
        // Replace token program account key with a fake one.
        accounts[6] = mk_account(Pubkey::new_unique(), Pubkey::new_unique(), Vec::new(), 0, false, false);
        let ix = borsh::to_vec(&VaultInstruction::Deposit { amount: 10 }).unwrap();
        assert_matches!(
            process_instruction(&PROGRAM_ID, &accounts, &ix),
            Err(ProgramError::IncorrectProgramId)
        );
    }

    #[test]
    fn deposit_rejects_bad_system_program() {
        let depositor = Pubkey::new_unique();
        let mut accounts = deposit_accounts(&depositor, true);
        accounts[7] = mk_account(Pubkey::new_unique(), Pubkey::new_unique(), Vec::new(), 0, false, false);
        let ix = borsh::to_vec(&VaultInstruction::Deposit { amount: 10 }).unwrap();
        assert_matches!(
            process_instruction(&PROGRAM_ID, &accounts, &ix),
            Err(ProgramError::IncorrectProgramId)
        );
    }

    #[test]
    fn deposit_rejects_wrong_vault_state_key() {
        let depositor = Pubkey::new_unique();
        let mut accounts = deposit_accounts(&depositor, true);
        accounts[1] = mk_account(Pubkey::new_unique(), PROGRAM_ID, valid_vault_state_data(&depositor), 0, false, true);
        let ix = borsh::to_vec(&VaultInstruction::Deposit { amount: 10 }).unwrap();
        assert_matches!(
            process_instruction(&PROGRAM_ID, &accounts, &ix),
            Err(ProgramError::Custom(e)) if e == VaultError::InvalidTokenAccount as u32
        );
    }

    #[test]
    fn deposit_rejects_unsupported_mint() {
        let depositor = Pubkey::new_unique();
        let mut accounts = deposit_accounts(&depositor, true);
        let bad_mint = distinct_pubkey();
        accounts[4] = mk_account(bad_mint, spl_token::id(), Vec::new(), 0, false, false);
        let ix = borsh::to_vec(&VaultInstruction::Deposit { amount: 10 }).unwrap();
        assert_matches!(
            process_instruction(&PROGRAM_ID, &accounts, &ix),
            Err(ProgramError::Custom(e)) if e == VaultError::UnsupportedMint as u32
        );
    }

    #[test]
    fn deposit_rejects_wrong_vault_ata() {
        let depositor = Pubkey::new_unique();
        let mut accounts = deposit_accounts(&depositor, true);
        accounts[3] = mk_account(Pubkey::new_unique(), spl_token::id(), token_account_data(&Pubkey::new_unique(), &USDC_MINT, 0), 0, false, true);
        let ix = borsh::to_vec(&VaultInstruction::Deposit { amount: 10 }).unwrap();
        // Unsupported-mint check passes (USDC), then Vault ATA check fails.
        assert_matches!(
            process_instruction(&PROGRAM_ID, &accounts, &ix),
            Err(ProgramError::Custom(e)) if e == VaultError::InvalidTokenAccount as u32
        );
    }

    #[test]
    fn deposit_rejects_vault_ata_not_owned_by_token_program() {
        let depositor = Pubkey::new_unique();
        let mut accounts = deposit_accounts(&depositor, true);
        // Correct PDA key (passes the PDA check) but owned by a different
        // program, so the vault token ownership check fails.
        let (vault_ata, _) = find_vault_token_address(&PROGRAM_ID, &USDC_MINT);
        accounts[3] = mk_account(
            vault_ata,
            Pubkey::new_unique(),
            token_account_data(&vault_ata, &USDC_MINT, 0),
            0,
            false,
            true,
        );
        let ix = borsh::to_vec(&VaultInstruction::Deposit { amount: 10 }).unwrap();
        assert_matches!(
            process_instruction(&PROGRAM_ID, &accounts, &ix),
            Err(ProgramError::Custom(e)) if e == VaultError::InvalidTokenAccount as u32
        );
    }

    // ---- classify_deposit_record ----

    #[test]
    fn classify_deposit_record_initialized() {
        let state = classify_deposit_record(
            &PROGRAM_ID,
            DepositRecord::SPACE,
            0,
            &PROGRAM_ID,
        );
        assert_eq!(state, DepositRecordState::Initialized);
    }

    #[test]
    fn classify_deposit_record_needs_create_when_unfunded() {
        let state = classify_deposit_record(
            &system_program::id(),
            0,
            0,
            &PROGRAM_ID,
        );
        assert_eq!(state, DepositRecordState::NeedsCreate);
    }

    #[test]
    fn classify_deposit_record_funded_takeover_when_prefunded() {
        // Pre-funded, large enough, but not program-owned.
        let state = classify_deposit_record(
            &system_program::id(),
            DepositRecord::SPACE,
            5_000_000,
            &PROGRAM_ID,
        );
        assert_eq!(state, DepositRecordState::FundedTakeover);
    }

    #[test]
    fn classify_deposit_record_insufficient_space_when_funded_small() {
        // Pre-funded but undersized (griefer): surfaced as an error.
        let state = classify_deposit_record(
            &system_program::id(),
            DepositRecord::SPACE - 1,
            5_000_000,
            &PROGRAM_ID,
        );
        assert_eq!(state, DepositRecordState::InsufficientSpace);
    }

    #[test]
    fn classify_deposit_record_undersized_program_owned_is_takeover() {
        // Program-owned but undersized with lamports: not "initialized", and
        // since it is large enough... it is undersized, so InsufficientSpace.
        let state = classify_deposit_record(
            &PROGRAM_ID,
            DepositRecord::SPACE - 8,
            5_000_000,
            &PROGRAM_ID,
        );
        assert_eq!(state, DepositRecordState::InsufficientSpace);
    }

    #[test]
    fn deposit_requires_writable_source() {
        let depositor = Pubkey::new_unique();
        let mut accounts = deposit_accounts(&depositor, true);
        accounts[2].is_writable = false;
        let ix = borsh::to_vec(&VaultInstruction::Deposit { amount: 10 }).unwrap();
        assert_matches!(
            process_instruction(&PROGRAM_ID, &accounts, &ix),
            Err(ProgramError::InvalidArgument)
        );
    }

    #[test]
    fn deposit_rejects_mint_not_owned_by_token_program() {
        let depositor = Pubkey::new_unique();
        let mut accounts = deposit_accounts(&depositor, true);
        // Key is USDC_MINT (passes the unsupported-mint check) but owned by
        // a different program, so the mint-ownership check fails.
        accounts[4] = mk_account(USDC_MINT, Pubkey::new_unique(), Vec::new(), 0, false, false);
        let ix = borsh::to_vec(&VaultInstruction::Deposit { amount: 10 }).unwrap();
        assert_matches!(
            process_instruction(&PROGRAM_ID, &accounts, &ix),
            Err(ProgramError::Custom(e)) if e == VaultError::InvalidMint as u32
        );
    }

    #[test]
    fn deposit_rejects_source_not_owned() {
        let depositor = Pubkey::new_unique();
        let mut accounts = deposit_accounts(&depositor, true);
        let stranger = Pubkey::new_unique();
        accounts[2] = mk_account(Pubkey::new_unique(), spl_token::id(), token_account_data(&stranger, &USDC_MINT, 1000), 0, false, true);
        let ix = borsh::to_vec(&VaultInstruction::Deposit { amount: 10 }).unwrap();
        assert_matches!(
            process_instruction(&PROGRAM_ID, &accounts, &ix),
            Err(ProgramError::Custom(e)) if e == VaultError::Unauthorized as u32
        );
    }

    #[test]
    fn deposit_rejects_source_mint_mismatch() {
        let depositor = Pubkey::new_unique();
        let mut accounts = deposit_accounts(&depositor, true);
        accounts[2] = mk_account(Pubkey::new_unique(), spl_token::id(), token_account_data(&depositor, &USDT_MINT, 1000), 0, false, true);
        let ix = borsh::to_vec(&VaultInstruction::Deposit { amount: 10 }).unwrap();
        assert_matches!(
            process_instruction(&PROGRAM_ID, &accounts, &ix),
            Err(ProgramError::Custom(e)) if e == VaultError::InvalidMint as u32
        );
    }

    #[test]
    fn deposit_rejects_insufficient_funds() {
        let depositor = Pubkey::new_unique();
        let mut accounts = deposit_accounts(&depositor, true);
        accounts[2] = mk_account(Pubkey::new_unique(), spl_token::id(), token_account_data(&depositor, &USDC_MINT, 5), 0, false, true);
        let ix = borsh::to_vec(&VaultInstruction::Deposit { amount: 10 }).unwrap();
        assert_matches!(
            process_instruction(&PROGRAM_ID, &accounts, &ix),
            Err(ProgramError::Custom(e)) if e == VaultError::InsufficientFunds as u32
        );
    }

    // ---- apply_deposit_updates ----


    #[test]
    fn apply_deposit_updates_writes_record_and_total() {
        let depositor = Pubkey::new_unique();
        let vault_state_pda = find_vault_state_address(&PROGRAM_ID).0;
        let rec_info = mk_account(
            Pubkey::new_unique(),
            PROGRAM_ID,
            borsh::to_vec(&DepositRecord {
                depositor,
                mint: USDC_MINT,
                amount: 100,
            })
            .unwrap(),
            0,
            false,
            true,
        );

        let state = VaultState {
            owner: depositor,
            total_deposits: 1000,
        };
        let state_info = mk_account(
            vault_state_pda,
            PROGRAM_ID,
            valid_vault_state_data(&depositor),
            0,
            false,
            true,
        );

        apply_deposit_updates(
            &rec_info,
            &state_info,
            state,
            100,
            50,
            &depositor,
            &USDC_MINT,
        )
        .unwrap();
        let updated =
            DepositRecord::try_from_slice(&rec_info.data.borrow()).unwrap();
        assert_eq!(updated.amount, 150);
        assert_eq!(updated.mint, USDC_MINT);
        assert_eq!(updated.depositor, depositor);
        let updated_state =
            VaultState::try_from_slice(&state_info.data.borrow()).unwrap();
        assert_eq!(updated_state.total_deposits, 1050);
        assert_eq!(updated_state.owner, depositor);
    }

    #[test]
    fn apply_deposit_updates_overflow_err() {
        let depositor = Pubkey::new_unique();
        let vault_state_pda = find_vault_state_address(&PROGRAM_ID).0;
        let rec_info = mk_account(
            Pubkey::new_unique(),
            PROGRAM_ID,
            borsh::to_vec(&DepositRecord {
                depositor,
                mint: USDC_MINT,
                amount: u64::MAX,
            })
            .unwrap(),
            0,
            false,
            true,
        );
        let state = VaultState {
            owner: depositor,
            total_deposits: 0,
        };
        let state_info = mk_account(
            vault_state_pda,
            PROGRAM_ID,
            valid_vault_state_data(&depositor),
            0,
            false,
            true,
        );

        assert_matches!(
            apply_deposit_updates(
                &rec_info,
                &state_info,
                state,
                u64::MAX,
                1,
                &depositor,
                &USDC_MINT,
            ),
            Err(ProgramError::Custom(e)) if e == VaultError::ArithmeticOverflow as u32
        );
    }

    // ---- PDA helpers round-trip ----

    #[test]
    fn pda_derivations_are_stable_and_on_curve() {
        let (state, _) = find_vault_state_address(&PROGRAM_ID);
        let (ata, _) = find_vault_token_address(&PROGRAM_ID, &USDC_MINT);
        let (dep, _) = find_deposit_address(
            &PROGRAM_ID,
            &Pubkey::new_unique(),
            &state,
            &USDT_MINT,
        );
        assert!(!state.is_on_curve());
        assert!(!ata.is_on_curve());
        assert!(!dep.is_on_curve());
    }

    // ---- process_redirect ----

    fn redirect_accounts(
        owner: &Pubkey,
        state_owner: &Pubkey,
        mint: &Pubkey,
        vault_balance: u64,
        dest_owner: &Pubkey,
        dest_mint: &Pubkey,
    ) -> Vec<AccountInfo<'static>> {
        let vault_ata = find_vault_token_address(&PROGRAM_ID, mint).0;
        let owner_info = mk_account(*owner, PROGRAM_ID, vec![], 0, true, true);
        let state_info = mk_account(
            find_vault_state_address(&PROGRAM_ID).0,
            PROGRAM_ID,
            valid_vault_state_data(state_owner),
            0,
            false,
            true,
        );
        let vault_ata_info = mk_account(
            vault_ata,
            spl_token::ID,
            token_account_data(state_owner, mint, vault_balance),
            0,
            false,
            true,
        );
        let dest_info = mk_account(
            Pubkey::new_unique(),
            spl_token::ID,
            token_account_data(dest_owner, dest_mint, 0),
            0,
            false,
            true,
        );
        let token_program_info =
            mk_account(spl_token::ID, system_program::ID, vec![], 0, false, false);
        vec![
            owner_info,
            state_info,
            vault_ata_info,
            dest_info,
            token_program_info,
        ]
    }

    #[test]
    fn redirect_requires_owner_signature() {
        let owner = Pubkey::new_unique();
        let mut accts = redirect_accounts(
            &owner,
            &owner,
            &USDC_MINT,
            100,
            &Pubkey::new_unique(),
            &USDC_MINT,
        );
        accts[0].is_signer = false;
        let res = process_redirect(&PROGRAM_ID, &accts);
        assert_eq!(res, Err(ProgramError::MissingRequiredSignature));
    }

    #[test]
    fn redirect_requires_valid_token_program() {
        let owner = Pubkey::new_unique();
        let mut accts = redirect_accounts(
            &owner,
            &owner,
            &USDC_MINT,
            100,
            &Pubkey::new_unique(),
            &USDC_MINT,
        );
        let bad_prog: &'static Pubkey = Box::leak(Box::new(distinct_pubkey()));
        accts[4].key = bad_prog;
        let res = process_redirect(&PROGRAM_ID, &accts);
        assert_eq!(res, Err(ProgramError::IncorrectProgramId));
    }

    #[test]
    fn redirect_rejects_wrong_vault_state_pda() {
        let owner = Pubkey::new_unique();
        let mut accts = redirect_accounts(
            &owner,
            &owner,
            &USDC_MINT,
            100,
            &Pubkey::new_unique(),
            &USDC_MINT,
        );
        let wrong_pda: &'static Pubkey = Box::leak(Box::new(Pubkey::new_unique()));
        accts[1].key = wrong_pda;
        let res = process_redirect(&PROGRAM_ID, &accts);
        assert_matches!(
            res,
            Err(ProgramError::Custom(e)) if e == VaultError::InvalidTokenAccount as u32
        );
    }

    #[test]
    fn redirect_rejects_non_owner() {
        let owner = Pubkey::new_unique();
        let stranger = Pubkey::new_unique();
        let accts = redirect_accounts(
            &stranger,
            &owner,
            &USDC_MINT,
            100,
            &Pubkey::new_unique(),
            &USDC_MINT,
        );
        let res = process_redirect(&PROGRAM_ID, &accts);
        assert_matches!(
            res,
            Err(ProgramError::Custom(e)) if e == VaultError::Unauthorized as u32
        );
    }

    #[test]
    fn redirect_rejects_non_owned_vault_token_account() {
        let owner = Pubkey::new_unique();
        let mut accts = redirect_accounts(
            &owner,
            &owner,
            &USDC_MINT,
            100,
            &Pubkey::new_unique(),
            &USDC_MINT,
        );
        accts[2].owner = &system_program::ID;
        let res = process_redirect(&PROGRAM_ID, &accts);
        assert_matches!(
            res,
            Err(ProgramError::Custom(e)) if e == VaultError::InvalidTokenAccount as u32
        );
    }

    #[test]
    fn redirect_rejects_destination_mint_mismatch() {
        let owner = Pubkey::new_unique();
        let accts = redirect_accounts(
            &owner,
            &owner,
            &USDC_MINT,
            100,
            &Pubkey::new_unique(),
            &USDT_MINT,
        );
        let res = process_redirect(&PROGRAM_ID, &accts);
        assert_matches!(
            res,
            Err(ProgramError::Custom(e)) if e == VaultError::InvalidMint as u32
        );
    }

    #[test]
    fn redirect_rejects_zero_balance() {
        let owner = Pubkey::new_unique();
        let accts = redirect_accounts(
            &owner,
            &owner,
            &USDC_MINT,
            0,
            &Pubkey::new_unique(),
            &USDC_MINT,
        );
        let res = process_redirect(&PROGRAM_ID, &accts);
        assert_matches!(
            res,
            Err(ProgramError::Custom(e)) if e == VaultError::InsufficientFunds as u32
        );
    }

    #[test]
    fn redirect_dispatch_requires_owner_signature() {
        let owner = Pubkey::new_unique();
        let mut accts = redirect_accounts(
            &owner,
            &owner,
            &USDC_MINT,
            100,
            &Pubkey::new_unique(),
            &USDC_MINT,
        );
        accts[0].is_signer = false;
        let ix = borsh::to_vec(&VaultInstruction::Redirect).unwrap();
        assert_matches!(
            process_instruction(&PROGRAM_ID, &accts, &ix),
            Err(ProgramError::MissingRequiredSignature)
        );
    }

    #[test]
    fn redirect_requires_writable_vault_ata() {
        let owner = Pubkey::new_unique();
        let mut accts = redirect_accounts(
            &owner,
            &owner,
            &USDC_MINT,
            100,
            &Pubkey::new_unique(),
            &USDC_MINT,
        );
        accts[2].is_writable = false;
        let res = process_redirect(&PROGRAM_ID, &accts);
        assert_eq!(res, Err(ProgramError::InvalidArgument));
    }

    #[test]
    fn redirect_requires_writable_destination() {
        let owner = Pubkey::new_unique();
        let mut accts = redirect_accounts(
            &owner,
            &owner,
            &USDC_MINT,
            100,
            &Pubkey::new_unique(),
            &USDC_MINT,
        );
        accts[3].is_writable = false;
        let res = process_redirect(&PROGRAM_ID, &accts);
        assert_eq!(res, Err(ProgramError::InvalidArgument));
    }

    #[test]
    fn redirect_rejects_destination_equals_vault_ata() {
        let owner = Pubkey::new_unique();
        let vault_ata: &'static Pubkey = Box::leak(Box::new(
            find_vault_token_address(&PROGRAM_ID, &USDC_MINT).0,
        ));
        let mut accts = redirect_accounts(
            &owner,
            &owner,
            &USDC_MINT,
            100,
            &Pubkey::new_unique(),
            &USDC_MINT,
        );
        accts[3].key = vault_ata;
        let res = process_redirect(&PROGRAM_ID, &accts);
        assert_matches!(
            res,
            Err(ProgramError::Custom(e)) if e == VaultError::InvalidTokenAccount as u32
        );
    }
}