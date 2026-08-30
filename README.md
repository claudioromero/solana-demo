# solana-demo

A Solana native program implementing a token vault. Any user can deposit
USDC or USDT into a program-owned vault, and the vault owner can withdraw
(redirect) the full balance of a given token to a destination address.

## Features

- **Deposit**: Any user deposits a supported token (USDC/USDT) into the
  vault. Only USDC and USDT mint addresses are accepted; any other mint is
  rejected.
- **Track balances**: The program tracks the total amount deposited per user
  and per token, effectively a `(user => token => amount)` mapping. One
  `DepositRecord` account exists for each (user, token) pair and is updated on
  every deposit.
- **Redirect**: The vault owner can transfer the entire balance of a given
  token held by the vault to a destination token account.

## Important semantics

`total_deposits` and each `DepositRecord` are **cumulative lifetime**
counters. A `redirect` withdraws the vault's actual on-chain tokens but does
**not** decrement these counters, so they represent how much has been
deposited over time, not the vault's current balance.

## Architecture

The program is written as a Solana **native** (BPF) program in Rust. It does
not use a framework such as Anchor.

| Concept | Details |
| --- | --- |
| Program language | Rust (Solana native / BPF) |
| Dependencies | `solana-program`, `spl-token`, `borsh` |
| State | `VaultState` (owner + total deposits) per vault |
| Records | `DepositRecord` per (depositor, vault, mint) |
| Allowed mints | `USDC_MINT`, `USDT_MINT` (mainnet addresses) |

### Instructions

- `Initialize` – creates the vault state PDA and sets its owner.
- `Deposit { amount }` – transfers `amount` of a supported token into the
  vault and updates the deposit record and total.
- `Redirect` – the owner withdraws the vault's full token balance to a
  destination account.

### PDA derivation

- Vault state: `find_program_address(["vault", "state"])`
- Vault token account: `find_program_address(["vault", "tokens", mint])`
- Deposit record: `find_program_address(["vault", "deposit", depositor, vault, mint])`

### Errors

`VaultError` covers: invalid instruction, already/not initialized,
unauthorized, unsupported mint, invalid mint, invalid token account,
insufficient funds, and arithmetic overflow.

### Security model

- **Ownership & PDA checks**: state, vault-token, and deposit-record
  addresses are checked against their deterministic PDAs; account ownership is
  verified against the program and the SPL Token program as appropriate
  (including that the vault token account is owned by the SPL Token program on
  both `Deposit` and `Redirect`).
- **Initialization griefing defense**: accounts that are pre-funded but not
  program-owned (e.g. created by a third party at the state or deposit-record
  PDA) are adopted via the system program's `allocate` + `assign` rather than
  bricking initialization / first deposit. A state account that is genuinely
  program-owned and initialized still returns `AlreadyInitialized`, and a
  deposit record that is funded but too small to hold a record returns a clean
  error.
- **Writable checks**: every account that this program writes to is verified
  to be marked writable before any mutation, so failures surface with a clear
  error instead of a runtime write violation.
- **Owner gating**: `Initialize` and `Redirect` require the owner's signature;
  `Redirect` additionally verifies the signer matches `VaultState.owner`.
- **Supported tokens**: only USDC/USDT mints are accepted, and the mint
  account itself must be owned by the SPL Token program.
- **Destination safety**: `Redirect` rejects a destination whose mint
  mismatches the vault's token and rejects the vault's own token account as a
  destination.
- **Overflow safety**: all balance arithmetic uses checked addition.
- **Atomicity**: a deposit's token transfer and record update occur in a
  single transaction, so any failure reverts everything.

## Project layout

```
src/lib.rs        Program logic, state, PDA helpers, and unit tests
src/entrypoint.rs BPF entrypoint delegating to lib.rs
```

## Getting started

```bash
# Build the program
cargo build

# Run the unit tests
cargo test
```

Coverage (if `cargo-llvm-cov` is installed):

```bash
cargo llvm-cov --lib
```

## Notes

- The project's unit tests run natively; SPL Token CPIs (transfers, account
  creation) execute on-chain and cannot run in these native unit tests.
