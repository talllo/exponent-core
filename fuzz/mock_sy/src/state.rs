//! On-chain account state owned by the mock SY program.
//!
//! These layouts are private to the mock (Exponent never deserializes them —
//! it only ever reads the `wire` types out of return data), so they are plain
//! borsh with a 1-byte account-kind tag at offset 0 to distinguish an
//! initialized account from a zeroed one.

use crate::{
    error::MockSyError,
    number::Number,
    wire::{Emission, PositionState, SyState},
};
use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{account_info::AccountInfo, program_error::ProgramError, pubkey::Pubkey};

/// PDA seed for the singleton global account.
pub const GLOBAL_SEED: &[u8] = b"sy_global";
/// PDA seed prefix for a per-owner position account.
pub const POSITION_SEED: &[u8] = b"sy_position";
/// PDA seed for the token-custody / mint authority.
pub const AUTHORITY_SEED: &[u8] = b"sy_authority";

pub const GLOBAL_TAG: u8 = 1;
pub const POSITION_TAG: u8 = 2;

/// Hard cap on emission streams.
///
/// Chosen so that a fully populated `PositionState` still fits inside the
/// 1024-byte `set_return_data` limit:
/// `32 + 8 + 4 + 8 * (32 + 8 + 32) = 620` bytes.
pub const MAX_EMISSIONS: usize = 8;

pub const GLOBAL_ACCOUNT_SIZE: usize = 1024;
pub const POSITION_ACCOUNT_SIZE: usize = 1024;

/// Singleton global account, PDA `["sy_global"]`.
///
/// It is also the **mint authority of the SY mint** — `mint_sy` does not
/// receive `sy_authority_pda` in its account list, so the global PDA signs the
/// `MintTo` CPI.
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, Default)]
pub struct SyGlobal {
    /// Underlying (base) tokens per 1 SY, scaled by 1e12.
    pub exchange_rate: Number,
    /// Mint of emission stream `i`. Parallel to `emission_indexes`.
    pub emission_mints: Vec<Pubkey>,
    /// Cumulative emission index of stream `i`, in emission tokens per SY,
    /// scaled by 1e12.
    pub emission_indexes: Vec<Number>,
    /// TEST CONTROL, set by `[205] arm_reentrancy`. When `Some`, `get_sy_state`
    /// invokes this program before returning.
    ///
    /// Exponent CPIs into `get_sy_state` from the middle of `update_vault_yield`
    /// — after the vault has been deserialized, before the handler's mutations
    /// are written back. If the callee can call back into Exponent in that
    /// window, an inner instruction's write would be silently overwritten by
    /// the outer one's stale in-memory copy. This field is what lets the
    /// harness ask whether that is reachable at all.
    ///
    /// Appended LAST, so the borsh layout of every field before it is unchanged.
    pub reenter_target: Option<Pubkey>,
}

impl SyGlobal {
    pub fn to_sy_state(&self) -> SyState {
        SyState {
            exchange_rate: self.exchange_rate,
            emission_indexes: self.emission_indexes.clone(),
        }
    }
}

/// Per-owner position, PDA `["sy_position", owner]`.
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug)]
pub struct SyPosition {
    pub owner: Pubkey,
    pub sy_balance: u64,
    pub emissions: Vec<Emission>,
}

impl SyPosition {
    pub fn to_position_state(&self) -> PositionState {
        PositionState {
            owner: self.owner,
            sy_balance: self.sy_balance,
            emissions: self.emissions.clone(),
        }
    }

    /// Extend the position's emission list to match the global one and accrue
    /// `sy_balance * (global_index - last_seen_index)` into `amount_claimable`.
    ///
    /// New streams start at the *current* global index so that adding a stream
    /// never retroactively pays out.
    pub fn sync_and_accrue(&mut self, global: &SyGlobal) -> Result<(), ProgramError> {
        while self.emissions.len() < global.emission_indexes.len() {
            let i = self.emissions.len();
            self.emissions.push(Emission {
                mint: global.emission_mints[i],
                amount_claimable: 0,
                last_seen_emission_index: global.emission_indexes[i],
            });
        }

        let balance = self.sy_balance as u128;
        for (i, e) in self.emissions.iter_mut().enumerate() {
            let Some(global_index) = global.emission_indexes.get(i) else {
                break;
            };
            let g = global_index
                .checked_raw_u128()
                .ok_or(MockSyError::NumberTooLarge)?;
            let seen = e
                .last_seen_emission_index
                .checked_raw_u128()
                .ok_or(MockSyError::NumberTooLarge)?;
            if g > seen {
                let accrued = balance
                    .checked_mul(g - seen)
                    .ok_or(MockSyError::MathOverflow)?
                    / Number::DENOM;
                let accrued = u64::try_from(accrued).map_err(|_| MockSyError::MathOverflow)?;
                e.amount_claimable = e
                    .amount_claimable
                    .checked_add(accrued)
                    .ok_or(MockSyError::MathOverflow)?;
            }
            e.last_seen_emission_index = *global_index;
        }
        Ok(())
    }
}

/// Write `value` at offset 1, tag at offset 0, zero-fill the remainder.
pub fn store<T: BorshSerialize>(account: &AccountInfo, tag: u8, value: &T) -> Result<(), ProgramError> {
    let bytes = borsh::to_vec(value).map_err(|_| MockSyError::SerializationFailed)?;
    let mut data = account.try_borrow_mut_data()?;
    if bytes.len() + 1 > data.len() {
        return Err(MockSyError::AccountTooSmall.into());
    }
    data[0] = tag;
    data[1..1 + bytes.len()].copy_from_slice(&bytes);
    for b in data[1 + bytes.len()..].iter_mut() {
        *b = 0;
    }
    Ok(())
}

/// Read a tagged account. Trailing zero padding is ignored.
pub fn load<T: BorshDeserialize>(account: &AccountInfo, tag: u8) -> Result<T, ProgramError> {
    let data = account.try_borrow_data()?;
    if data.is_empty() || data[0] != tag {
        return Err(MockSyError::AccountNotInitialized.into());
    }
    let mut slice: &[u8] = &data[1..];
    T::deserialize(&mut slice).map_err(|_| MockSyError::DeserializationFailed.into())
}
