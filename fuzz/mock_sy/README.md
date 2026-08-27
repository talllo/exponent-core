# mock_sy — mock Standardized Yield program (fuzzing fixture)

A plain native Solana program (no Anchor) implementing the SY CPI interface that
Exponent Core calls into, plus four test-control instructions that let a fuzz
harness move the exchange rate and the emission indexes between two user
actions.

Caller side of the interface (authoritative):
`exponent-core/programs/exponent_core/src/utils/sy_cpi.rs`.
Wire types: `exponent-core/libraries/sy_common/src/lib.rs`,
`exponent-core/libraries/precise_number/src/lib.rs`,
`exponent-core/libraries/amount_value/src/lib.rs`.

## Program id

```
5oPn327MeFdp9GVik2VwYNVyMN6ZL88rx8yKo54ViQWk
```

Keypair: `mock_sy-keypair.json` in this directory (also copied to
`target/deploy/mock_sy-keypair.json` so `solana program deploy` uses the same
id). Declared in `src/lib.rs` via `solana_program::declare_id!`.

## Build

```
cargo build-sbf          # from this directory
```

Output ELF: `<crate>/target/deploy/mock_sy.so`.

Verify the toolchain produced a real SBF object — `263` is Solana Bytecode
Format, `247` means the host toolchain built it and the artifact is unusable:

```
od -An -tu2 -j18 -N2 target/deploy/mock_sy.so
```

This crate declares its own `[workspace]` table, so it is never absorbed into a
parent workspace and its Solana dependency graph stays independent.

Toolchain this was built and tested against: `solana-cargo-build-sbf 3.1.14`,
platform-tools v1.52, `solana-cli 3.1.14`; `solana-program` 3.0,
`spl-token-interface` 2.0 (which is exactly what `spl-token` 9 re-exports).

## Test

```
cargo build-sbf && cargo test
```

`tests/svm.rs` loads the real ELF into `litesvm` and drives every
discriminator. The `.so` path is anchored to `env!("CARGO_MANIFEST_DIR")`; no
absolute path and no `../../..` chain appears anywhere in this crate.

## Instruction encoding

A **bare 1-byte discriminator**, then borsh args. There is no 8-byte Anchor
discriminator anywhere in this interface — that is why this is a native program.

| disc | name | args | return data |
|---|---|---|---|
| `1` | `mint_sy` | `u64` amount_base (LE) | `MintSyReturnData` |
| `2` | `redeem_sy` | `u64` amount_sy (LE) | `RedeemSyReturnData` |
| `3` | `init_sy_personal_account` | — | — |
| `5` | `deposit_sy` | `u64` amount (LE) | `SyState` |
| `6` | `withdraw_sy` | `u64` amount (LE) | `SyState` |
| `7` | `get_sy_state` | — | `SyState` |
| `8` | `claim_emission` | `Amount` | — |
| `10` | `get_position` | — | `PositionState` |
| `199` | `init_global` (test control) | `Number` initial rate | — |
| `200` | `set_exchange_rate` (test control) | `Number` | — |
| `201` | `set_emission_index` (test control) | `u32` index, `Number` value | — |
| `202` | `add_emission_index` (test control) | `Number` initial `[, Pubkey mint]` | — |
| `203` | `fund_emission` (test control) | `u32` index, `u64` amount | — |

`199` is not part of the Exponent interface; it exists because the singleton
global account has to be created by somebody. Call it once during harness setup.

## Borsh layouts

Anchor's `AnchorSerialize`/`AnchorDeserialize` are borsh, so these are
byte-identical to the `sy_common` / `amount_value` types.

```
Number            = [u64; 4]                        32 bytes, LE per word,
                                                    least-significant word first
                                                    fixed point, ONE = 1e12
Pubkey            = [u8; 32]                        32 raw bytes
Vec<T>            = u32 LE length, then elements
enum              = u8 variant index, then payload

SyState           { exchange_rate: Number,          32
                    emission_indexes: Vec<Number> }  4 + 32*n

PositionState     { owner: Pubkey,                  32
                    sy_balance: u64,                 8
                    emissions: Vec<Emission> }       4 + 72*n

Emission          { mint: Pubkey,                   32
                    amount_claimable: u64,           8
                    last_seen_emission_index: Number } 32

MintSyReturnData  { sy_out_amount: u64,              8
                    exchange_rate: Number }         32   -> 40 bytes total

RedeemSyReturnData{ base_out_amount: u64,            8
                    exchange_rate: Number }         32   -> 40 bytes total

Amount            = All        -> [0]
                  | Some(u64)  -> [1] ++ u64 LE      9 bytes
```

`MAX_EMISSIONS = 8`. The cap exists so a fully populated `PositionState`
(`32 + 8 + 4 + 8*72 = 620` bytes) still fits inside the 1024-byte
`set_return_data` limit.

## Economics

`exchange_rate` is **underlying (base) tokens per SY**, scaled by 1e12.
Exponent computes `py = sy * rate` (`sy_to_py`) and `sy = py / rate`
(`py_to_sy`), so SY appreciating over time means the rate **increases**.

```
mint_sy(base)  ->  sy_out   = floor(base * 1e12 / rate_raw)
redeem_sy(sy)  ->  base_out = floor(sy * rate_raw / 1e12)
```

Both legs floor, so `mint_sy(b)` followed by `redeem_sy(sy_out)` at an unchanged
rate can never return more than `b` — the rounding residue stays with the
program. This is asserted in
`tests/svm.rs::mint_then_redeem_round_trip_never_favours_the_caller` over five
rates including deliberately non-dividing ones.

`set_exchange_rate` [200] is an **absolute** assignment and explicitly permits a
**decrease**. Exponent has an emergency mode keyed on
`all_time_high_sy_exchange_rate > last_seen_sy_exchange_rate`, so a falling rate
is a real, in-scope state that the fuzzer must be able to reach.

Note that raising the rate does not create base tokens. If the harness wants a
subsequent `redeem_sy` to actually pay out more, it must also move real base
tokens into `base_custody` (that is what "yield arrived" means); otherwise the
SPL transfer fails with `insufficient funds`, which is itself a realistic state.

### Emissions

Emission streams are cumulative indexes in *emission tokens per SY*, scaled by
1e12. On every position-touching instruction (`deposit_sy`, `withdraw_sy`,
`claim_emission`, `fund_emission`) the position accrues:

```
amount_claimable += floor(sy_balance * (global_index - last_seen_index) / 1e12)
last_seen_index   = global_index
```

A stream the position has **never seen** is registered at the *current* global
index, so adding or moving a stream never pays retroactively. Practical
consequence for a harness: to make an index move accrue, the position must be
touched once while the index is still at its old value.

`fund_emission` [203] credits `amount_claimable` directly, on top of accrual.
The harness is responsible for making sure `emission_custody` actually holds the
tokens; `claim_emission` does a real SPL transfer out of it.

`claim_emission` resolves *which* stream is being claimed from the **mint of the
`emission_custody` token account**, because Exponent's CPI carries no stream
index. That mint must have been registered by passing the optional 32-byte tail
to `add_emission_index` [202]; otherwise the claim fails with
`UnknownEmissionMint` (custom error 13).

## Accounts

The harness controls which accounts are passed (Exponent's `CpiAccounts` plus
address-lookup-table indirection), so it must match these orders exactly.
`(w)` = writable, `(s)` = signer.

| disc | instruction | accounts, in order |
|---|---|---|
| `1` | `mint_sy` | `sy_global(w)`, `base_src(w)`, `base_custody(w)`, `sy_mint(w)`, `sy_dst(w)`, `user_authority(s)`, `token_program` |
| `2` | `redeem_sy` | `sy_global(w)`, `sy_src(w)`, `sy_mint(w)`, `base_custody(w)`, `base_dst(w)`, `sy_authority_pda`, `user_authority(s)`, `token_program` |
| `3` | `init_sy_personal_account` | `payer(s,w)`, `sy_position(w)`, `owner`, `system_program` |
| `5` | `deposit_sy` | `sy_global(w)`, `sy_position(w)`, `sy_src(w)`, `sy_custody(w)`, `src_authority(s)`, `token_program` |
| `6` | `withdraw_sy` | `sy_global(w)`, `sy_position(w)`, `sy_custody(w)`, `sy_dst(w)`, `sy_authority_pda`, `token_program` |
| `7` | `get_sy_state` | `sy_global` |
| `8` | `claim_emission` | `sy_global(w)`, `sy_position(w)`, `emission_custody(w)`, `emission_dst(w)`, `sy_authority_pda`, `token_program` |
| `10` | `get_position` | `sy_position` |
| `199` | `init_global` | `payer(s,w)`, `sy_global(w)`, `system_program` |
| `200` | `set_exchange_rate` | `sy_global(w)` |
| `201` | `set_emission_index` | `sy_global(w)` |
| `202` | `add_emission_index` | `sy_global(w)` |
| `203` | `fund_emission` | `sy_global(w)`, `sy_position(w)` |

### PDAs

| PDA | seeds |
|---|---|
| `sy_global` | `[b"sy_global"]` |
| `sy_position` | `[b"sy_position", owner]` |
| `sy_authority_pda` | `[b"sy_authority"]` |

Helpers: `mock_sy::sy_global_address`, `mock_sy::sy_position_address`,
`mock_sy::sy_authority_address`.

### Token account ownership the harness must set up

| account | SPL owner / authority |
|---|---|
| `sy_mint` | mint authority = **`sy_global`** PDA |
| `base_custody` | `sy_authority_pda` |
| `sy_custody` | `sy_authority_pda` |
| `emission_custody` | `sy_authority_pda` |

`sy_mint`'s authority is `sy_global` rather than `sy_authority_pda` because
`mint_sy` does not receive `sy_authority_pda` in its account list, so the global
PDA is the only program-owned signer available for the `MintTo` CPI.

Authorities that must sign at the outer/CPI level:

* `mint_sy`: `user_authority` signs the base `Transfer` in.
* `redeem_sy`: `user_authority` signs the SY `Burn`; `sy_authority_pda` signs
  the base `Transfer` out.
* `deposit_sy`: `src_authority` signs the SY `Transfer` in (this is Exponent's
  vault PDA, already a signer because Exponent uses `invoke_signed`).
* `withdraw_sy` / `claim_emission`: `sy_authority_pda` signs the `Transfer` out.

## Custom errors

`ProgramError::Custom(n)`:

| n | meaning |
|---|---|
| 0 | `UnknownDiscriminator` |
| 1 | `InvalidInstructionData` |
| 2 | `InvalidPda` |
| 3 | `AccountNotInitialized` |
| 4 | `AccountAlreadyInitialized` |
| 5 | `AccountTooSmall` |
| 6 | `SerializationFailed` |
| 7 | `DeserializationFailed` |
| 8 | `MathOverflow` |
| 9 | `NumberTooLarge` (upper two `Number` words non-zero) |
| 10 | `ZeroExchangeRate` |
| 11 | `EmissionIndexOutOfRange` |
| 12 | `TooManyEmissions` |
| 13 | `UnknownEmissionMint` |
| 14 | `InsufficientClaimable` |
| 15 | `InsufficientSyBalance` |
| 16 | `MissingSigner` |
| 17 | `WrongAccountOwner` |

Custom error `1` from `TokenkegQ...` in a log is SPL Token's `InsufficientFunds`,
not one of these.

## Layout

```
Cargo.toml            own [workspace]; pins solana-transaction-context =4.1.2
                      (litesvm 0.15.2 destructures ExecutionRecord exhaustively)
mock_sy-keypair.json  program keypair
src/lib.rs            entrypoint, dispatch, all instruction handlers
src/number.rs         Number, byte-compatible with precise_number::Number
src/wire.rs           SyState / PositionState / Emission / *ReturnData / Amount
src/state.rs          SyGlobal, SyPosition, PDA seeds, emission accrual
src/token.rs          SPL Token transfer / mint_to / burn CPI wrappers
src/error.rs          custom error codes
tests/svm.rs          litesvm end-to-end tests over the real ELF
```
