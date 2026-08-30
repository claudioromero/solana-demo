---
name: build-vault
description: Build a vault.
license: MIT
compatibility: opencode
metadata:
  audience: developers
---

## What I do

- Build s Solana Program in Rust called `Vault`. This program represents a vault.
- The vault has an owner.
- Any user can deposit tokens in the vault. The tokens accepted are: USDC and USDT. Otherwise the vault must throw an error during the deposit.
- The vault must track the total amount deposited per user and token, like a mapping (user => token => amount).  

- Create a function called `redirect`. This function must transfer the entire balance of a given token to the destination address specified.
This function can be called by the owner of the program only. This function accepts two parameters: the token and the destination address.

- Make sure all functions are well-documented.
- Create comprehensive unit tests and make sure the test coverage is greater than 90%. 
- Update the README.md file. Create it if it does not exist.
- Create a .gitignore file


## When to use me

Use this when you are building a vault.
