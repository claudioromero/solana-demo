use solana_program::{
    account_info::AccountInfo, entrypoint::ProgramResult, pubkey::Pubkey,
};

solana_program::entrypoint!(process_instruction);

/// The on-chain BPF entrypoint. This thin wrapper is registered with the
/// `solana_program::entrypoint!` macro above and simply delegates to the
/// program's main `process_instruction` in `lib.rs`.
///
/// See `crate::process_instruction` for account and error documentation.
pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    crate::process_instruction(program_id, accounts, instruction_data)
}
