//! End-to-end tests: the real SBF ELF is loaded into LiteSVM and every
//! discriminator is exercised against observable state (return data, SPL token
//! balances, mint supply, position balance).
//!
//! Run `cargo build-sbf` first — the tests load
//! `$CARGO_MANIFEST_DIR/target/deploy/mock_sy.so`.

use litesvm::{types::TransactionMetadata, LiteSVM};
use mock_sy::{
    number::Number,
    state::{AUTHORITY_SEED, GLOBAL_SEED, POSITION_SEED},
    wire::{Amount, MintSyReturnData, PositionState, RedeemSyReturnData, SyState},
};
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_program::{program_pack::Pack, pubkey::Pubkey};
use solana_signer::Signer;
use solana_transaction::Transaction;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Paths and ids
// ---------------------------------------------------------------------------

/// Anchored to the crate directory, never to an absolute path or a `../..`
/// chain that would depend on the current working directory.
fn program_so_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("deploy")
        .join("mock_sy.so")
}

fn token_program_id() -> Pubkey {
    spl_token_interface::id()
}

// ---------------------------------------------------------------------------
// Instruction data builders
// ---------------------------------------------------------------------------

fn data_u64(disc: u8, v: u64) -> Vec<u8> {
    let mut d = vec![disc];
    d.extend_from_slice(&v.to_le_bytes());
    d
}

fn number_bytes(n: Number) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, w) in n.0.iter().enumerate() {
        out[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    out
}

fn data_number(disc: u8, n: Number) -> Vec<u8> {
    let mut d = vec![disc];
    d.extend_from_slice(&number_bytes(n));
    d
}

// ---------------------------------------------------------------------------
// Test fixture
// ---------------------------------------------------------------------------

struct Fixture {
    svm: LiteSVM,
    payer: Keypair,
    user: Keypair,
    program_id: Pubkey,
    sy_global: Pubkey,
    sy_authority: Pubkey,
    sy_position: Pubkey,
    base_mint: Pubkey,
    sy_mint: Pubkey,
    emission_mint: Pubkey,
    user_base: Pubkey,
    base_custody: Pubkey,
    user_sy: Pubkey,
    sy_custody: Pubkey,
    emission_custody: Pubkey,
    emission_dst: Pubkey,
}

impl Fixture {
    fn new(initial_rate: Number) -> Self {
        let so = program_so_path();
        assert!(
            so.exists(),
            "missing {}; run `cargo build-sbf` in the crate root first",
            so.display()
        );

        let mut svm = LiteSVM::new();
        let program_id = mock_sy::ID;
        svm.add_program_from_file(program_id, &so)
            .expect("failed to load mock_sy.so");

        let payer = Keypair::new();
        let user = Keypair::new();
        svm.airdrop(&payer.pubkey(), 1_000 * 1_000_000_000).unwrap();
        svm.airdrop(&user.pubkey(), 1_000 * 1_000_000_000).unwrap();

        let (sy_global, _) = Pubkey::find_program_address(&[GLOBAL_SEED], &program_id);
        let (sy_authority, _) = Pubkey::find_program_address(&[AUTHORITY_SEED], &program_id);
        let (sy_position, _) =
            Pubkey::find_program_address(&[POSITION_SEED, user.pubkey().as_ref()], &program_id);

        let mut f = Fixture {
            svm,
            payer,
            user,
            program_id,
            sy_global,
            sy_authority,
            sy_position,
            base_mint: Pubkey::default(),
            sy_mint: Pubkey::default(),
            emission_mint: Pubkey::default(),
            user_base: Pubkey::default(),
            base_custody: Pubkey::default(),
            user_sy: Pubkey::default(),
            sy_custody: Pubkey::default(),
            emission_custody: Pubkey::default(),
            emission_dst: Pubkey::default(),
        };

        // The SY mint authority is the sy_global PDA (see README: `mint_sy`
        // does not receive `sy_authority_pda`).
        let payer_pk = f.payer.pubkey();
        f.base_mint = f.create_mint(payer_pk);
        f.sy_mint = f.create_mint(sy_global);
        f.emission_mint = f.create_mint(payer_pk);

        let user_pk = f.user.pubkey();
        f.user_base = f.create_token_account(f.base_mint, user_pk);
        f.base_custody = f.create_token_account(f.base_mint, sy_authority);
        f.user_sy = f.create_token_account(f.sy_mint, user_pk);
        f.sy_custody = f.create_token_account(f.sy_mint, sy_authority);
        f.emission_custody = f.create_token_account(f.emission_mint, sy_authority);
        f.emission_dst = f.create_token_account(f.emission_mint, user_pk);

        // Fund the user with base tokens and stock the emission custody.
        f.mint_tokens_with_payer_authority(f.base_mint, f.user_base, 1_000_000_000);
        f.mint_tokens_with_payer_authority(f.emission_mint, f.emission_custody, 1_000_000_000);

        // [199] init_global
        f.send(
            vec![Instruction {
                program_id,
                accounts: vec![
                    AccountMeta::new(payer_pk, true),
                    AccountMeta::new(sy_global, false),
                    AccountMeta::new_readonly(solana_system_interface::program::ID, false),
                ],
                data: data_number(199, initial_rate),
            }],
            &[],
        );

        f
    }

    // -- plumbing ---------------------------------------------------------

    fn send(&mut self, ixs: Vec<Instruction>, extra_signers: &[&Keypair]) -> TransactionMetadata {
        self.try_send(ixs, extra_signers)
            .unwrap_or_else(|e| panic!("transaction failed: {e:?}"))
    }

    fn try_send(
        &mut self,
        ixs: Vec<Instruction>,
        extra_signers: &[&Keypair],
    ) -> Result<TransactionMetadata, litesvm::types::FailedTransactionMetadata> {
        self.svm.expire_blockhash();
        let mut signers: Vec<&Keypair> = vec![&self.payer];
        signers.extend_from_slice(extra_signers);
        let tx = Transaction::new_signed_with_payer(
            &ixs,
            Some(&self.payer.pubkey()),
            &signers,
            self.svm.latest_blockhash(),
        );
        self.svm.send_transaction(tx)
    }

    fn create_mint(&mut self, authority: Pubkey) -> Pubkey {
        let kp = Keypair::new();
        let len = spl_token_interface::state::Mint::LEN;
        let lamports = self.svm.minimum_balance_for_rent_exemption(len);
        let payer_pk = self.payer.pubkey();
        let ixs = vec![
            solana_system_interface::instruction::create_account(
                &payer_pk,
                &kp.pubkey(),
                lamports,
                len as u64,
                &token_program_id(),
            ),
            spl_token_interface::instruction::initialize_mint(
                &token_program_id(),
                &kp.pubkey(),
                &authority,
                None,
                6,
            )
            .unwrap(),
        ];
        self.send(ixs, &[&kp]);
        kp.pubkey()
    }

    fn create_token_account(&mut self, mint: Pubkey, owner: Pubkey) -> Pubkey {
        let kp = Keypair::new();
        let len = spl_token_interface::state::Account::LEN;
        let lamports = self.svm.minimum_balance_for_rent_exemption(len);
        let payer_pk = self.payer.pubkey();
        let ixs = vec![
            solana_system_interface::instruction::create_account(
                &payer_pk,
                &kp.pubkey(),
                lamports,
                len as u64,
                &token_program_id(),
            ),
            spl_token_interface::instruction::initialize_account(
                &token_program_id(),
                &kp.pubkey(),
                &mint,
                &owner,
            )
            .unwrap(),
        ];
        self.send(ixs, &[&kp]);
        kp.pubkey()
    }

    fn mint_tokens_with_payer_authority(&mut self, mint: Pubkey, dst: Pubkey, amount: u64) {
        let payer_pk = self.payer.pubkey();
        let ix = spl_token_interface::instruction::mint_to(
            &token_program_id(),
            &mint,
            &dst,
            &payer_pk,
            &[],
            amount,
        )
        .unwrap();
        self.send(vec![ix], &[]);
    }

    fn token_balance(&self, account: Pubkey) -> u64 {
        let acc = self.svm.get_account(&account).expect("token account");
        spl_token_interface::state::Account::unpack(&acc.data)
            .expect("unpack token account")
            .amount
    }

    fn mint_supply(&self, mint: Pubkey) -> u64 {
        let acc = self.svm.get_account(&mint).expect("mint account");
        spl_token_interface::state::Mint::unpack(&acc.data)
            .expect("unpack mint")
            .supply
    }

    // -- protocol instructions -------------------------------------------

    /// `[7] get_sy_state` — accounts: `[sy_global]`
    fn get_sy_state(&mut self) -> SyState {
        let ix = Instruction {
            program_id: self.program_id,
            accounts: vec![AccountMeta::new_readonly(self.sy_global, false)],
            data: vec![7],
        };
        let meta = self.send(vec![ix], &[]);
        borsh::from_slice(&meta.return_data.data).expect("decode SyState")
    }

    /// `[10] get_position` — accounts: `[sy_position]`
    fn get_position(&mut self) -> PositionState {
        let ix = Instruction {
            program_id: self.program_id,
            accounts: vec![AccountMeta::new_readonly(self.sy_position, false)],
            data: vec![10],
        };
        let meta = self.send(vec![ix], &[]);
        borsh::from_slice(&meta.return_data.data).expect("decode PositionState")
    }

    /// `[3] init_sy_personal_account`
    /// accounts: `[payer(s,w), sy_position(w), owner, system_program]`
    fn init_sy_personal_account(&mut self) {
        let payer_pk = self.payer.pubkey();
        let owner = self.user.pubkey();
        let ix = Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new(payer_pk, true),
                AccountMeta::new(self.sy_position, false),
                AccountMeta::new_readonly(owner, false),
                AccountMeta::new_readonly(solana_system_interface::program::ID, false),
            ],
            data: vec![3],
        };
        self.send(vec![ix], &[]);
    }

    /// `[1] mint_sy`
    /// accounts: `[sy_global(w), base_src(w), base_custody(w), sy_mint(w),
    ///             sy_dst(w), user_authority(s), token_program]`
    fn mint_sy(&mut self, amount_base: u64) -> MintSyReturnData {
        let user = self.user.insecure_clone();
        let ix = Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new(self.sy_global, false),
                AccountMeta::new(self.user_base, false),
                AccountMeta::new(self.base_custody, false),
                AccountMeta::new(self.sy_mint, false),
                AccountMeta::new(self.user_sy, false),
                AccountMeta::new_readonly(user.pubkey(), true),
                AccountMeta::new_readonly(token_program_id(), false),
            ],
            data: data_u64(1, amount_base),
        };
        let meta = self.send(vec![ix], &[&user]);
        borsh::from_slice(&meta.return_data.data).expect("decode MintSyReturnData")
    }

    /// `[2] redeem_sy`
    /// accounts: `[sy_global(w), sy_src(w), sy_mint(w), base_custody(w),
    ///             base_dst(w), sy_authority_pda, user_authority(s),
    ///             token_program]`
    fn redeem_sy(&mut self, amount_sy: u64) -> RedeemSyReturnData {
        let user = self.user.insecure_clone();
        let ix = Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new(self.sy_global, false),
                AccountMeta::new(self.user_sy, false),
                AccountMeta::new(self.sy_mint, false),
                AccountMeta::new(self.base_custody, false),
                AccountMeta::new(self.user_base, false),
                AccountMeta::new_readonly(self.sy_authority, false),
                AccountMeta::new_readonly(user.pubkey(), true),
                AccountMeta::new_readonly(token_program_id(), false),
            ],
            data: data_u64(2, amount_sy),
        };
        let meta = self.send(vec![ix], &[&user]);
        borsh::from_slice(&meta.return_data.data).expect("decode RedeemSyReturnData")
    }

    /// `[5] deposit_sy`
    /// accounts: `[sy_global(w), sy_position(w), sy_src(w), sy_custody(w),
    ///             src_authority(s), token_program]`
    fn deposit_sy(&mut self, amount: u64) -> SyState {
        let user = self.user.insecure_clone();
        let ix = Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new(self.sy_global, false),
                AccountMeta::new(self.sy_position, false),
                AccountMeta::new(self.user_sy, false),
                AccountMeta::new(self.sy_custody, false),
                AccountMeta::new_readonly(user.pubkey(), true),
                AccountMeta::new_readonly(token_program_id(), false),
            ],
            data: data_u64(5, amount),
        };
        let meta = self.send(vec![ix], &[&user]);
        borsh::from_slice(&meta.return_data.data).expect("decode SyState")
    }

    /// `[6] withdraw_sy`
    /// accounts: `[sy_global(w), sy_position(w), sy_custody(w), sy_dst(w),
    ///             sy_authority_pda, token_program]`
    fn withdraw_sy(&mut self, amount: u64) -> SyState {
        let ix = Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new(self.sy_global, false),
                AccountMeta::new(self.sy_position, false),
                AccountMeta::new(self.sy_custody, false),
                AccountMeta::new(self.user_sy, false),
                AccountMeta::new_readonly(self.sy_authority, false),
                AccountMeta::new_readonly(token_program_id(), false),
            ],
            data: data_u64(6, amount),
        };
        let meta = self.send(vec![ix], &[]);
        borsh::from_slice(&meta.return_data.data).expect("decode SyState")
    }

    /// `[8] claim_emission`
    /// accounts: `[sy_global(w), sy_position(w), emission_custody(w),
    ///             emission_dst(w), sy_authority_pda, token_program]`
    fn claim_emission(&mut self, amount: Amount) {
        let mut data = vec![8u8];
        data.extend_from_slice(&borsh::to_vec(&amount).unwrap());
        let ix = Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new(self.sy_global, false),
                AccountMeta::new(self.sy_position, false),
                AccountMeta::new(self.emission_custody, false),
                AccountMeta::new(self.emission_dst, false),
                AccountMeta::new_readonly(self.sy_authority, false),
                AccountMeta::new_readonly(token_program_id(), false),
            ],
            data,
        };
        self.send(vec![ix], &[]);
    }

    // -- test control -----------------------------------------------------

    /// `[200] set_exchange_rate` — accounts: `[sy_global(w)]`
    fn set_exchange_rate(&mut self, rate: Number) {
        let ix = Instruction {
            program_id: self.program_id,
            accounts: vec![AccountMeta::new(self.sy_global, false)],
            data: data_number(200, rate),
        };
        self.send(vec![ix], &[]);
    }

    /// `[201] set_emission_index` — accounts: `[sy_global(w)]`
    fn set_emission_index(&mut self, index: u32, value: Number) {
        let mut data = vec![201u8];
        data.extend_from_slice(&index.to_le_bytes());
        data.extend_from_slice(&number_bytes(value));
        let ix = Instruction {
            program_id: self.program_id,
            accounts: vec![AccountMeta::new(self.sy_global, false)],
            data,
        };
        self.send(vec![ix], &[]);
    }

    /// `[202] add_emission_index` — accounts: `[sy_global(w)]`
    fn add_emission_index(&mut self, initial: Number, mint: Option<Pubkey>) {
        let mut data = vec![202u8];
        data.extend_from_slice(&number_bytes(initial));
        if let Some(m) = mint {
            data.extend_from_slice(m.as_ref());
        }
        let ix = Instruction {
            program_id: self.program_id,
            accounts: vec![AccountMeta::new(self.sy_global, false)],
            data,
        };
        self.send(vec![ix], &[]);
    }

    /// `[203] fund_emission` — accounts: `[sy_global(w), sy_position(w)]`
    fn fund_emission(&mut self, index: u32, amount: u64) {
        let mut data = vec![203u8];
        data.extend_from_slice(&index.to_le_bytes());
        data.extend_from_slice(&amount.to_le_bytes());
        let ix = Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new(self.sy_global, false),
                AccountMeta::new(self.sy_position, false),
            ],
            data,
        };
        self.send(vec![ix], &[]);
    }
}

fn rate(raw: u128) -> Number {
    Number::from_raw_u128(raw)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// [199] + [7]: the global comes up with the rate we asked for and no streams.
#[test]
fn init_global_and_get_sy_state() {
    let mut f = Fixture::new(Number::ONE);
    let state = f.get_sy_state();
    assert_eq!(state.exchange_rate, Number::ONE);
    assert!(state.emission_indexes.is_empty());
}

/// [3] + [10]: a fresh position is owned by `owner` with a zero balance.
#[test]
fn init_personal_account_and_get_position() {
    let mut f = Fixture::new(Number::ONE);
    f.init_sy_personal_account();

    let p = f.get_position();
    assert_eq!(p.owner, f.user.pubkey());
    assert_eq!(p.sy_balance, 0);
    assert!(p.emissions.is_empty());

    // Calling it twice must be a no-op, not a failure: Exponent may re-init.
    f.init_sy_personal_account();
    assert_eq!(f.get_position().sy_balance, 0);
}

/// [1]: `sy_out = floor(base * 1e12 / rate)`; base moves into custody and SY is
/// really minted (supply grows).
#[test]
fn mint_sy_moves_tokens_and_returns_rate() {
    // rate = 1.5 base per SY
    let r = rate(1_500_000_000_000);
    let mut f = Fixture::new(r);

    let base_before = f.token_balance(f.user_base);
    let custody_before = f.token_balance(f.base_custody);
    let sy_before = f.token_balance(f.user_sy);
    let supply_before = f.mint_supply(f.sy_mint);

    let out = f.mint_sy(3_000_000);

    assert_eq!(out.sy_out_amount, 2_000_000, "3_000_000 base / 1.5");
    assert_eq!(out.exchange_rate, r);
    assert_eq!(f.token_balance(f.user_base), base_before - 3_000_000);
    assert_eq!(f.token_balance(f.base_custody), custody_before + 3_000_000);
    assert_eq!(f.token_balance(f.user_sy), sy_before + 2_000_000);
    assert_eq!(f.mint_supply(f.sy_mint), supply_before + 2_000_000);
}

/// [2]: `base_out = floor(sy * rate / 1e12)`; SY is really burned.
#[test]
fn redeem_sy_moves_tokens_and_burns() {
    let r = rate(1_500_000_000_000);
    let mut f = Fixture::new(r);
    f.mint_sy(3_000_000);

    let base_before = f.token_balance(f.user_base);
    let custody_before = f.token_balance(f.base_custody);
    let supply_before = f.mint_supply(f.sy_mint);

    let out = f.redeem_sy(2_000_000);

    assert_eq!(out.base_out_amount, 3_000_000);
    assert_eq!(out.exchange_rate, r);
    assert_eq!(f.token_balance(f.user_base), base_before + 3_000_000);
    assert_eq!(f.token_balance(f.base_custody), custody_before - 3_000_000);
    assert_eq!(f.mint_supply(f.sy_mint), supply_before - 2_000_000);
    assert_eq!(f.token_balance(f.user_sy), 0);
}

/// The round trip must never pay out more base than went in, at any rate.
/// Both legs floor, so the residue stays with the program.
#[test]
fn mint_then_redeem_round_trip_never_favours_the_caller() {
    // A rate that divides nothing evenly: 1.333333333333
    for raw in [
        1_000_000_000_000u128,
        1_333_333_333_333,
        999_999_999_999,
        3_141_592_653_589,
        1,
    ] {
        let mut f = Fixture::new(rate(raw));
        let base_in = 1_000_000u64;

        let base_before = f.token_balance(f.user_base);
        let minted = f.mint_sy(base_in);
        let redeemed = f.redeem_sy(minted.sy_out_amount);
        let base_after = f.token_balance(f.user_base);

        assert!(
            redeemed.base_out_amount <= base_in,
            "rate raw {raw}: round trip returned {} for {} in",
            redeemed.base_out_amount,
            base_in
        );
        assert!(
            base_after <= base_before,
            "rate raw {raw}: user base balance grew across a round trip"
        );
        // And SY is fully unwound.
        assert_eq!(f.token_balance(f.user_sy), 0, "rate raw {raw}");
    }
}

/// [5] + [6]: SY moves in and out of custody and `sy_balance` tracks it.
#[test]
fn deposit_and_withdraw_track_sy_balance() {
    let r = rate(1_000_000_000_000);
    let mut f = Fixture::new(r);
    f.init_sy_personal_account();
    let minted = f.mint_sy(5_000_000);
    assert_eq!(minted.sy_out_amount, 5_000_000);

    let user_sy_before = f.token_balance(f.user_sy);

    let state = f.deposit_sy(2_000_000);
    assert_eq!(state.exchange_rate, r);
    assert_eq!(f.token_balance(f.user_sy), user_sy_before - 2_000_000);
    assert_eq!(f.token_balance(f.sy_custody), 2_000_000);
    assert_eq!(f.get_position().sy_balance, 2_000_000);

    f.deposit_sy(1_000_000);
    assert_eq!(f.get_position().sy_balance, 3_000_000);
    assert_eq!(f.token_balance(f.sy_custody), 3_000_000);

    let state = f.withdraw_sy(1_500_000);
    assert_eq!(state.exchange_rate, r);
    assert_eq!(f.get_position().sy_balance, 1_500_000);
    assert_eq!(f.token_balance(f.sy_custody), 1_500_000);
    assert_eq!(f.token_balance(f.user_sy), user_sy_before - 1_500_000);

    // Over-withdrawing is rejected (custom error 15, InsufficientSyBalance).
    let ix_err = f.try_send(
        vec![Instruction {
            program_id: f.program_id,
            accounts: vec![
                AccountMeta::new(f.sy_global, false),
                AccountMeta::new(f.sy_position, false),
                AccountMeta::new(f.sy_custody, false),
                AccountMeta::new(f.user_sy, false),
                AccountMeta::new_readonly(f.sy_authority, false),
                AccountMeta::new_readonly(token_program_id(), false),
            ],
            data: data_u64(6, 9_999_999),
        }],
        &[],
    );
    assert!(ix_err.is_err(), "over-withdraw should fail");
    assert_eq!(f.get_position().sy_balance, 1_500_000);
}

/// [200]: the rate is set absolutely and `get_sy_state` reflects it, in both
/// directions. A falling rate is Exponent's emergency-mode trigger, so it must
/// be reachable.
#[test]
fn set_exchange_rate_is_observable_and_may_decrease() {
    let mut f = Fixture::new(Number::ONE);

    let r1 = rate(1_200_000_000_000);
    f.set_exchange_rate(r1);
    let s1 = f.get_sy_state();
    assert_eq!(s1.exchange_rate, r1);

    let r2 = rate(900_000_000_000); // strictly lower than r1
    f.set_exchange_rate(r2);
    let s2 = f.get_sy_state();
    assert_eq!(s2.exchange_rate, r2);

    assert_ne!(s1.exchange_rate, s2.exchange_rate);
    assert!(
        s2.exchange_rate < s1.exchange_rate,
        "the mock must allow the exchange rate to decrease"
    );

    // And the moved rate actually changes the economics.
    let minted_at_r2 = f.mint_sy(900_000);
    assert_eq!(minted_at_r2.exchange_rate, r2);
    assert_eq!(minted_at_r2.sy_out_amount, 1_000_000); // 900_000 / 0.9
}

/// [202] + [201] + [203] + [8]: emission streams can be created, moved,
/// accrued, topped up and claimed.
#[test]
fn emission_indexes_accrue_and_claim() {
    let mut f = Fixture::new(Number::ONE);
    f.init_sy_personal_account();
    f.mint_sy(4_000_000);
    f.deposit_sy(1_000_000);

    // [202] add a stream, registered against the emission mint.
    let emission_mint = f.emission_mint;
    f.add_emission_index(Number::ZERO, Some(emission_mint));
    let state = f.get_sy_state();
    assert_eq!(state.emission_indexes.len(), 1);
    assert_eq!(state.emission_indexes[0], Number::ZERO);

    // A second, unregistered stream, to prove `Vec<Number>` round-trips.
    f.add_emission_index(rate(5), None);
    assert_eq!(f.get_sy_state().emission_indexes.len(), 2);

    // Touch the position so it registers both streams at their current values.
    // A stream the position has never seen starts at the live index, so this is
    // what makes the *subsequent* index move accrue rather than being swallowed
    // as "already seen".
    f.deposit_sy(0);
    let p = f.get_position();
    assert_eq!(p.emissions.len(), 2);
    assert_eq!(p.emissions[0].amount_claimable, 0);
    assert_eq!(p.emissions[0].last_seen_emission_index, Number::ZERO);

    // [201] move the index to 2.0 emission tokens per SY.
    let idx = rate(2_000_000_000_000);
    f.set_emission_index(0, idx);
    let state = f.get_sy_state();
    assert_eq!(state.emission_indexes[0], idx);
    assert_eq!(state.emission_indexes[1], rate(5));

    // Touching the position accrues sy_balance * delta_index.
    f.deposit_sy(0);
    let p = f.get_position();
    assert_eq!(p.emissions.len(), 2);
    assert_eq!(p.emissions[0].mint, emission_mint);
    assert_eq!(p.emissions[0].amount_claimable, 2_000_000);
    assert_eq!(p.emissions[0].last_seen_emission_index, idx);
    // Stream 1 never moved, so it pays nothing.
    assert_eq!(p.emissions[1].amount_claimable, 0);

    // [203] fund_emission tops up on top of accrual.
    f.fund_emission(0, 500_000);
    assert_eq!(f.get_position().emissions[0].amount_claimable, 2_500_000);

    // [8] claim a partial amount.
    let dst_before = f.token_balance(f.emission_dst);
    let custody_before = f.token_balance(f.emission_custody);
    f.claim_emission(Amount::Some(1_000_000));
    assert_eq!(f.token_balance(f.emission_dst), dst_before + 1_000_000);
    assert_eq!(
        f.token_balance(f.emission_custody),
        custody_before - 1_000_000
    );
    assert_eq!(f.get_position().emissions[0].amount_claimable, 1_500_000);

    // [8] claim the rest with Amount::All.
    f.claim_emission(Amount::All);
    assert_eq!(f.token_balance(f.emission_dst), dst_before + 2_500_000);
    assert_eq!(f.get_position().emissions[0].amount_claimable, 0);

    // Over-claiming is rejected.
    let mut data = vec![8u8];
    data.extend_from_slice(&borsh::to_vec(&Amount::Some(1)).unwrap());
    let res = f.try_send(
        vec![Instruction {
            program_id: f.program_id,
            accounts: vec![
                AccountMeta::new(f.sy_global, false),
                AccountMeta::new(f.sy_position, false),
                AccountMeta::new(f.emission_custody, false),
                AccountMeta::new(f.emission_dst, false),
                AccountMeta::new_readonly(f.sy_authority, false),
                AccountMeta::new_readonly(token_program_id(), false),
            ],
            data,
        }],
        &[],
    );
    assert!(res.is_err(), "claiming more than claimable should fail");
}

/// A rate move between two user actions changes what the second action pays —
/// this is the capability the whole fixture exists for.
#[test]
fn rate_move_between_actions_changes_payout() {
    let mut f = Fixture::new(Number::ONE);

    // Mint at rate 1.0 ...
    let minted = f.mint_sy(1_000_000);
    assert_eq!(minted.sy_out_amount, 1_000_000);

    // ... the SY appreciates to 1.25 ...
    let r2 = rate(1_250_000_000_000);
    f.set_exchange_rate(r2);
    // ... backed by real yield arriving in the base custody (a bare rate bump
    // with no new base would just leave custody insolvent, which the SPL
    // transfer would reject) ...
    let base_mint = f.base_mint;
    let base_custody = f.base_custody;
    f.mint_tokens_with_payer_authority(base_mint, base_custody, 250_000);

    // ... and redeeming the same SY now pays 25% more base.
    let base_before = f.token_balance(f.user_base);
    let redeemed = f.redeem_sy(1_000_000);
    assert_eq!(redeemed.base_out_amount, 1_250_000);
    assert_eq!(redeemed.exchange_rate, r2);
    assert_eq!(f.token_balance(f.user_base), base_before + 1_250_000);
}

/// Every discriminator in the interface is reachable in one run.
#[test]
fn all_discriminators_exercised() {
    let mut f = Fixture::new(Number::ONE);

    // [199] happened in Fixture::new. [7]:
    assert_eq!(f.get_sy_state().exchange_rate, Number::ONE);
    // [3]
    f.init_sy_personal_account();
    // [10]
    assert_eq!(f.get_position().owner, f.user.pubkey());
    // [1]
    assert_eq!(f.mint_sy(1_000_000).sy_out_amount, 1_000_000);
    // [2]
    assert_eq!(f.redeem_sy(400_000).base_out_amount, 400_000);
    // [5]
    assert_eq!(f.deposit_sy(300_000).exchange_rate, Number::ONE);
    // [6]
    assert_eq!(f.withdraw_sy(100_000).exchange_rate, Number::ONE);
    // [200]
    f.set_exchange_rate(rate(2_000_000_000_000));
    // [202]
    let emission_mint = f.emission_mint;
    f.add_emission_index(Number::ZERO, Some(emission_mint));
    // [201]
    f.set_emission_index(0, rate(1_000_000_000_000));
    // [203]
    f.deposit_sy(0);
    f.fund_emission(0, 7);
    // [8]
    f.claim_emission(Amount::All);

    let p = f.get_position();
    assert_eq!(p.sy_balance, 200_000);
    assert_eq!(p.emissions[0].amount_claimable, 0);
    assert_eq!(f.get_sy_state().exchange_rate, rate(2_000_000_000_000));
}

/// Unknown discriminators are rejected rather than silently succeeding — a
/// fuzzer must not get a false "ok" from a typo'd byte.
#[test]
fn unknown_discriminator_fails() {
    let mut f = Fixture::new(Number::ONE);
    let res = f.try_send(
        vec![Instruction {
            program_id: f.program_id,
            accounts: vec![AccountMeta::new(f.sy_global, false)],
            data: vec![99],
        }],
        &[],
    );
    assert!(res.is_err());
}
