//! Wire types shared with Exponent Core.
//!
//! These are byte-for-byte re-implementations of `sy_common`'s types and
//! `amount_value::Amount`. Anchor's `AnchorSerialize`/`AnchorDeserialize` are
//! borsh, so plain borsh derives produce identical bytes:
//!
//! * `u64` -> 8 bytes LE
//! * `Pubkey` -> 32 raw bytes
//! * `Vec<T>` -> `u32` LE length, then the elements
//! * `[u64; 4]` -> 4 x 8 bytes LE, no length prefix
//! * fieldless/tuple enum -> `u8` variant index, then the payload

use crate::number::Number;
use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::pubkey::Pubkey;

/// Returned by `get_sy_state` [7], `deposit_sy` [5] and `withdraw_sy` [6].
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct SyState {
    pub exchange_rate: Number,
    pub emission_indexes: Vec<Number>,
}

/// Returned by `get_position` [10].
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct PositionState {
    pub owner: Pubkey,
    pub sy_balance: u64,
    pub emissions: Vec<Emission>,
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Emission {
    pub mint: Pubkey,
    pub amount_claimable: u64,
    pub last_seen_emission_index: Number,
}

/// Returned by `mint_sy` [1].
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct MintSyReturnData {
    pub sy_out_amount: u64,
    pub exchange_rate: Number,
}

/// Returned by `redeem_sy` [2].
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct RedeemSyReturnData {
    pub base_out_amount: u64,
    pub exchange_rate: Number,
}

/// Argument of `claim_emission` [8]. Matches `amount_value::Amount`.
#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Amount {
    /// tag `0`, no payload
    All,
    /// tag `1`, then `u64` LE
    Some(u64),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amount_encoding() {
        assert_eq!(borsh::to_vec(&Amount::All).unwrap(), vec![0u8]);
        let some = borsh::to_vec(&Amount::Some(7)).unwrap();
        assert_eq!(some[0], 1);
        assert_eq!(&some[1..], &7u64.to_le_bytes());
        assert_eq!(some.len(), 9);
    }

    #[test]
    fn sy_state_encoding() {
        let s = SyState {
            exchange_rate: Number::ONE,
            emission_indexes: vec![Number::ONE, Number::ZERO],
        };
        let bytes = borsh::to_vec(&s).unwrap();
        // 32 (rate) + 4 (vec len) + 2 * 32
        assert_eq!(bytes.len(), 32 + 4 + 64);
        assert_eq!(&bytes[32..36], &2u32.to_le_bytes());
    }

    #[test]
    fn position_state_encoding() {
        let p = PositionState {
            owner: Pubkey::new_from_array([9u8; 32]),
            sy_balance: 5,
            emissions: vec![Emission {
                mint: Pubkey::new_from_array([3u8; 32]),
                amount_claimable: 11,
                last_seen_emission_index: Number::ONE,
            }],
        };
        let bytes = borsh::to_vec(&p).unwrap();
        // 32 (owner) + 8 (balance) + 4 (vec len) + 1 * (32 + 8 + 32)
        assert_eq!(bytes.len(), 32 + 8 + 4 + 72);
    }
}
