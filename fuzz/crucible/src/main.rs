// SCOUT:TESTS:BEGIN
#[cfg(test)]
mod zz_collect_interest_realloc {
    use super::*;
    use crucible_test_context::TxOutcome;

    /// `collect_interest` with the outcome kept, so the error code is visible.
    /// (`action_collect_interest` only returns a bool.)
    pub fn collect_interest(f: &mut ExponentCoreFixture) -> TxOutcome {
        let s = f.users[f.actor].insecure_clone();
        let event_authority =
            Pubkey::find_program_address(&[b"__event_authority"], &f.program_id).0;
        let metas = vec![
            AccountMeta::new(f.sy_global, false),
            AccountMeta::new(f.vault_sy_position, false),
            AccountMeta::new(f.sy_custody, false),
            AccountMeta::new_readonly(f.sy_authority, false),
        ];
        f.ctx.program(f.program_id)
            .call(instruction::CollectInterest { amount: exponent_core::types::Amount::All })
            .accounts(accounts::CollectInterest {
                owner: s.pubkey(), yield_position: f.yield_position[f.actor], vault: f.vault,
                token_sy_dst: f.ta_sy[f.actor], escrow_sy: f.escrow_sy,
                authority: f.vault_authority, token_program: SPL_TOKEN_ID,
                sy_program: f.sy_program_id, treasury_sy_token_account: f.treasury_sy_ta,
                address_lookup_table: f.alt, event_authority, program: f.program_id,
            })
            .remaining_accounts_metas(metas)
            .signers(&[&s]).send().expect("collect_interest send")
    }

    fn position_len(f: &ExponentCoreFixture, pos: &Pubkey) -> usize {
        f.ctx.account_data(pos).expect("position").len()
    }
    fn tracker_count(f: &ExponentCoreFixture, pos: &Pubkey) -> u32 {
        let d = f.ctx.account_data(pos).expect("position");
        u32::from_le_bytes(d[120..124].try_into().unwrap())
    }
    fn interest_staged(f: &ExponentCoreFixture, pos: &Pubkey) -> u64 {
        let d = f.ctx.account_data(pos).expect("position");
        u64::from_le_bytes(d[112..120].try_into().unwrap())
    }

    #[test]
    fn add_emission_breaks_collect_interest_for_existing_positions() {
        let mut f = ExponentCoreFixture::setup();
        f.action_select_actor(0);
        assert!(f.action_acquire_sy(500_000_000), "acquire_sy");
        assert!(f.strip_exact(100_000_000), "strip");
        assert!(f.deposit_yt_exact(100_000_000), "deposit_yt");
        assert!(f.action_set_sy_exchange_rate(2_000), "rate -> 2.0");
        assert!(f.action_stage_yt_yield(), "stage_yt_yield");

        let pos = f.yield_position[0];
        println!("[before] trackers={} account_len={} interest_staged={}",
                 tracker_count(&f, &pos), position_len(&f, &pos), interest_staged(&f, &pos));
        assert!(interest_staged(&f, &pos) > 0, "expected staged interest");

        // Control on a clone of this exact state: collect_interest works today.
        let mut control = f.clone();
        let o = collect_interest(&mut control);
        println!("[control] collect_interest ok={}", o.is_success());
        assert!(o.is_success(), "control failed: {:#?}", o.logs());

        // The SY program registers the stream, then the hot admin registers it on the vault.
        // (The SY side must come first: `Vault::add_emission` reads
        // `sy_state.emission_indexes[self.emissions.len()]`, vault.rs:377.)
        assert!(f.mock_sy_add_emission_index(0, f.emission_mint), "sy add_emission_index");
        let o = f.run_add_emission(500);
        assert!(o.is_success(), "add_emission failed: {:#?}", o.logs());

        println!("[after add_emission] trackers={} account_len={}",
                 tracker_count(&f, &pos), position_len(&f, &pos));

        let staged_before = interest_staged(&f, &pos);
        let o = collect_interest(&mut f);
        println!("[after] collect_interest ok={}", o.is_success());
        let logs: Vec<String> = o.logs().to_vec();
        for l in logs.iter().rev().take(3).collect::<Vec<_>>().into_iter().rev() {
            println!("LOG: {l}");
        }
        assert!(!o.is_success(), "collect_interest unexpectedly still works");
        assert_eq!(interest_staged(&f, &pos), staged_before,
                   "staged interest moved despite the failure");

        // Recovery: stage_yt_yield DOES carry the realloc, so it resizes the position.
        assert!(f.action_stage_yt_yield(), "stage_yt_yield after add_emission");
        println!("[recovered] trackers={} account_len={}",
                 tracker_count(&f, &pos), position_len(&f, &pos));
        let o = collect_interest(&mut f);
        println!("[recovered] collect_interest ok={}", o.is_success());
        assert!(o.is_success(), "still broken after stage_yt_yield: {:#?}", o.logs());
    }
}

#[cfg(test)]
mod zz_emergency_emission_inflation {
    use super::*;

    /// strip -> deposit YT -> register one emission stream -> rate 1.0 to 2.0 -> stage.
    fn build_world() -> ExponentCoreFixture {
        let mut f = ExponentCoreFixture::setup();
        f.action_select_actor(0);
        assert!(f.action_acquire_sy(500_000_000), "acquire_sy");
        assert!(f.strip_exact(100_000_000), "strip");
        assert!(f.deposit_yt_exact(100_000_000), "deposit_yt");
        assert!(f.mock_sy_add_emission_index(0, f.emission_mint), "sy add_emission_index");
        let o = f.run_add_emission(0);
        assert!(o.is_success(), "add_emission failed: {:#?}", o.logs());
        assert!(f.action_set_sy_exchange_rate(2_000), "rate -> 2.0");
        assert!(f.action_stage_yt_yield(), "stage_yt_yield");
        f
    }

    /// `all_time_high` / `final_sy_exchange_rate` / `total_sy_in_escrow` straight out of the Vault
    /// account (borsh offsets per `state/vault.rs` field order; `address_lookup_table` at 232 is
    /// independently pinned in CLAUDE.md).
    fn vault_rates(f: &ExponentCoreFixture) -> (u128, u128, u64) {
        let d = f.ctx.account_data(&f.vault).expect("vault");
        let ath = u128::from_le_bytes(d[369..385].try_into().unwrap());
        let final_rate = u128::from_le_bytes(d[401..417].try_into().unwrap());
        let escrow = u64::from_le_bytes(d[433..441].try_into().unwrap());
        (ath, final_rate, escrow)
    }

    #[test]
    fn withdraw_yt_during_emergency_over_credits_emissions() {
        // ---- honest world: the SY rate stays at its all-time high -----------------
        let mut a = build_world();
        let pos_a = a.yield_position[0];
        let (_, _, index0_a, staged0_a) = a.read_position_emission(&pos_a, 0);
        let (ath, final_rate, escrow) = vault_rates(&a);
        println!("[world] ath={ath} final_rate={final_rate} escrow={escrow} \
                  tracker_index={index0_a} tracker_staged={staged0_a}");

        // The SY program pays out 0.001 reward token per SY held.
        const DELTA_1E12: u128 = 1_000_000_000;
        assert!(a.mock_sy_set_emission_index(0, DELTA_1E12), "set_emission_index");
        assert!(a.withdraw_yt_exact(1), "honest withdraw_yt");
        let (_, _, _, staged1_a) = a.read_position_emission(&pos_a, 0);
        let honest = staged1_a - staged0_a;
        let (_, final_a, escrow_a) = vault_rates(&a);
        println!("[honest] earned={honest} final_rate={final_a} escrow={escrow_a}");

        // ---- attack world: identical, but the SY rate has fallen back to 1.0 ------
        let mut b = build_world();
        let pos_b = b.yield_position[0];
        let (_, _, _, staged0_b) = b.read_position_emission(&pos_b, 0);
        assert_eq!(staged0_b, staged0_a, "the two worlds must start identical");
        assert!(b.mock_sy_set_emission_index(0, DELTA_1E12), "set_emission_index");
        assert!(b.action_set_sy_exchange_rate(1_000), "rate -> 1.0 (below ATH 2.0)");

        // Every sibling refuses to run now.
        assert!(!b.action_stage_yt_yield(), "stage_yt_yield should be blocked");
        assert!(!b.deposit_yt_exact(1), "deposit_yt should be blocked");
        assert!(!b.strip_exact(1_000_000), "strip should be blocked");
        assert!(!b.merge_exact(1_000_000), "merge should be blocked");

        // withdraw_yt is not blocked, and it installs the depressed rate.
        assert!(b.withdraw_yt_exact(1), "attack withdraw_yt");
        let (_, _, _, staged1_b) = b.read_position_emission(&pos_b, 0);
        let attacked = staged1_b - staged0_b;
        let (ath_b, final_b, escrow_b) = vault_rates(&b);
        println!("[attack] earned={attacked} final_rate={final_b} ath={ath_b} escrow={escrow_b}");

        // What the SY program credits the vault: index_delta * sy_balance.
        let (sy_balance, claimable) = a.read_vault_sy_position(0);
        let true_accrual = (DELTA_1E12 * sy_balance as u128 / NUMBER_ONE) as u64;
        println!("[truth] vault sy_balance={sy_balance} sy-side claimable={claimable} \
                  true_accrual={true_accrual}");

        // `sy_balance` is the WHOLE vault's SY (this actor's 100_000_000 strip plus the
        // 10_000_000 the fixture seeds the market with), so the honest share is strictly less
        // than the vault-wide accrual. The attacked figure is larger than the entire pot.
        assert!(honest < true_accrual,
                "honest share should be below the vault-wide accrual: {honest} vs {true_accrual}");
        assert!(attacked > honest, "no over-credit: attacked={attacked} honest={honest}");
        assert!(attacked > true_accrual,
                "attacked share should exceed the whole vault accrual: {attacked} vs {true_accrual}");
        println!("[compare] honest={honest} attacked={attacked} vault_wide_accrual={true_accrual} \
                  over-credit vs honest = {}%", attacked * 100 / honest);
    }

    /// Two YT holders, so the SECOND holder's ordinary `collect_interest` can be measured.
    fn build_world_two_holders() -> ExponentCoreFixture {
        let mut f = ExponentCoreFixture::setup();
        for actor in [0u8, 1u8] {
            f.action_select_actor(actor);
            assert!(f.action_acquire_sy(500_000_000), "acquire_sy actor {actor}");
            assert!(f.strip_exact(100_000_000), "strip actor {actor}");
            assert!(f.deposit_yt_exact(100_000_000), "deposit_yt actor {actor}");
        }
        assert!(f.mock_sy_add_emission_index(0, f.emission_mint), "sy add_emission_index");
        let o = f.run_add_emission(0);
        assert!(o.is_success(), "add_emission failed: {:#?}", o.logs());
        assert!(f.action_set_sy_exchange_rate(2_000), "rate -> 2.0");
        for actor in [0u8, 1u8] {
            f.action_select_actor(actor);
            assert!(f.action_stage_yt_yield(), "stage_yt_yield actor {actor}");
        }
        f
    }

    /// The follow-on path the issue-03 writeup asserted from source but did not measure: once a
    /// `withdraw_yt` has installed a depressed `final_sy_exchange_rate`, a DIFFERENT holder's
    /// ordinary `collect_interest` reads that stored rate and is over-credited too.
    ///
    /// This matters because it widens who is affected. The `withdraw_yt` result shows one caller
    /// over-claiming for themselves. This shows the depressed rate persists in the vault and
    /// distorts the next holder's payout as well, through an instruction that has no emergency
    /// guard and never refreshes from the SY program (`collect_interest.rs:115` into
    /// `common.rs:6-8`; it is the ONE user instruction that skips `update_from_sy_state`).
    ///
    /// Actor 1 does nothing abnormal in either world -- the two runs differ only in the SY rate
    /// that actor 0's `withdraw_yt` wrote into the vault beforehand.
    #[test]
    fn an_installed_depressed_rate_also_inflates_the_next_holders_collect_interest() {
        const DELTA_1E12: u128 = 1_000_000_000; // 0.001 reward token per SY

        // ---- honest world: actor 0 withdraws at the all-time-high rate ----------------
        let mut a = build_world_two_holders();
        let pos1_a = a.yield_position[1];
        let (_, _, _, staged0_a) = a.read_position_emission(&pos1_a, 0);
        assert!(a.mock_sy_set_emission_index(0, DELTA_1E12), "set_emission_index");
        a.action_select_actor(0);
        assert!(a.withdraw_yt_exact(1), "honest withdraw_yt refreshes the vault at the ATH");
        let (_, final_a, _) = vault_rates(&a);
        a.action_select_actor(1);
        assert!(a.action_collect_interest(), "actor 1 collect_interest (honest)");
        let (_, _, _, staged1_a) = a.read_position_emission(&pos1_a, 0);
        let honest = staged1_a - staged0_a;
        println!("[honest] stored final_rate={final_a} actor1 emission credit={honest}");

        // ---- attack world: identical, except the rate has fallen back to 1.0 ----------
        let mut b = build_world_two_holders();
        let pos1_b = b.yield_position[1];
        let (_, _, _, staged0_b) = b.read_position_emission(&pos1_b, 0);
        assert_eq!(staged0_b, staged0_a, "the two worlds must start identical");
        assert!(b.mock_sy_set_emission_index(0, DELTA_1E12), "set_emission_index");
        assert!(b.action_set_sy_exchange_rate(1_000), "rate -> 1.0 (below ATH 2.0)");
        b.action_select_actor(0);
        assert!(b.withdraw_yt_exact(1), "attack withdraw_yt installs the depressed rate");
        let (ath_b, final_b, _) = vault_rates(&b);
        assert!(final_b < ath_b, "the vault must now store a below-ATH rate: {final_b} < {ath_b}");

        // Actor 1 now does the most ordinary thing available to them.
        b.action_select_actor(1);
        assert!(b.action_collect_interest(), "actor 1 collect_interest (after the install)");
        let (_, _, _, staged1_b) = b.read_position_emission(&pos1_b, 0);
        let attacked = staged1_b - staged0_b;
        println!("[attack] stored final_rate={final_b} ath={ath_b} actor1 emission credit={attacked}");

        let (sy_balance, _) = a.read_vault_sy_position(0);
        let vault_wide = (DELTA_1E12 * sy_balance as u128 / NUMBER_ONE) as u64;
        println!("[truth] vault sy_balance={sy_balance} vault-wide accrual={vault_wide}");

        assert!(attacked > honest,
                "actor 1's credit should be inflated by the stored rate: attacked={attacked} \
                 honest={honest}");
        println!("[compare] actor1 honest={honest} attacked={attacked} \
                  ({}% of the honest figure), vault-wide accrual={vault_wide}",
                 attacked * 100 / honest.max(1));
    }
}

// ---------------------------------------------------------------------------------------------
// LEAD #4 PoC: an emission stream added on the EXTERNAL SY program, which Exponent has not
// registered, bricks every vault instruction.
//
// `Vault::update_from_sy_state` iterates the SY program's list and indexes the vault's own:
//     for (index, x) in sy_state.emission_indexes.iter().enumerate() {
//         self.emissions[index].last_seen_index = *x;      // state/vault.rs:356-357
// so `sy_state.emission_indexes.len() > vault.emissions.len()` is an out-of-bounds panic. Every
// value-flow instruction calls this via `update_vault_yield` (`instructions/vault/common.rs:22`).
// `Vault::add_emission` states the assumption out loud at `vault.rs:376`: "emissions are assumed to
// be added 1 at a time, and so the SY state should have the same number of indexes" -- but the SY
// program is a THIRD PARTY (Kamino / Jupiter / Hylo / Solstice), and nothing constrains it.
//
// Baseline first, so the failure cannot be blamed on the fixture.
// ---------------------------------------------------------------------------------------------
#[cfg(test)]
mod zz_sy_stream_dos {
    use super::*;

    #[test]
    fn unregistered_sy_emission_stream_bricks_the_vault() {
        let mut f = ExponentCoreFixture::setup();
        f.action_select_actor(0);
        assert!(f.action_acquire_sy(200_000_000), "setup: acquire_sy");

        // Baseline: the vault is healthy and strip works.
        assert!(f.strip_exact(10_000_000), "baseline strip must succeed");
        println!("BASELINE strip ok");

        // A third party adds ONE reward stream on the SY program. Exponent is not involved and
        // cannot prevent this; no Exponent instruction is called here.
        assert!(f.mock_sy_add_emission_index(0, f.emission_mint),
                "registering a stream on the SY program should succeed");
        println!("SY program now reports 1 emission stream; vault has registered 0");

        // Every subsequent vault instruction reads that list.
        // Every instruction that refreshes vault state from the SY program dies.
        for (name, ok) in [
            ("strip", f.strip_exact(10_000_000)),
            ("merge", f.merge_exact(1_000_000)),
            ("deposit_yt", f.deposit_yt_exact(1_000_000)),
            ("withdraw_yt", f.withdraw_yt_exact(1)),
            ("stage_yt_yield", f.action_stage_yt_yield()),
        ] {
            println!("AFTER  {name:<18} ok={ok}");
            assert!(!ok, "{name} unexpectedly succeeded -- the vault is not bricked");
        }
        // collect_interest is the ONE survivor, and for a telling reason: it is the only vault
        // instruction that never calls `update_from_sy_state` (it only does `yield_position_earn`,
        // `collect_interest.rs:115`), so it never reads the SY program's emission list. That is the
        // same omission that leaves `sy_for_pt` stale after it runs.
        let ci = f.action_collect_interest();
        println!("AFTER  collect_interest   ok={ci}  (does not refresh from SY state)");
        assert!(ci, "collect_interest is expected to survive; if it stopped, re-derive the claim");

        // Show the actual failure, not just the boolean.
        let user = f.users[0].clone();
        let ea = Pubkey::find_program_address(&[b"__event_authority"], &f.program_id).0;
        let o = f.ctx.program(f.program_id)
            .call(instruction::Strip { amount: 1_000_000 })
            .accounts(accounts::Strip {
                depositor: user.pubkey(), authority: f.vault_authority, vault: f.vault,
                sy_src: f.ta_sy[0], escrow_sy: f.escrow_sy, yt_dst: f.ta_yt[0],
                pt_dst: f.ta_pt[0], mint_yt: f.mint_yt, mint_pt: f.mint_pt,
                token_program: SPL_TOKEN_ID, address_lookup_table: f.alt,
                sy_program: f.sy_program_id, yield_position: f.vault_yield_position,
                event_authority: ea, program: f.program_id,
            })
            .remaining_accounts_metas(vec![
                AccountMeta::new(f.sy_global, false),
                AccountMeta::new(f.vault_sy_position, false),
                AccountMeta::new(f.sy_custody, false),
                AccountMeta::new_readonly(f.sy_authority, false),
            ])
            .signers(&[&*user]).send().expect("send");
        for l in o.logs() { println!("LOG: {l}"); }
        assert!(!o.is_success());

        // Recovery leg: is this permanent, or admin-recoverable? `add_emission` is the only vault
        // instruction that does NOT call `update_from_sy_state` before touching the emission list
        // (`vault/admin/add_emission.rs:64-89` goes straight to `Vault::add_emission`), so it is
        // reachable in the bricked state and re-syncs the lengths.
        let rec = f.run_add_emission(0);
        println!("RECOVERY add_emission ok={}", rec.is_success());
        if !rec.is_success() { for l in rec.logs() { println!("RECLOG: {l}"); } }
        assert!(rec.is_success(), "add_emission must be reachable in the bricked state");
        let after = f.strip_exact(1_000_000);
        println!("RECOVERY strip ok={after}");
        assert!(after, "strip must work again once the lists match");
    }
}

// =============================================================================================
// ---------------------------------------------------------------------------------------------
// PoC for LEAD #1: a newly added emission over-credits every pre-existing YT holder by the ENTIRE
// cumulative emission index, instead of only the accrual since the emission was added.
//
// Chain under test:
//   YieldTokenPosition::ensure_trackers seeds a new tracker at Number::ZERO
//     (state/yield_token_position.rs:125)
//   -> earn_emissions calls calc_share_value(0, emission.final_index, sy_balance)
//     (state/yield_token_position.rs:112)
//   -> calc_share_value has no zero-guard, so the delta is the FULL final_index
//     (utils/math.rs:4-13)
//
// Everything below is driven through real instructions against the real program in LiteSVM.
// ---------------------------------------------------------------------------------------------
#[cfg(test)]
mod emission_tracker_zero_index_poc {
    use super::*;

    /// 1e12 fixed point: 5.0 emission tokens per SY, cumulative, distributed by the SY protocol
    /// BEFORE Exponent ever knew about this stream.
    const BIG_INDEX: u128 = 5 * NUMBER_ONE;
    /// How far the index moves AFTER `add_emission`. This is the only accrual anybody has actually
    /// earned through the vault, and the only amount the SY program will ever pay the vault.
    const DELTA_INDEX: u128 = NUMBER_ONE / 100; // 0.01 emission tokens per SY

    const STRIP_AMOUNT: u64 = 100_000_000; // 100 SY at 6 decimals, per user

    #[test]
    fn new_emission_credits_the_entire_cumulative_index_to_pre_existing_holders() {
        let mut f = ExponentCoreFixture::setup();

        // ---- 1. two users strip and deposit YT, so both hold a real, pre-existing position ----
        for user in 0..2usize {
            f.action_select_actor(user as u8);
            assert!(f.action_acquire_sy(STRIP_AMOUNT), "acquire_sy failed for user {}", user);
            assert!(f.strip_exact(STRIP_AMOUNT), "strip failed for user {}", user);
            assert!(f.deposit_yt_exact(STRIP_AMOUNT), "deposit_yt failed for user {}", user);
        }
        // `strip` forwards SY straight through `escrow_sy` into the SY program via `deposit_sy`,
        // so the vault's SY balance lives on its position with the SY program.
        let (vault_sy, _) = f.read_vault_sy_position(0);
        println!("vault SY balance held by the SY program = {}", vault_sy);

        // ---- 2. the emission stream runs up to a large cumulative index on the SY program -----
        assert!(f.mock_sy_add_emission_index(0, f.emission_mint), "mock add_emission_index failed");
        assert!(f.mock_sy_set_emission_index(0, BIG_INDEX), "mock set_emission_index failed");

        // ---- 3. the admin registers the emission on the vault ---------------------------------
        // EmissionInfo::new seeds initial_index = last_seen_index = final_index = BIG_INDEX
        // (state/vault.rs:520-536): the vault correctly records "nothing before now counts".
        let add = f.run_add_emission(0);
        assert!(add.is_success(), "add_emission failed: {:#?}", add.logs());

        // ---- 4. a small, REAL accrual happens after the emission was added --------------------
        assert!(f.mock_sy_set_emission_index(0, BIG_INDEX + DELTA_INDEX), "index bump failed");
        // Touch the vault's SY position so the non-retroactive mock actually accrues it, then fund
        // the SY side's custody with exactly what it now owes.
        assert!(f.mock_sy_fund_vault_emission(0, 0), "fund_emission(touch) failed");
        let (_, legit_pool) = f.read_vault_sy_position(0);
        assert_eq!(legit_pool, ((vault_sy as u128 * DELTA_INDEX) / NUMBER_ONE) as u64);
        let emission_mint = f.emission_mint;
        let emission_custody = f.emission_custody;
        let payer = f.payer.clone();
        f.ctx.mint_to(&emission_mint, &emission_custody, legit_pool, &payer).unwrap();

        // ---- 5. user 0 touches their position: ensure_trackers runs for the first time --------
        f.action_select_actor(0);
        assert!(f.action_stage_yt_yield(), "stage_yt_yield failed for user 0");

        let (yt0, interest0, seen0, staged0) = f.read_position_emission(&f.yield_position[0], 0);
        // The sy_balance earn_emissions uses is interest.staged + py_to_sy(final_rate, yt_balance);
        // the SY exchange rate never moved here, so it is just the YT balance plus staged interest.
        let sy_balance0 = interest0 + yt0;
        let owed0 = ((sy_balance0 as u128 * DELTA_INDEX) / NUMBER_ONE) as u64;

        println!("\n=== user 0 after stage_yt_yield ===");
        println!("  yt_balance                 = {}", yt0);
        println!("  interest.staged            = {}", interest0);
        println!("  sy_balance for emissions   = {}", sy_balance0);
        println!("  tracker.last_seen_index    = {} (= vault emission final_index, 1e12 fp)", seen0);
        println!("  emissions[0].staged        = {}   <-- CREDITED", staged0);
        println!("  correct entitlement        = {}   (only the post-add accrual)", owed0);
        println!("  whole emission pool the SY program will ever pay = {}", legit_pool);

        let full_index_credit =
            (((BIG_INDEX + DELTA_INDEX) * sy_balance0 as u128) / NUMBER_ONE) as u64;
        assert_eq!(seen0, BIG_INDEX + DELTA_INDEX,
                   "vault final_index should be the current global index");
        assert_eq!(staged0, full_index_credit,
                   "staged credit equals final_index * sy_balance -- the zero-seeded delta");
        assert!(staged0 > owed0 * 100,
                "expected a gross over-credit, got staged={} owed={}", staged0, owed0);
        assert!(staged0 > legit_pool * 100,
                "expected the credit to dwarf the entire emission pool, got staged={} pool={}",
                staged0, legit_pool);
        println!("  over-credit factor         = {}x", staged0 / owed0.max(1));

        // The vault's own robot position -- which holds every YT that is not deposited into a user
        // position, and which feeds collect_treasury_emission -- is seeded by the same call.
        let (robot_yt, robot_int, _, robot_staged) =
            f.read_position_emission(&f.vault_yield_position, 0);
        println!("  vault robot position: yt_balance={} interest.staged={} emissions[0].staged={}",
                 robot_yt, robot_int, robot_staged);

        // ---- 6. user 0 actually collects, and takes the WHOLE pool ---------------------------
        let user0_dst = f.ta_emission[0];
        let before = f.ctx.token_balance(&user0_dst);
        let outcome = f.run_collect_emission(0, exponent_core::types::Amount::Some(legit_pool));
        assert!(outcome.is_success(), "collect_emission failed: {:#?}", outcome.logs());
        let user0_got = f.ctx.token_balance(&user0_dst) - before;
        println!("\n=== user 0 collect_emission(Some({})) ===", legit_pool);
        println!("  emission tokens received   = {}", user0_got);
        println!("  fair share of the pool     = {}", owed0);
        assert_eq!(user0_got, legit_pool,
                   "user 0 should have walked off with the entire emission pool");
        assert!(user0_got > owed0, "user 0 took more than they were owed");

        // ---- 7. user 1, owed exactly as much as user 0, gets nothing --------------------------
        f.action_select_actor(1);
        assert!(f.action_stage_yt_yield(), "stage_yt_yield failed for user 1");
        let (_, _, _, staged1) = f.read_position_emission(&f.yield_position[1], 0);
        let user1_dst = f.ta_emission[1];
        let before1 = f.ctx.token_balance(&user1_dst);
        let outcome1 = f.run_collect_emission(0, exponent_core::types::Amount::Some(owed0));
        let user1_got = f.ctx.token_balance(&user1_dst) - before1;
        println!("\n=== user 1 ===");
        println!("  emissions[0].staged        = {}", staged1);
        println!("  entitlement                = {}", owed0);
        println!("  collect_emission success   = {}", outcome1.is_success());
        println!("  error code                 = {:?}", outcome1.error_code());
        println!("  emission tokens received   = {}", user1_got);
        println!("  SY-side custody left       = {}", f.ctx.token_balance(&f.emission_custody));
        println!("  vault emission escrow left = {}", f.ctx.token_balance(&f.emission_escrow));

        assert_eq!(staged1, staged0, "user 1 is credited the same over-credit");
        assert_eq!(user1_got, 0, "user 1 got nothing: user 0 already drained the stream");
        assert!(!outcome1.is_success(),
                "user 1's claim of their own fair share should now fail");
    }
}

/* ---------------------------------------------------------------------------------------------
Observed output (cargo test --features admin_actions new_emission_credits -- --nocapture):

running 1 test
vault SY balance held by the SY program = 210000000

=== user 0 after stage_yt_yield ===
  yt_balance                 = 100000000
  interest.staged            = 0
  sy_balance for emissions   = 100000000
  tracker.last_seen_index    = 5010000000000 (= vault emission final_index, 1e12 fp)
  emissions[0].staged        = 501000000   <-- CREDITED
  correct entitlement        = 1000000   (only the post-add accrual)
  whole emission pool the SY program will ever pay = 2100000
  over-credit factor         = 501x
  vault robot position: yt_balance=10000000 interest.staged=0 emissions[0].staged=50100000

=== user 0 collect_emission(Some(2100000)) ===
  emission tokens received   = 2100000
  fair share of the pool     = 1000000

=== user 1 ===
  emissions[0].staged        = 501000000
  entitlement                = 1000000
  collect_emission success   = false
  error code                 = Some(14)
  emission tokens received   = 0
  SY-side custody left       = 0
  vault emission escrow left = 0
test emission_tracker_zero_index_poc::new_emission_credits_the_entire_cumulative_index_to_pre_existing_holders ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out; finished in 48.42s

Error code 14 is the MOCK SY program's `InsufficientClaimable`. A production SY program would
report its own equivalent, or the transfer out of an empty escrow would fail in SPL Token. Either
way user 1 receives nothing.
--------------------------------------------------------------------------------------------- */
/* ---------------------------------------------------------------------------------------------
P-0002 triage: "PT supply == YT supply while the vault is ACTIVE".

VERDICT: harness artifact. The only two instructions that move either supply are `strip`
(mints PT and YT 1:1, vault/strip.rs:111-125) and `merge` (burns PT always, burns YT only while
active, vault/merge.rs:120-130) -- confirmed by grepping every `mint_to`/`token_2022::burn` in the
program. Both use ONE timestamp per invocation (`util::now()`), so while the clock is inside
[start_ts, start_ts+duration] the PT-YT difference is invariant. A nonzero difference observed at an
active timestamp therefore has to have been created at a NON-active timestamp, which requires the
clock to move backwards -- impossible on-chain, but `action_advance_time` recomputes an ABSOLUTE
timestamp (`vault_start_ts + days*86400`) so the fuzzer rewinds it freely.
Crash `crash_8d60f151be1cf8a1` is literally advance_time(379) -> merge -> advance_time(115).

The two tests below are the end-to-end proof, both driving real instructions through the real
program and reading the SPL mint supplies back out of TestContext.
--------------------------------------------------------------------------------------------- */
#[cfg(test)]
mod zz_pt_yt_supply_divergence {
    use super::*;

    fn supplies(f: &ExponentCoreFixture) -> (u64, u64) {
        (
            f.mint_supply(&f.mint_pt).expect("pt mint supply"),
            f.mint_supply(&f.mint_yt).expect("yt mint supply"),
        )
    }

    /// Exactly P-0002's predicate, which is exactly `Vault::is_active` (state/vault.rs:105-117),
    /// evaluated against the SVM Clock sysvar rather than any mirrored Rust field.
    fn is_active(f: &ExponentCoreFixture) -> bool {
        let v = f
            .ctx
            .read_anchor_account::<exponent_core::state::Vault>(&f.vault)
            .expect("vault");
        let now = f.svm_unix_timestamp().expect("clock sysvar");
        now >= v.start_ts && now <= v.start_ts.saturating_add(v.duration)
    }

    /// The timestamp `action_advance_time(d)` produces: an ABSOLUTE value, not an increment.
    fn day(f: &ExponentCoreFixture, d: i64) -> i64 {
        f.vault_start_ts as i64 + d * 24 * 60 * 60
    }

    fn report(f: &ExponentCoreFixture, tag: &str) -> (u64, u64) {
        let (pt, yt) = supplies(f);
        println!(
            "  {tag:<34} ts={} active={} pt={} yt={} yt-pt={}",
            f.svm_unix_timestamp().unwrap(),
            is_active(f),
            pt,
            yt,
            yt as i64 - pt as i64
        );
        (pt, yt)
    }

    fn world() -> ExponentCoreFixture {
        let mut f = ExponentCoreFixture::setup();
        f.action_select_actor(0);
        assert!(f.action_acquire_sy(500_000_000), "acquire_sy");
        assert!(f.strip_exact(100_000_000), "strip");
        f
    }

    /// The counterexample the fuzzer found, minimised: merge once AFTER maturity, then move the
    /// clock BACK into the active window. Nothing diverges while the vault is active.
    #[test]
    fn only_a_post_maturity_merge_plus_a_clock_rewind_diverges_pt_from_yt() {
        let mut f = world();
        println!("=== P-0002 counterexample ===");
        let (pt0, yt0) = report(&f, "after strip(100_000_000)");
        assert!(is_active(&f), "vault should be active at day 0");
        assert_eq!(pt0, yt0, "strip mints PT and YT 1:1");

        // --- healthy leg: merge while ACTIVE burns both mints ---
        assert!(f.merge_exact(10_000_000), "merge while active");
        let (pt1, yt1) = report(&f, "merge(10_000_000) while active");
        assert!(is_active(&f));
        assert_eq!(pt1, yt1, "an active merge burns PT and YT 1:1");
        assert_eq!(pt0 - pt1, 10_000_000);

        // --- cross maturity: this is action_advance_time(379) on a 365-day vault ---
        let ts = day(&f, 379);
        ExponentCoreFixture::warp_clock(&mut f.ctx, ts);
        assert!(!is_active(&f), "day 379 is past maturity");
        assert!(f.merge_exact(10_000_000), "merge after maturity");
        let (pt2, yt2) = report(&f, "merge(10_000_000) after maturity");
        assert_eq!(pt1 - pt2, 10_000_000, "PT is burned after maturity");
        assert_eq!(yt2, yt1, "YT is deliberately NOT burned after maturity");
        assert_eq!(yt2 - pt2, 10_000_000, "the whole divergence is created here");

        // --- the rewind: action_advance_time(115) recomputes an ABSOLUTE timestamp ---
        let ts = day(&f, 115);
        ExponentCoreFixture::warp_clock(&mut f.ctx, ts);
        let (pt3, yt3) = report(&f, "clock rewound to day 115");
        assert!(is_active(&f), "day 115 is inside the active window");
        assert_eq!(
            (pt3, yt3),
            (pt2, yt2),
            "the rewind moves no tokens; it only relabels the state as active"
        );
        assert_ne!(pt3, yt3, "P-0002 fires here -- on state built while INACTIVE");
        println!(
            "  => P-0002 message: vault active (ts={}) but mint_pt.supply={} != mint_yt.supply={}",
            f.svm_unix_timestamp().unwrap(),
            pt3,
            yt3
        );
    }

    /// Healthy comparison: the same actions with a MONOTONIC clock. PT and YT stay equal for the
    /// whole active window, and the divergence that does appear after maturity is never observed
    /// at an active timestamp.
    #[test]
    fn pt_and_yt_never_diverge_while_active_under_a_monotonic_clock() {
        let mut f = world();
        println!("=== monotonic-clock control ===");

        for d in [0i64, 10, 200, 364] {
            let ts = day(&f, d);
            ExponentCoreFixture::warp_clock(&mut f.ctx, ts);
            assert!(f.strip_exact(5_000_000), "strip at day {d}");
            assert!(f.merge_exact(3_000_000), "merge at day {d}");
            let (pt, yt) = report(&f, &format!("day {d}: strip+merge"));
            assert!(is_active(&f), "day {d} must be active");
            assert_eq!(pt, yt, "PT != YT at day {d} under a monotonic clock");
        }

        // Past maturity the divergence appears -- and is correctly outside P-0002's scope.
        let ts = day(&f, 366);
        ExponentCoreFixture::warp_clock(&mut f.ctx, ts);
        assert!(f.merge_exact(3_000_000), "merge after maturity");
        let (pt, yt) = report(&f, "day 366: merge (post-maturity)");
        assert!(!is_active(&f), "day 366 is past maturity");
        assert_eq!(yt - pt, 3_000_000, "post-maturity merge burns PT only, by design");

        // Time only ever goes forward from here, so this state is never seen as "active".
        for d in [400i64, 499] {
            let ts = day(&f, d);
            ExponentCoreFixture::warp_clock(&mut f.ctx, ts);
            assert!(!is_active(&f), "day {d} must stay inactive");
        }
    }
}

/// issue-04 carried one claim that was reasoned from source and NOT reproduced: that
/// `collect_emission` indexes `position.emissions[index]` (`collect_emission.rs:97`) before
/// anything can grow the position, so on a position created before the emission existed it is an
/// out-of-bounds `Vec` index rather than a clean error. The house rule is that a claim in a writeup
/// is backed by a PoC or it is not in the writeup, so here it is, end to end.
///
/// The distinction matters for triage: `AccountDidNotSerialize` (the realloc half of issue-04) is a
/// clean revert an integrator can read, whereas a panic aborts the whole transaction with no error
/// code at all.
#[cfg(test)]
mod zz_collect_emission_oob {
    use super::*;

    #[test]
    fn collect_emission_panics_on_a_position_older_than_the_emission() {
        let mut f = ExponentCoreFixture::setup();
        f.actor = 0;

        // A real position, funded and staged, created while the vault had ZERO emissions.
        assert!(f.action_acquire_sy(500_000_000), "acquire_sy");
        assert!(f.strip_exact(200_000_000), "strip");
        assert!(f.deposit_yt_exact(200_000_000), "deposit_yt");
        let trackers_before = ExponentCoreFixture::tracker_count_of(&f, &f.yield_position[0]);
        assert_eq!(trackers_before, 0, "position must predate the emission");

        // Now the vault gains a reward stream. `add_emission` grows the vault and the vault's own
        // robot position -- never a user's (add_emission.rs:20-22, :43-48).
        assert!(f.action_enable_emission(), "enable_emission");
        let vault = f.ctx.read_anchor_account::<exponent_core::state::Vault>(&f.vault)
            .expect("vault");
        assert_eq!(vault.emissions.len(), 1, "vault has the stream");
        assert_eq!(ExponentCoreFixture::tracker_count_of(&f, &f.yield_position[0]), 0,
                   "the user's position was NOT grown");

        // `collect_emission` is the very next instruction the user runs -- no intervening
        // `stage_yt_yield` to resize the position.
        let outcome = f.run_collect_emission(0, exponent_core::types::Amount::All);
        let logs = outcome.logs().join("\n");
        println!("[oob] success={} logs=\n{}", outcome.is_success(), logs);
        assert!(!outcome.is_success(), "collect_emission must not succeed here");

        // The specific claim: a PANIC at the indexing expression, not a returned Anchor error.
        assert!(logs.contains("panicked"),
                "expected a panic from collect_emission.rs:97, got:\n{logs}");
        assert!(logs.contains("collect_emission.rs"),
                "the panic must come from collect_emission, got:\n{logs}");

        // And the documented recovery: any account may stage for the position, which resizes it,
        // after which the same call is a clean failure rather than a panic.
        assert!(f.action_stage_yt_yield(), "stage_yt_yield resizes the position");
        assert_eq!(ExponentCoreFixture::tracker_count_of(&f, &f.yield_position[0]), 1,
                   "position now carries the tracker");
        let after = f.run_collect_emission(0, exponent_core::types::Amount::All);
        let after_logs = after.logs().join("\n");
        println!("[oob] after stage: success={} ", after.is_success());
        assert!(!after_logs.contains("panicked"),
                "no panic once the position has been resized, got:\n{after_logs}");
    }
}

/// BLIND-SPOTS.md #3: is Exponent reachable by reentrancy through the SY program?
///
/// Every Exponent instruction that refreshes vault state CPIs into `get_sy_state` from the middle
/// of `update_vault_yield` (`common.rs:15-26`) -- the vault deserialized, the handler's mutations
/// not yet written back. If the SY program can call back into Exponent in that window, an inner
/// instruction's write is overwritten by the outer one's stale in-memory copy on exit.
///
/// The harness could not ask this question at all until the mock could invoke Exponent. Now it can,
/// so the answer is measured rather than assumed -- in either direction. If the Solana runtime
/// refuses the reentry, that closes the blind spot with a platform guarantee and no Exponent change
/// is needed; if it permits it, this is the setup for a real finding.
#[cfg(test)]
mod zz_sy_reentrancy {
    use super::*;

    #[test]
    fn the_sy_program_cannot_call_back_into_exponent_mid_cpi() {
        let mut f = ExponentCoreFixture::setup();
        f.action_select_actor(0);
        assert!(f.action_acquire_sy(500_000_000), "acquire_sy");
        assert!(f.strip_exact(100_000_000), "strip so the position has something to stage");

        // `stage_yt_yield` is the vehicle, NOT `strip`. `strip` reaches the SY program through
        // `do_deposit_sy` (`strip.rs:159`) and reads the SY state out of that call's return data;
        // it never invokes `get_sy_state`. Arming a `get_sy_state` reentry and then calling `strip`
        // would pass while proving nothing -- which is exactly what the first version of this test
        // did.
        let base = f.run_stage_yt_yield();
        assert!(base.is_success(), "baseline stage_yt_yield: {:#?}", base.logs());
        println!("[baseline] stage_yt_yield ok");

        let exponent = f.program_id;
        assert!(f.mock_sy_arm_reentrancy(Some(exponent)), "arm reentrancy");

        let out = f.run_stage_yt_yield();
        let logs = out.logs().join("\n");
        println!("[armed] stage_yt_yield ok={}\n{}", out.is_success(), logs);

        // The branch must have been REACHED, or the result says nothing either way.
        assert!(logs.contains("attempting reentrancy"),
                "the mock never reached its reentry attempt -- this test proves nothing:\n{logs}");

        if out.is_success() {
            panic!("REENTRANCY PERMITTED: the SY program called back into Exponent from inside \
                    get_sy_state and the outer stage_yt_yield still succeeded. That is the window \
                    where an inner write is overwritten by the outer handler's stale copy.");
        }

        // Refused. The closure now rests on the runtime's own words rather than on an inference.
        println!("[armed] reentry REFUSED by the runtime");

        assert!(f.mock_sy_arm_reentrancy(None), "disarm");
        let after = f.run_stage_yt_yield();
        assert!(after.is_success(), "stage_yt_yield works again once disarmed: {:#?}", after.logs());
        println!("[disarmed] stage_yt_yield ok");
    }
}

/// P-0012's rate coverage. The property is sound at any installed rate, but "sound at any rate" is
/// worth nothing unless the round trip actually COMPLETES at rates with awkward reciprocals -- and
/// a complete round trip is a narrow target (both legs must succeed and PT and YT must both return
/// to baseline). This drives it across a spread of rates and reports the exact SY delta at each.
#[cfg(test)]
mod zz_p0012_rate_coverage {
    use super::*;

    #[test]
    fn the_strip_merge_round_trip_completes_and_conserves_at_awkward_rates() {
        // 1/3 and 1/7 do not terminate in 1e12 fixed point, which is where a floor/ceil mismatch
        // between the two legs would show up if there were one.
        for rate_milli in [1_000u32, 3_000, 337, 7_000, 1_500, 999] {
            let mut f = ExponentCoreFixture::setup();
            f.action_select_actor(0);
            assert!(f.action_acquire_sy(900_000_000), "acquire_sy at {rate_milli}");
            assert!(f.action_set_sy_exchange_rate(rate_milli), "rate -> {rate_milli}/1000");

            let a = f.actor;
            let before = f.ctx.token_balance(&f.ta_sy[a]);
            let (pt0, yt0) = (f.ctx.token_balance(&f.ta_pt[a]), f.ctx.token_balance(&f.ta_yt[a]));

            let ran = f.action_probe_strip_merge_roundtrip(123_456_789);
            let after = f.ctx.token_balance(&f.ta_sy[a]);
            let (pt1, yt1) = (f.ctx.token_balance(&f.ta_pt[a]), f.ctx.token_balance(&f.ta_yt[a]));
            let complete = pt1 == pt0 && yt1 == yt0;
            println!("rate={rate_milli:>5}/1000 ran={ran} complete_round_trip={complete} \
                      sy {before} -> {after} (delta {})", after as i128 - before as i128);

            if complete {
                assert!(after <= before,
                        "round trip GAINED at rate {rate_milli}/1000: {before} -> {after}");
            }
        }
    }
}

/// P-0011's lead, measured directly: Exponent's own comment says *"Since emissions are
/// non-decreasing, that is the only constraint"* (`vault.rs:265`) and nothing validates it, while
/// `update_from_sy_state` copies the third-party SY program's indexes into vault state
/// unconditionally (`vault.rs:356-363`).
///
/// Two worlds, identical except that one sees the SY program report a LOWER index before climbing
/// to the same final value. If the treasury ends up with more in the rewound world, the same
/// accrual has been swept twice.
#[cfg(test)]
mod zz_rewound_emission_index {
    use super::*;

    fn treasury_emission(f: &ExponentCoreFixture) -> u64 {
        f.ctx.read_anchor_account::<exponent_core::state::Vault>(&f.vault)
            .ok().and_then(|v| v.emissions.first().map(|e| e.treasury_emission)).unwrap_or(0)
    }
    fn vault_last_seen(f: &ExponentCoreFixture) -> u128 {
        f.ctx.read_anchor_account::<exponent_core::state::Vault>(&f.vault)
            .ok().and_then(|v| v.emissions.first()
                .map(|e| ExponentCoreFixture::number_u128(&e.last_seen_index)))
            .unwrap_or(0)
    }

    /// `(treasury_emission, vault.last_seen_index)` after: accrue to 1.0, mature, [rewind to 0.5],
    /// climb to 2.0. Both worlds end with the SY program reporting exactly 2.0.
    fn run(rewind: bool) -> (u64, u128, u64, u64) {
        let mut f = ExponentCoreFixture::setup();
        f.action_select_actor(0);
        assert!(f.action_acquire_sy(500_000_000), "acquire_sy");
        assert!(f.strip_exact(200_000_000), "strip");
        assert!(f.deposit_yt_exact(200_000_000), "deposit_yt");
        assert!(f.action_enable_emission(), "enable stream 0");

        // Honest accrual to 1.0, observed by the vault.
        assert!(f.mock_sy_set_emission_index(0, NUMBER_ONE), "index -> 1.0");
        assert!(f.action_stage_yt_yield(), "observe 1.0");
        assert_eq!(vault_last_seen(&f), NUMBER_ONE, "vault has ingested 1.0");

        assert!(f.action_advance_time(400), "cross maturity");

        if rewind {
            // The SY program now reports a LOWER index than it did before.
            assert!(f.mock_sy_set_emission_index(0, NUMBER_ONE / 2), "index -> 0.5 (rewind)");
            assert!(f.action_stage_yt_yield(), "observe the rewind");
        }

        // Both worlds now climb to exactly 2.0.
        assert!(f.mock_sy_set_emission_index(0, 2 * NUMBER_ONE), "index -> 2.0");
        assert!(f.action_stage_yt_yield(), "observe 2.0");

        let escrow = f.ctx.read_anchor_account::<exponent_core::state::Vault>(&f.vault)
            .map(|v| v.total_sy_in_escrow).unwrap_or(0);
        // What the holder ends up owed, so the finding is not just "the treasury took more" but
        // "and here is who it came from".
        let staged = f.position_staged_emission(&f.yield_position[0], 0);
        (treasury_emission(&f), vault_last_seen(&f), escrow, staged)
    }

    #[test]
    fn a_rewound_sy_emission_index_sweeps_the_same_accrual_into_the_treasury_twice() {
        let (t_ctl, seen_ctl, escrow_ctl, staged_ctl) = run(false);
        println!("[control] treasury_emission={t_ctl} vault.last_seen={seen_ctl} \
                  escrow={escrow_ctl} holder_staged={staged_ctl}");
        let (t_bug, seen_bug, escrow_bug, staged_bug) = run(true);
        println!("[rewound] treasury_emission={t_bug} vault.last_seen={seen_bug} \
                  escrow={escrow_bug} holder_staged={staged_bug}");

        assert_eq!(escrow_ctl, escrow_bug, "same SY in escrow -- the accrual basis is identical");
        assert_eq!(seen_ctl, seen_bug, "both worlds end with the vault reporting the same index");

        // The honest sweep is measured from where the vault had already accounted: 1.0 -> 2.0.
        println!("[compare] treasury control={t_ctl} rewound={t_bug} delta={}",
                 t_bug as i128 - t_ctl as i128);
        println!("[compare] holder   control={staged_ctl} rewound={staged_bug} delta={}",
                 staged_bug as i128 - staged_ctl as i128);
        assert!(t_bug > t_ctl,
                "the rewind must enlarge the treasury sweep: {t_bug} vs {t_ctl}");
        assert_eq!(staged_ctl, staged_bug,
                   "the holder's claim is unchanged -- the extra sweep is not taken from what they \
                    are OWED, it is taken from the escrow that has to satisfy everyone");

        // Everything the SY program will ever pay for this stream is `final_index * sy_balance`.
        // Both worlds end at index 2.0 on an escrow of 210,000,000, so that is 420,000,000.
        let ever_accrued = (2 * NUMBER_ONE * escrow_ctl as u128 / NUMBER_ONE) as u64;
        let committed_ctl = t_ctl + staged_ctl;
        let committed_bug = t_bug + staged_bug;
        println!("[truth] the stream will ever pay {ever_accrued}; \
                  control commits {committed_ctl}, rewound commits {committed_bug}");
        assert!(committed_ctl <= ever_accrued,
                "the control must stay within what the stream pays: {committed_ctl} > {ever_accrued}");
        assert!(committed_bug > ever_accrued,
                "the rewound world must over-commit the escrow: {committed_bug} <= {ever_accrued}");
        println!("[truth] shortfall introduced by the rewind = {}",
                 committed_bug - ever_accrued);
    }
}

/// issue-01 claims the zero-seeded tracker "recurs independently for each emission - adding a
/// second stream later seeds a second zero tracker on every existing position". That was reasoned
/// from source and never executed, because until now the fixture could only ever hold ONE stream
/// (BLIND-SPOTS.md #1). This runs it.
///
/// It also exercises the parts of the program a single stream cannot reach at all: a non-zero
/// `collect_emission` index, and two parallel emission vectors that `calc_emission_surpluses`
/// zips (`vault.rs:281-289`).
#[cfg(test)]
mod zz_second_emission_stream {
    use super::*;

    #[test]
    fn a_second_stream_seeds_another_zero_tracker_on_every_existing_position() {
        let mut f = ExponentCoreFixture::setup();
        f.action_select_actor(0);
        assert!(f.action_acquire_sy(500_000_000), "acquire_sy");
        assert!(f.strip_exact(200_000_000), "strip");
        assert!(f.deposit_yt_exact(200_000_000), "deposit_yt");

        // Stream 0 is attached while its index is still zero -- the SAFE ordering, so nothing here
        // is contaminated by the issue-01 over-credit on stream 0.
        assert!(f.action_enable_emission(), "enable stream 0");
        assert!(f.action_stage_yt_yield(), "stage: create the tracker for stream 0");
        let pos = f.yield_position[0];
        let (_, _, seen0, staged0) = f.read_position_emission(&pos, 0);
        println!("[s0] tracker.last_seen={seen0} staged={staged0}");
        assert_eq!(staged0, 0, "stream 0 attached at index 0 credits nothing");

        // Stream 1 is given a HISTORY before Exponent adopts it -- the realistic case for any
        // reward programme that predates the vault.
        const HISTORY: u128 = 5 * NUMBER_ONE; // 5.0 reward tokens per SY
        assert!(f.mock_sy_add_emission_index(HISTORY, f.emission_mints[1]),
                "register stream 1 on the SY program WITH a running index");
        let o = f.run_add_emission_stream(1, 0);
        assert!(o.is_success(), "add_emission for stream 1: {:#?}", o.logs());

        let vault = f.ctx.read_anchor_account::<exponent_core::state::Vault>(&f.vault)
            .expect("vault");
        assert_eq!(vault.emissions.len(), 2, "the vault now tracks two streams");
        let initial_1 = ExponentCoreFixture::number_u128(&vault.emissions[1].initial_index);
        println!("[s1] vault recorded initial_index={initial_1} (the correct starting point)");
        assert_eq!(initial_1, HISTORY, "the vault DOES record where the stream started");

        // One touch. `ensure_trackers` appends the missing tracker for stream 1, seeded at zero.
        assert!(f.action_stage_yt_yield(), "stage: create the tracker for stream 1");
        let yt = f.position_yt_balance(0);
        let (_, _, seen1, staged1) = f.read_position_emission(&pos, 1);
        println!("[s1] tracker.last_seen={seen1} staged={staged1} yt_balance={yt}");

        // The credit is the FULL cumulative index times the balance, not the delta since the
        // stream was attached -- which is zero, because nothing has moved since.
        let expected_if_correct = 0u64;
        let expected_if_buggy = (HISTORY * yt as u128 / NUMBER_ONE) as u64;
        println!("[s1] correct credit = {expected_if_correct}; \
                  full-index credit = {expected_if_buggy}; ACTUAL = {staged1}");
        assert_eq!(staged1, expected_if_buggy,
                   "the second stream is over-credited exactly like the first");

        // And stream 0, attached safely, is untouched -- so this is the second stream's own
        // defect and not spill-over from the first.
        let (_, _, _, staged0_after) = f.read_position_emission(&pos, 0);
        assert_eq!(staged0_after, 0, "stream 0 stays correct");

        // P-0003 would now fire on stream 1 and not on stream 0. Check the same quantity the
        // property does, so this doubles as the per-stream liveness check for it.
        let vault_holds = f.ctx.token_balance(&f.emission_escrows[1]) as u128
            + f.ctx.token_balance(&f.treasury_emission_tas[1]) as u128
            + f.read_vault_sy_position(1).1 as u128;
        println!("[s1] vault holds-or-can-claim = {vault_holds}; staged = {staged1}");
        assert!(staged1 as u128 > vault_holds + 2,
                "P-0003 must see this on stream 1: staged={staged1} holds={vault_holds}");
    }
}

/// P-0004 liveness. A gated property that never opens its gate is worse than no property at all --
/// it reports "no counterexample" for a reason that has nothing to do with the code. This drives
/// the whole vault subsystem through real instructions and asserts, at every step, both that the
/// gate is genuinely OPEN and that the adversary's value is conserved.
///
/// It is also the value-model's own regression test: if `adversary_value_py` mis-prices anything,
/// a plain strip or merge shifts the number and this fails, rather than the fuzzer reporting a
/// "bug" that is really an accounting mistake in the harness.
#[cfg(test)]
mod zz_p0004_liveness {
    use super::*;

    #[test]
    fn adversary_value_is_conserved_under_an_open_gate() {
        let mut f = ExponentCoreFixture::setup();
        f.actor = 0; // the designated adversary

        let start = f.baseline_adversary_py;
        assert!(f.p0004_gate_open(), "gate must be open in the state every iteration starts from");
        assert_eq!(f.adversary_value_py(), start, "baseline must be self-consistent");
        println!("[p0004] baseline={} rate={} market_pt={} market_sy={}",
                 start, f.baseline_sy_rate, f.baseline_market_pt, f.baseline_market_sy);

        let mut step = |f: &mut ExponentCoreFixture, tag: &str| {
            assert!(f.p0004_gate_open(), "gate closed after {tag} -- P-0004 would be vacuous here");
            let v = f.adversary_value_py();
            println!("[p0004] after {tag:<22} value={v} delta={}", v as i128 - start as i128);
            assert!(v <= start, "adversary GAINED value after {tag}: {v} > {start}");
        };

        assert!(f.action_acquire_sy(500_000_000), "acquire_sy");
        step(&mut f, "acquire_sy");

        assert!(f.strip_exact(200_000_000), "strip");
        step(&mut f, "strip");

        assert!(f.deposit_yt_exact(100_000_000), "deposit_yt");
        step(&mut f, "deposit_yt");

        assert!(f.action_stage_yt_yield(), "stage_yt_yield");
        step(&mut f, "stage_yt_yield");

        assert!(f.action_collect_interest(), "collect_interest");
        step(&mut f, "collect_interest");

        assert!(f.withdraw_yt_exact(100_000_000), "withdraw_yt");
        step(&mut f, "withdraw_yt");

        // The round trip: everything stripped comes back. This is the leg that P-0006 would have
        // asserted on its own, and it is exact here because the rate has never moved.
        assert!(f.merge_exact(200_000_000), "merge");
        step(&mut f, "merge");

        let end = f.adversary_value_py();
        assert_eq!(end, start,
                   "a complete strip -> merge round trip at an unchanged rate must return EXACTLY \
                    what went in; got {end} from {start}");

        // The counterparty guard must actually bite. A trade by ANY actor moves the market off its
        // baseline, and from then on an adversary gain could be an honest transfer rather than
        // value creation -- so the gate has to shut. This is the assertion that keeps P-0004 from
        // silently degrading into the naive per-actor check the playbook warns about.
        assert!(f.action_acquire_sy(400_000_000), "counterparty acquires SY");
        assert!(f.strip_exact(200_000_000), "counterparty strips for PT");
        assert!(f.p0004_gate_open(), "vault-only actions must not shut the gate");
        let before = (f.ctx.token_balance(&f.market_escrow_pt),
                      f.ctx.token_balance(&f.market_escrow_sy),
                      f.market_sy_position_balance());
        assert!(f.action_trade_pt_clamped(100_000, false), "trade_pt");
        let after = (f.ctx.token_balance(&f.market_escrow_pt),
                     f.ctx.token_balance(&f.market_escrow_sy),
                     f.market_sy_position_balance());
        println!("[p0004] market before={before:?} after={after:?}");
        assert_ne!(before, after, "the trade must have moved the market at all");
        assert!(!f.p0004_gate_open(), "gate must shut once the market has a counterparty position");

        // And it must shut on a rate move, which is the other way a gain can be legitimate.
        let mut g = ExponentCoreFixture::setup();
        assert!(g.p0004_gate_open(), "fresh world");
        assert!(g.action_set_sy_exchange_rate(2_000), "move the rate");
        assert!(!g.p0004_gate_open(), "gate must shut once the SY rate moves");
    }
}

// ---------------------------------------------------------------------------------------------
// LEAD: "SY rate round trip pays YT twice."  1.0 -> 0.5 -> 1.0 was suspected to leave a touched
// position's `interest.last_seen_index` at 0.5, so the climb back would credit
// (1/0.5 - 1/1.0) * yt = 1.0 * yt SY out of the SY that backs PT.
//
// The chain the lead assumed:
//   calc_earned_sy(yt, last_seen, cur) pays when last_seen < cur (yield_token_position.rs:202-220)
//   -> earn_sy_interest sets last_seen := vault.final_sy_exchange_rate (:91)
//   -> update_from_sy_state sets final := cur_rate whenever the vault is active (vault.rs:352-354)
//   -> so a dip should install 0.5 on the position and a recovery should pay for it.
//
// The link that does not hold is the third one, and the guard is `is_in_emergency_mode`
// (vault.rs:120-122): `all_time_high > last_seen_rate`. Because ATH is one-way (vault.rs:348), the
// whole of any dip is spent in emergency mode, and `earn_sy_interest` early-returns there WITHOUT
// writing `last_seen_index` (yield_token_position.rs:69-71). This module drives that end to end.
// ---------------------------------------------------------------------------------------------
#[cfg(test)]
mod zz_rate_round_trip_interest {
    use super::*;
    use crucible_test_context::TxOutcome;

    /// `YieldTokenPosition.interest.last_seen_index`, low 128 bits (1e12 fixed point).
    /// Layout: 8 disc + 32 owner + 32 vault + 8 yt_balance = 80, then the `Number` ([u64; 4] LE).
    /// The sibling field `interest.staged` at 112 is independently pinned by the issue-01 PoC.
    fn pos_last_seen(f: &ExponentCoreFixture, pos: &Pubkey) -> u128 {
        let d = f.ctx.account_data(pos).expect("yield position");
        u128::from_le_bytes(d[80..96].try_into().unwrap())
    }
    fn pos_staged(f: &ExponentCoreFixture, pos: &Pubkey) -> u64 {
        let d = f.ctx.account_data(pos).expect("yield position");
        u64::from_le_bytes(d[112..120].try_into().unwrap())
    }
    fn pos_yt(f: &ExponentCoreFixture, pos: &Pubkey) -> u64 {
        let d = f.ctx.account_data(pos).expect("yield position");
        u64::from_le_bytes(d[72..80].try_into().unwrap())
    }

    /// Every vault field this investigation turns on, straight out of the account.
    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    struct V {
        last_seen_rate: u128,
        ath: u128,
        final_rate: u128,
        escrow: u64,
        sy_for_pt: u64,
        pt_supply: u64,
        treasury: u64,
        uncollected: u64,
    }
    fn vault_of(f: &ExponentCoreFixture) -> V {
        let v = f.ctx.read_anchor_account::<exponent_core::state::Vault>(&f.vault).expect("vault");
        V {
            last_seen_rate: ExponentCoreFixture::number_u128(&v.last_seen_sy_exchange_rate),
            ath: ExponentCoreFixture::number_u128(&v.all_time_high_sy_exchange_rate),
            final_rate: ExponentCoreFixture::number_u128(&v.final_sy_exchange_rate),
            escrow: v.total_sy_in_escrow,
            sy_for_pt: v.sy_for_pt,
            pt_supply: v.pt_supply,
            treasury: v.treasury_sy,
            uncollected: v.uncollected_sy,
        }
    }
    /// The protocol's own solvency condition, `Vault::sy_balance_invariant` (vault.rs:110-113),
    /// which is `#[cfg(test)]` with zero call sites on-chain. This is P-0001.
    fn p0001(v: &V) -> bool {
        v.escrow as u128 >= v.sy_for_pt as u128 + v.treasury as u128 + v.uncollected as u128
    }
    fn show(tag: &str, v: &V) {
        println!("  [{tag:<26}] last_seen={} ath={} final={} | escrow={} sy_for_pt={} \
                  treasury={} uncollected={} pt_supply={} | P-0001 slack={}",
                 v.last_seen_rate, v.ath, v.final_rate, v.escrow, v.sy_for_pt, v.treasury,
                 v.uncollected, v.pt_supply,
                 v.escrow as i128 - (v.sy_for_pt as i128 + v.treasury as i128
                                     + v.uncollected as i128));
    }

    /// `stage_yt_yield` with the outcome kept, so the Anchor error is visible rather than a bool.
    fn stage(f: &mut ExponentCoreFixture) -> TxOutcome {
        let s = f.users[f.actor].insecure_clone();
        let ea = Pubkey::find_program_address(&[b"__event_authority"], &f.program_id).0;
        let metas = vec![
            AccountMeta::new(f.sy_global, false),
            AccountMeta::new(f.vault_sy_position, false),
            AccountMeta::new(f.sy_custody, false),
            AccountMeta::new_readonly(f.sy_authority, false),
        ];
        let payer = f.payer.clone();
        f.ctx.program(f.program_id)
            .call(instruction::StageYtYield {})
            .accounts(accounts::StageYtYield {
                payer: s.pubkey(), vault: f.vault,
                user_yield_position: f.yield_position[f.actor],
                yield_position: f.vault_yield_position, sy_program: f.sy_program_id,
                address_lookup_table: f.alt, system_program: system_program::ID,
                event_authority: ea, program: f.program_id,
            })
            .remaining_accounts_metas(metas)
            .signers(&[&*payer, &s]).send().expect("stage_yt_yield send")
    }

    const STRIP: u64 = 100_000_000; // 100 SY at 6 decimals
    const ONE: u128 = NUMBER_ONE;

    /// actor 0 is the YT holder (strips, then deposits all its YT into its own position).
    /// actor 1 is a pure PT holder (strips, keeps PT, and leaves its YT unstaked).
    fn world() -> ExponentCoreFixture {
        let mut f = ExponentCoreFixture::setup();
        f.action_select_actor(0);
        assert!(f.action_acquire_sy(500_000_000), "acquire_sy 0");
        assert!(f.strip_exact(STRIP), "strip 0");
        assert!(f.deposit_yt_exact(STRIP), "deposit_yt 0");
        f.action_select_actor(1);
        assert!(f.action_acquire_sy(500_000_000), "acquire_sy 1");
        assert!(f.strip_exact(STRIP), "strip 1");
        f.action_select_actor(0);
        assert!(f.action_stage_yt_yield(), "stage 0");
        f
    }

    // -----------------------------------------------------------------------------------------
    // 1. The control. The credit machinery works, and an HONEST 1.0 -> 2.0 rise pays the YT
    //    holder exactly (1/1.0 - 1/2.0) * yt. Without this, "the dip credited nothing" would be
    //    indistinguishable from "the test never exercised the credit path".
    // -----------------------------------------------------------------------------------------
    #[test]
    fn control_a_genuine_rate_rise_does_credit_yt_interest() {
        let mut f = world();
        let pos = f.yield_position[0];
        let v0 = vault_of(&f);
        println!("\n=== control: honest 1.0 -> 2.0 ===");
        show("start", &v0);
        assert_eq!(pos_last_seen(&f, &pos), ONE, "position starts at rate 1.0");
        assert_eq!(pos_staged(&f, &pos), 0);

        assert!(f.action_set_sy_exchange_rate(2_000), "rate -> 2.0");
        assert!(f.action_stage_yt_yield(), "stage at 2.0");

        let v1 = vault_of(&f);
        show("after rise+stage", &v1);
        let staged = pos_staged(&f, &pos);
        let yt = pos_yt(&f, &pos);
        // (1/1.0 - 1/2.0) * 100_000_000 = 50_000_000 SY
        let expect = ((ONE * ONE / ONE - ONE * ONE / (2 * ONE)) as u128 * yt as u128 / ONE) as u64;
        println!("  yt={yt} staged={staged} expected=(1/1.0 - 1/2.0)*yt={expect}");
        assert_eq!(staged, 50_000_000, "the honest credit is half the YT balance in SY");
        assert_eq!(expect, 50_000_000);
        assert_eq!(pos_last_seen(&f, &pos), 2 * ONE, "last_seen followed the rate UP");

        // `uncollected_sy` grows by the credit to EVERY position, and the vault owns one itself
        // (`Vault.yield_position`, vault.rs:37-39) that holds all unstaked YT -- here actor 1's
        // 100,000,000 plus the 10,000,000 the fixture seeds the market with.
        let robot_yt = pos_yt(&f, &f.vault_yield_position);
        let robot_staged = pos_staged(&f, &f.vault_yield_position);
        println!("  vault robot position: yt={robot_yt} staged={robot_staged}");
        assert_eq!(robot_staged, 55_000_000, "the robot earns on the other 110,000,000 YT");
        assert_eq!(v1.uncollected - v0.uncollected, staged + robot_staged,
                   "uncollected_sy grew by exactly the sum of the two credits");
        // And the credit came straight out of the SY backing PT: escrow unchanged, sy_for_pt halved.
        assert_eq!(v1.escrow, v0.escrow, "no SY entered the vault");
        assert_eq!(v0.sy_for_pt - v1.sy_for_pt, v1.uncollected - v0.uncollected,
                   "every unit credited to YT came out of sy_for_pt");
        assert!(p0001(&v1), "P-0001 must still hold: {v1:?}");
    }

    // -----------------------------------------------------------------------------------------
    // 2. The lead itself, end to end. 1.0 -> 0.5 -> 1.0, touching the position by every means the
    //    program still permits during the dip.
    // -----------------------------------------------------------------------------------------
    #[test]
    fn a_rate_round_trip_credits_the_yt_holder_nothing() {
        let mut f = world();
        let pos = f.yield_position[0];
        let v0 = vault_of(&f);
        let staged0 = pos_staged(&f, &pos);
        let yt0 = pos_yt(&f, &pos);
        println!("\n=== dip: 1.0 -> 0.5 -> 1.0 ===");
        show("start (rate 1.0)", &v0);
        println!("  position: yt={yt0} last_seen={} staged={staged0}", pos_last_seen(&f, &pos));
        assert_eq!(pos_last_seen(&f, &pos), ONE);

        // ---- the dip -------------------------------------------------------------------------
        assert!(f.action_set_sy_exchange_rate(500), "rate -> 0.5");

        // Everything that could install the depressed rate on the position, in one place.
        let blocked = stage(&mut f);
        println!("  stage_yt_yield during the dip: success={} error={:?}",
                 blocked.is_success(), blocked.error_code());
        for l in blocked.logs().iter().filter(|l| l.contains("Error") || l.contains("Emergency")) {
            println!("  LOG: {l}");
        }
        assert!(!blocked.is_success(), "stage_yt_yield must be refused during the dip");
        assert!(blocked.logs().join("\n").contains("VaultInEmergencyMode"),
                "expected VaultInEmergencyMode, got:\n{}", blocked.logs().join("\n"));

        for (name, ok) in [
            ("deposit_yt", f.deposit_yt_exact(1)),
            ("strip",      f.strip_exact(1_000_000)),
            ("merge",      f.merge_exact(1_000_000)),
        ] {
            println!("  {name:<14} during the dip: ok={ok}");
            assert!(!ok, "{name} must be refused during the dip");
        }

        // withdraw_yt and collect_interest carry NO emergency guard (withdraw_yt.rs:181-199,
        // collect_interest.rs:96-115) and both call `yield_position_earn`. If the position's
        // `last_seen_index` were going to follow the rate down, this is where it would happen.
        assert!(f.withdraw_yt_exact(1), "withdraw_yt is not emergency-guarded and must succeed");
        assert!(f.action_collect_interest(), "collect_interest is not emergency-guarded");

        let vd = vault_of(&f);
        show("during dip (rate 0.5)", &vd);
        let last_seen_in_dip = pos_last_seen(&f, &pos);
        println!("  position: yt={} last_seen={last_seen_in_dip} staged={}",
                 pos_yt(&f, &pos), pos_staged(&f, &pos));

        // THE CRUX. The VAULT followed the rate all the way down -- `final_sy_exchange_rate` is
        // 0.5 -- but the POSITION did not, because `earn_sy_interest` returned early.
        assert_eq!(vd.final_rate, ONE / 2, "the vault's final rate DID follow the rate down");
        assert_eq!(vd.last_seen_rate, ONE / 2, "vault last_seen_sy_exchange_rate = 0.5");
        assert_eq!(vd.ath, ONE, "ATH is one-way and stayed at 1.0");
        assert_eq!(last_seen_in_dip, ONE,
                   "position last_seen must NOT have followed the rate down to 0.5");

        // ---- the recovery --------------------------------------------------------------------
        assert!(f.action_set_sy_exchange_rate(1_000), "rate -> 1.0");
        let ok = stage(&mut f);
        println!("  stage_yt_yield after recovery: success={}", ok.is_success());
        assert!(ok.is_success(), "stage must work again once the rate is back at the ATH: {:#?}",
                ok.logs());

        let v1 = vault_of(&f);
        show("after recovery (rate 1.0)", &v1);
        let staged1 = pos_staged(&f, &pos);
        println!("  position: yt={} last_seen={} staged={staged1}",
                 pos_yt(&f, &pos), pos_last_seen(&f, &pos));

        // The predicted exploit was a credit of (1/0.5 - 1/1.0) * yt = 1.0 * yt = 100_000_000 SY.
        println!("  predicted-by-the-lead credit = {} SY; ACTUAL credit = {} SY",
                 yt0, staged1 - staged0);
        assert_eq!(staged1, staged0, "the round trip credited the YT holder NOTHING");
        assert_eq!(v1.uncollected, vd.uncollected, "uncollected_sy did not grow");
        assert_eq!(pos_last_seen(&f, &pos), ONE, "position is back in sync at 1.0");

        // ---- who paid? nobody. -----------------------------------------------------------------
        assert!(p0001(&v1), "P-0001 must hold after the round trip: {v1:?}");
        assert!(v1.sy_for_pt >= v0.sy_for_pt.saturating_sub(1),
                "sy_for_pt must not have been eaten: {} -> {}", v0.sy_for_pt, v1.sy_for_pt);

        // The PT holder (actor 1) never touched a yield position and can still redeem in full.
        f.action_select_actor(1);
        let sy_before = f.ctx.token_balance(&f.ta_sy[1]);
        assert!(f.merge_exact(STRIP), "PT holder must be able to merge after the round trip");
        let got = f.ctx.token_balance(&f.ta_sy[1]) - sy_before;
        println!("  PT holder merged {STRIP} PY and received {got} SY (put in {STRIP} SY)");
        assert_eq!(got, STRIP, "the PT holder gets back exactly what they put in");
        let v2 = vault_of(&f);
        show("after PT holder merges", &v2);
        assert!(p0001(&v2), "P-0001 after the merge: {v2:?}");
    }

    // -----------------------------------------------------------------------------------------
    // 3. The variant the guard is actually written for: a position that first appears DURING the
    //    dip, whose `last_seen_index` is therefore Number::ZERO rather than a stale high rate.
    //    `calc_earned_sy`'s own zero-guard (yield_token_position.rs:207-211) closes it.
    // -----------------------------------------------------------------------------------------
    #[test]
    fn a_position_that_first_earns_during_the_dip_is_not_credited_either() {
        let mut f = world();
        // actor 2 has never had a yield position. Give it YT before the dip but do NOT deposit,
        // so nothing has ever written its `interest.last_seen_index`.
        f.action_select_actor(2);
        assert!(f.action_acquire_sy(500_000_000), "acquire_sy 2");
        assert!(f.strip_exact(STRIP), "strip 2");

        assert!(f.action_set_sy_exchange_rate(500), "rate -> 0.5");
        // deposit_yt is refused during the dip -- vault.rs:120-122 via deposit_yt.rs:192-196,
        // whose comment says exactly why: "This prevents users from increasing their position
        // amount with a lower last_seen_index, which would break the economics."
        assert!(!f.deposit_yt_exact(STRIP), "deposit_yt during the dip must be refused");

        // So the only way in is after the recovery, at which point the rate is back at the ATH.
        assert!(f.action_set_sy_exchange_rate(1_000), "rate -> 1.0");
        assert!(f.deposit_yt_exact(STRIP), "deposit_yt after recovery");
        let pos2 = f.yield_position[2];
        println!("\n=== late joiner ===");
        println!("  position 2: yt={} last_seen={} staged={}",
                 pos_yt(&f, &pos2), pos_last_seen(&f, &pos2), pos_staged(&f, &pos2));
        assert_eq!(pos_staged(&f, &pos2), 0, "a position created after the dip is credited nothing");
        assert_eq!(pos_last_seen(&f, &pos2), ONE);

        assert!(f.action_stage_yt_yield(), "stage 2");
        assert_eq!(pos_staged(&f, &pos2), 0, "and still nothing after a stage");
        let v = vault_of(&f);
        show("late joiner, after stage", &v);
        assert!(p0001(&v));
    }

    // -----------------------------------------------------------------------------------------
    // 4. The hardest remaining shape: freeze `final_sy_exchange_rate` LOW by maturing the vault
    //    while the rate is depressed, then let the rate recover above the ATH. This is the only
    //    way `final` can be lower at a later non-emergency moment than at an earlier one, and it
    //    is the last place a position could be paid for a recovery it did not earn.
    // -----------------------------------------------------------------------------------------
    #[test]
    fn maturing_during_the_dip_does_not_pay_for_the_recovery_either() {
        let mut f = world();
        let pos = f.yield_position[0];

        // Establish a real gain first, so the position sits at a HIGH last_seen (2.0).
        assert!(f.action_set_sy_exchange_rate(2_000), "rate -> 2.0");
        assert!(f.action_stage_yt_yield(), "stage at 2.0");
        let staged_at_peak = pos_staged(&f, &pos);
        assert_eq!(pos_last_seen(&f, &pos), 2 * ONE);
        println!("\n=== mature-in-the-dip ===");
        show("peak (rate 2.0)", &vault_of(&f));
        println!("  position last_seen=2.0 staged={staged_at_peak}");

        // Dip to 1.0 and push the depressed rate into `final_sy_exchange_rate` with the one
        // instruction that still runs: withdraw_yt (no emergency guard).
        assert!(f.action_set_sy_exchange_rate(1_000), "rate -> 1.0 (below ATH 2.0)");
        assert!(f.withdraw_yt_exact(1), "withdraw_yt installs the depressed final rate");
        let vd = vault_of(&f);
        show("dip, still active", &vd);
        assert_eq!(vd.final_rate, ONE, "final rate is now 1.0 while ATH is 2.0");
        assert_eq!(vd.ath, 2 * ONE);
        assert_eq!(pos_last_seen(&f, &pos), 2 * ONE, "position still at 2.0 -- earn returned early");

        // Mature the vault while it is in emergency mode, so `final` freezes at the DEPRESSED 1.0.
        assert!(f.action_advance_time(400), "cross the 365-day maturity");
        assert!(f.action_collect_interest(), "an unguarded instruction after maturity");
        let vm = vault_of(&f);
        show("after maturity (dip)", &vm);
        assert_eq!(vm.final_rate, ONE, "final is frozen at the depressed 1.0");

        // Now recover the rate ABOVE the old ATH. Emergency clears; `final` stays frozen at 1.0.
        // `withdraw_yt` cannot be used here -- it requires `is_active` (withdraw_yt.rs:82-94) --
        // so drive it with `stage_yt_yield`, which refreshes from the SY state and then earns.
        // The `collect_interest` above already paid out the 50,000,000 staged at the peak, so the
        // baseline for "was the holder credited again" is what the position holds RIGHT NOW.
        let staged_before_recovery = pos_staged(&f, &pos);
        println!("  staged immediately before the recovery = {staged_before_recovery}");
        assert!(f.action_set_sy_exchange_rate(3_000), "rate -> 3.0, above the ATH");
        let rec = stage(&mut f);
        println!("  stage_yt_yield after post-maturity recovery: success={} error={:?}",
                 rec.is_success(), rec.error_code());
        assert!(rec.is_success(), "stage must run once emergency clears: {:#?}", rec.logs());
        let v1 = vault_of(&f);
        show("after recovery (rate 3.0)", &v1);
        let staged1 = pos_staged(&f, &pos);
        println!("  position: last_seen={} staged={staged1} (was {staged_at_peak})",
                 pos_last_seen(&f, &pos));
        assert!(!(v1.ath > v1.last_seen_rate), "emergency mode really did clear");
        assert_eq!(staged1, staged_before_recovery,
                   "the post-maturity recovery must not credit the YT holder again");
        assert_eq!(v1.uncollected, vm.uncollected, "uncollected_sy did not grow on the recovery");
        assert!(p0001(&v1), "P-0001 after the post-maturity recovery: {v1:?}");
        assert_eq!(staged_at_peak, 50_000_000, "sanity: the peak credit really happened");

        // NOTE what DID move on that recovery, and is the subject of the next test:
        // treasury_sy went 0 -> 70,000,000 and sy_for_pt 105,000,000 -> 35,000,000.
        println!("  >>> treasury_sy {} -> {}, sy_for_pt {} -> {}",
                 vm.treasury, v1.treasury, vm.sy_for_pt, v1.sy_for_pt);
    }

    // -----------------------------------------------------------------------------------------
    // 5. Where the rate round trip DOES cost PT holders, and it is not `calc_earned_sy`.
    //
    //    `update_from_sy_state` hands post-maturity SY appreciation to the treasury
    //    (vault.rs:335-338). The GUARD is against the all-time high --
    //    `can_collect_sy_lambo` requires `cur > all_time_high` (vault.rs:257-263), and its comment
    //    says why: "This ATH check is a safety check, since SY can depreciate, which would cause
    //    problems for PT backing". But the AMOUNT is computed from `last_seen_sy_exchange_rate`
    //    (`calc_sy_surplus`, vault.rs:272-278), which is whatever the rate was at the last
    //    observation -- including a depressed one. So the climb back out of a dip is taxed as if
    //    it were fresh appreciation.
    //
    //    A/B: identical vaults, identical start rate and identical end rate. The only difference
    //    is whether the rate dipped and recovered before maturity.
    // -----------------------------------------------------------------------------------------
    #[test]
    fn a_pre_maturity_dip_lets_the_treasury_take_pt_holders_principal() {
        // A vault matured with the last observation at rate `mature_at_milli`, then the rate is
        // taken to 3.0 post-maturity. Returns (treasury_sy, sy_for_pt, SY the PT holder merges for).
        fn run(dip: bool) -> (u64, u64, u64, V) {
            let mut f = world();
            // Real yield first: 1.0 -> 2.0, staged and left in place. ATH = 2.0.
            assert!(f.action_set_sy_exchange_rate(2_000), "rate -> 2.0");
            assert!(f.action_stage_yt_yield(), "stage at 2.0");

            if dip {
                // The rate falls back to 1.0 while the vault is still ACTIVE. Only `withdraw_yt`
                // can observe it -- it is the one refreshing instruction with no emergency guard
                // (withdraw_yt.rs:181-199) -- and it writes `last_seen_sy_exchange_rate = 1.0`.
                assert!(f.action_set_sy_exchange_rate(1_000), "rate -> 1.0");
                assert!(f.withdraw_yt_exact(1), "withdraw_yt observes the dip");
            }
            // Maturity.
            assert!(f.action_advance_time(400), "cross maturity");
            // And then the rate reaches 3.0, above the ATH, so the lambo path opens.
            assert!(f.action_set_sy_exchange_rate(3_000), "rate -> 3.0");
            let o = stage(&mut f);
            assert!(o.is_success(), "stage after maturity: {:#?}", o.logs());

            let v = vault_of(&f);
            // The PT holder (actor 1) redeems. Post-maturity `merge` burns PT only
            // (merge.rs:265-268) and pays `pt_supply`'s share of `sy_for_pt` (merge.rs:236-243).
            f.action_select_actor(1);
            let before = f.ctx.token_balance(&f.ta_sy[1]);
            assert!(f.merge_exact(STRIP), "PT holder merge");
            let got = f.ctx.token_balance(&f.ta_sy[1]) - before;
            (v.treasury, v.sy_for_pt, got, v)
        }

        println!("\n=== lambo basis: monotone vs dip-and-recover ===");
        let (t_a, pt_a, got_a, v_a) = run(false);
        show("A monotone 2.0 -> 3.0", &v_a);
        println!("  A: treasury_sy={t_a} sy_for_pt={pt_a} PT holder merged 100000000 PT for {got_a} SY \
                  (= {} py at rate 3.0)", got_a as u128 * 3);
        let (t_b, pt_b, got_b, v_b) = run(true);
        show("B dip 2.0 -> 1.0 -> 3.0", &v_b);
        println!("  B: treasury_sy={t_b} sy_for_pt={pt_b} PT holder merged 100000000 PT for {got_b} SY \
                  (= {} py at rate 3.0)", got_b as u128 * 3);

        // Both worlds end at the same rate with the same escrow and the same uncollected YT claim.
        assert_eq!(v_a.escrow, v_b.escrow, "same SY in escrow");
        assert_eq!(v_a.uncollected, v_b.uncollected, "same YT claim");
        assert_eq!(v_a.last_seen_rate, 3 * ONE);
        assert_eq!(v_b.last_seen_rate, 3 * ONE);

        // The correct surplus is measured from the all-time high the guard itself uses.
        // active_sy before the lambo is the same in both: escrow - uncollected.
        let active = v_a.escrow - v_a.uncollected;
        let correct = ((3 * ONE - 2 * ONE) * active as u128 / (3 * ONE)) as u64;
        println!("  active_sy before the lambo = {active}; surplus measured from the ATH (2.0) \
                  would be {correct}");
        assert_eq!(t_a, correct, "world A takes exactly the ATH-based surplus");
        assert!(t_b > t_a,
                "the dip must have enlarged the treasury take: {t_b} vs {t_a}");
        println!("  treasury over-take from the dip = {} SY", t_b - t_a);
        println!("  PT holder loss from the dip     = {} SY = {} py",
                 got_a - got_b, (got_a - got_b) as u128 * 3);
        assert!(got_b < got_a, "the PT holder was paid less purely because of the dip");
        assert_eq!(t_b - t_a, pt_a - pt_b,
                   "every SY the treasury took beyond the ATH basis came out of sy_for_pt");

        // And the vault still reports itself solvent: `set_sy_for_pt` clamps `sy_for_pt` to
        // `active_sy`, so P-0001 cannot see this at all.
        assert!(p0001(&v_a) && p0001(&v_b), "P-0001 holds in BOTH worlds -- it is blind here");

        // P-0009 is the property written BECAUSE P-0001 is blind here: PT's backing, valued at the
        // vault's own rate, must be worth at least the PT supply. This is its liveness check --
        // it has to hold in the healthy world and fail in the damaged one, or it is not the net it
        // claims to be.
        let backed_py = |v: &V| v.sy_for_pt as u128 * v.last_seen_rate / ONE;
        println!("  P-0009: A backs {} py of {} py owed; B backs {} py of {} py owed",
                 backed_py(&v_a), v_a.pt_supply, backed_py(&v_b), v_b.pt_supply);
        assert!(backed_py(&v_a) + 4 >= v_a.pt_supply as u128,
                "P-0009 must HOLD in the healthy world: {} < {}",
                backed_py(&v_a), v_a.pt_supply);
        assert!(backed_py(&v_b) + 4 < v_b.pt_supply as u128,
                "P-0009 must FIRE in the damaged world: {} >= {}",
                backed_py(&v_b), v_b.pt_supply);
    }

    // -----------------------------------------------------------------------------------------
    // 6. The shortfall is PERMANENT. Each further post-maturity rise re-applies the same
    //    `active_sy * last_seen / cur` scaling, so a PT position that is short by a factor stays
    //    short by that same factor no matter how far the SY recovers.
    // -----------------------------------------------------------------------------------------
    /// ESCALATION pass on issue-05. Two chains, one refuted and one confirmed.
    ///
    /// **#11 scale/iteration amplification — REFUTED, by code.** The chain was "the drain repeats,
    /// so PT goes to zero". It cannot: `withdraw_yt` is the only refreshing instruction without an
    /// emergency guard, and it `require!`s `vault.is_active(now)` (`withdraw_yt.rs:90-92`). So a
    /// depressed `last_seen` can only be installed BEFORE maturity, while the treasury sweep only
    /// opens AFTER it (`can_collect_sy_lambo` requires `is_expired`, `vault.rs:257-263`). Every
    /// other post-maturity refresher either carries the emergency guard and reverts during a dip,
    /// or does not refresh at all (`collect_interest`). One shot, not a loop.
    ///
    /// **#10 config/value extreme — CONFIRMED.** The loss is not fixed at "half". The sweep takes
    /// `active_sy * (cur - last_seen) / cur`, so it scales with how DEEP the observed dip was, and
    /// the attacker only has to witness the low with a free `withdraw_yt(0)`. issue-05's PoC used a
    /// dip to 1.0 from an ATH of 2.0 and measured 50%. This sweeps the depth.
    #[test]
    fn escalation_the_pt_drain_scales_with_dip_depth() {
        println!("\n=== escalation: PT backing vs depth of the observed dip ===");
        let mut rows = vec![];
        for dip_milli in [1_000u32, 500, 100, 10, 1] {
            let mut f = ExponentCoreFixture::setup();
            f.action_select_actor(1);
            assert!(f.action_acquire_sy(500_000_000), "acquire_sy");
            assert!(f.strip_exact(200_000_000), "strip");

            // Rate rises to 2.0 honestly, then dips to `dip_milli` while the vault is ACTIVE.
            assert!(f.action_set_sy_exchange_rate(2_000), "rate -> 2.0");
            assert!(f.action_set_sy_exchange_rate(dip_milli), "dip -> {dip_milli}/1000");
            // One permissionless, zero-amount call observes the low.
            assert!(f.withdraw_yt_exact(0), "withdraw_yt observes the dip");
            assert!(f.action_advance_time(400), "cross maturity");
            // Recovery above the old ATH opens the sweep.
            assert!(f.action_set_sy_exchange_rate(3_000), "recover -> 3.0");
            let o = stage(&mut f);
            assert!(o.is_success(), "post-maturity refresh: {:#?}", o.logs());

            let v = vault_of(&f);
            let rate = f.sy_exchange_rate();
            let pct = (v.sy_for_pt as u128 * rate / ONE) * 100 / v.pt_supply.max(1) as u128;
            println!("  dip to {dip_milli:>5}/1000 -> treasury_sy={:<12} sy_for_pt={:<12} \
                      PT backed at {pct}% of face", v.treasury, v.sy_for_pt);
            rows.push((dip_milli, pct));
        }
        let shallow = rows[0].1;
        let deepest = rows.last().unwrap().1;
        println!("  shallowest dip leaves {shallow}% backing; deepest leaves {deepest}%");
        assert!(deepest <= shallow, "a deeper dip must not leave MORE backing");
        if deepest < 5 {
            println!("  >>> ESCALATION CONFIRMED: a deep dip leaves PT essentially unbacked");
        } else {
            println!("  >>> partial: the drain scales but is bounded well above zero");
        }
    }

    #[test]
    fn the_pt_shortfall_never_heals_however_far_the_rate_recovers() {
        let mut f = ExponentCoreFixture::setup();
        // No YT is ever deposited and nothing is ever staged, so `uncollected_sy` stays 0 and the
        // only thing moving `sy_for_pt` is the lambo path.
        f.action_select_actor(1);
        assert!(f.action_acquire_sy(500_000_000), "acquire_sy");
        assert!(f.strip_exact(200_000_000), "strip");
        let v0 = vault_of(&f);
        println!("\n=== permanence ===");
        show("start (rate 1.0)", &v0);
        assert_eq!(v0.uncollected, 0, "no YT interest anywhere in this world");

        // Dip to 0.5, observed by `withdraw_yt` -- the one refreshing instruction with no
        // emergency guard. From here `last_seen_sy_exchange_rate` is 0.5 and the ATH is 1.0.
        assert!(f.action_set_sy_exchange_rate(500), "rate -> 0.5");
        assert!(f.withdraw_yt_exact(0), "withdraw_yt observes the dip");
        show("dip (rate 0.5)", &vault_of(&f));
        assert!(f.action_advance_time(400), "cross maturity");

        // Every PT holder's py backing, as a percentage of what they are owed.
        let backing_pct = |f: &ExponentCoreFixture| -> u128 {
            let v = vault_of(f);
            let rate = f.sy_exchange_rate();
            // py value of the SY set aside for PT, over the py the PT supply represents.
            (v.sy_for_pt as u128 * rate / ONE) * 100 / v.pt_supply.max(1) as u128
        };

        for milli in [2_000u32, 4_000, 3_999] {
            assert!(f.action_set_sy_exchange_rate(milli), "rate -> {milli}/1000");
            let o = stage(&mut f);
            if !o.is_success() { println!("  stage at {milli}/1000 refused: {:?}", o.error_code()); }
            let v = vault_of(&f);
            println!("  rate={}/1000 treasury_sy={} sy_for_pt={} pt_supply={} \
                      -> PT backed at {}% of face", milli, v.treasury, v.sy_for_pt, v.pt_supply,
                     backing_pct(&f));
        }
        let end = backing_pct(&f);
        assert!(end <= 51,
                "PT should still be about half-backed however far the rate climbs, got {end}%");
        let v = vault_of(&f);
        show("end", &v);
        assert!(p0001(&v), "and P-0001 still holds throughout");
    }
}


/// COVERAGE DIAGNOSIS. Ten instructions have a generated action that has NEVER succeeded -- 569
/// attempts, zero successes, across a 400-sequence corpus replay. Several of them read as 100%
/// line-covered, which is the `ACTION_NEVER_SUCCEEDED` trap: the covered lines are the early-return
/// path or a shared handler, not the instruction's own logic.
///
/// This prints the REAL error for each, from a world set up the way an ordinary sequence would
/// leave it. No fix is guessed at until its error is on screen.
#[cfg(test)]
mod zz_coverage_diagnosis {
    use super::*;

    fn err(o: &crucible_test_context::TxOutcome) -> String {
        let logs = o.logs().join("\n");
        // The Anchor error line is the one that matters; fall back to the program failure line.
        for pat in ["AnchorError", "panicked at", "Error Code:", "failed:"] {
            if let Some(i) = logs.find(pat) {
                let tail = &logs[i..];
                return tail.lines().take(2).collect::<Vec<_>>().join(" / ");
            }
        }
        format!("(no error line) success={} logs={}", o.is_success(), logs.lines().count())
    }

    /// A world with liquidity, YT deposited, an emission stream and a matured-capable clock --
    /// i.e. the state these instructions are supposed to be reachable from.
    fn world() -> ExponentCoreFixture {
        let mut f = ExponentCoreFixture::setup();
        for a in [0u8, 1u8] {
            f.action_select_actor(a);
            assert!(f.action_acquire_sy(800_000_000), "acquire_sy {a}");
            assert!(f.strip_exact(200_000_000), "strip {a}");
        }
        f.action_select_actor(0);
        assert!(f.action_market_two_deposit_liquidity(), "seed liquidity");
        assert!(f.deposit_yt_exact(50_000_000), "deposit_yt");
        // Register EVERY stream: the mock SY keeps one global emission list, and the vault indexes
        // its own list by that list's length, so any gap panics (issue-02).
        for _ in 0..N_EMISSION_STREAMS {
            assert!(f.action_enable_emission(), "enable emission");
        }
        assert!(f.action_accrue_emission(500), "accrue emission");
        // Every position must be STAGED after the emission exists, or it still carries no tracker
        // and any instruction that touches it dies on issue-04's missing realloc (3004) rather
        // than on whatever we are actually trying to diagnose.
        for a in [0u8, 1u8] {
            f.action_select_actor(a);
            f.action_stage_yt_yield();
        }
        // The MARKET needs its own emission stream, or `market_collect_emission` indexes
        // `market.emissions.trackers[0]` on an empty vec and panics (`market_collect_emission.rs:34`).
        f.action_select_actor(0);
        assert!(f.action_add_market_emission(), "add_market_emission");
        // ...and the LP POSITION needs its own tracker for that stream, or
        // `market_collect_emission.rs:70` indexes an empty vec. Touching the position grows it.
        assert!(f.action_market_deposit_lp(), "market_deposit_lp grows the LP tracker vec");
        f
    }

    #[test]
    fn why_do_the_dead_actions_fail() {
        let mut f = world();
        f.action_select_actor(0);
        println!("\n=== dead-action diagnosis ===");
        if std::env::var("DIAG_FULL").is_ok() {
            for (n, k) in [
                ("sy_global", f.sy_global), ("sy_authority", f.sy_authority), ("sy_custody", f.sy_custody),
                ("base_custody", f.base_custody), ("vault_sy_position", f.vault_sy_position),
                ("market_sy_position", f.market_sy_position), ("vault", f.vault),
                ("vault_authority", f.vault_authority), ("market", f.market), ("alt", f.alt),
                ("escrow_sy", f.escrow_sy), ("escrow_yt", f.escrow_yt), ("mint_lp", f.mint_lp),
                ("market_escrow_pt", f.market_escrow_pt), ("market_escrow_sy", f.market_escrow_sy),
                ("market_escrow_lp", f.market_escrow_lp), ("sy_mint", f.sy_mint),
                ("base_mint", f.base_mint), ("mint_pt", f.mint_pt), ("mint_yt", f.mint_yt),
                ("token_treasury_fee_sy", f.token_treasury_fee_sy),
                ("vault_yield_position", f.vault_yield_position), ("ta_sy0", f.ta_sy[0]),
                ("ta_base0", f.ta_base[0]), ("ta_pt0", f.ta_pt[0]), ("ta_yt0", f.ta_yt[0]),
                ("ta_lp0", f.ta_lp[0]), ("lp_position0", f.lp_position[0]),
                ("yield_position0", f.yield_position[0]), ("user0", f.users[0].pubkey()),
            ] { println!("KEY {n:<24} {k}"); }
        }
        macro_rules! probe {
            ($name:literal, $call:expr) => {{
                let o = $call;
                println!("{:<32} ok={:<5} {}", $name, o.is_success(), err(&o));
                if std::env::var("DIAG_FULL").map(|v| v == $name || v == "1").unwrap_or(false) {
                    println!("--- full logs for {} ---\n{}\n---", $name, o.logs().join("\n"));
                }
            }};
        }
        probe!("buy_yt", f.diag_buy_yt());
        probe!("market_collect_emission", f.diag_market_collect_emission());
        probe!("collect_treasury_interest", f.diag_collect_treasury_interest());
        probe!("add_lp_tokens_metadata", f.diag_add_lp_tokens_metadata());
        probe!("initialize_yield_position", f.diag_initialize_yield_position());
        probe!("wrapper_buy_pt", f.diag_wrapper_buy_pt());
        probe!("wrapper_buy_yt", f.diag_wrapper_buy_yt());
        probe!("wrapper_collect_interest", f.diag_wrapper_collect_interest());
        probe!("wrapper_provide_liquidity", f.diag_wrapper_provide_liquidity());
        probe!("wrapper_provide_liquidity_base", f.diag_wrapper_provide_liquidity_base());
        println!("=== end ===\n");
    }
}


/// `buy_yt` and `wrapper_buy_yt` both die with "Access violation in heap section" after burning the
/// FULL 1.4M compute budget. That is not a slippage failure -- it is the curve solver failing to
/// terminate. Sweep `yt_out` to find whether ANY size works: if small ones do, it is a clamp; if
/// none do, the instruction is unreachable and that is a finding in itself.
#[cfg(test)]
mod zz_buy_yt_sweep {
    use super::*;

    #[test]
    fn what_size_of_buy_yt_actually_works() {
        let mut f = ExponentCoreFixture::setup();
        f.action_select_actor(0);
        assert!(f.action_acquire_sy(900_000_000), "acquire_sy");
        assert!(f.strip_exact(100_000_000), "strip");
        let market_pt = f.ctx.token_balance(&f.market_escrow_pt);
        println!("market_escrow_pt = {market_pt}");
        for div in [4u64, 16, 64, 256, 1024, 4096, 65536] {
            let yt_out = (market_pt / div).max(4);
            let sy_in = (yt_out / 4).max(1);
            let o = f.run_buy_yt(sy_in, yt_out);
            let logs = o.logs().join(" ");
            let why = ["Access violation", "panicked at", "Error Code:"].iter()
                .find_map(|p| logs.find(p).map(|i| logs[i..].chars().take(70).collect::<String>()))
                .unwrap_or_else(|| "-".into());
            println!("yt_out={yt_out:<12} (pt/{div:<6}) sy_in={sy_in:<12} ok={:<5} {why}", o.is_success());
        }
    }

    #[test]
    fn what_size_of_wrapper_provide_liquidity_base_works() {
        let mut f = ExponentCoreFixture::setup();
        f.action_select_actor(0);
        assert!(f.action_acquire_sy(900_000_000), "acquire_sy");
        assert!(f.strip_exact(100_000_000), "strip");
        let market_pt = f.ctx.token_balance(&f.market_escrow_pt);
        for div in [8u64, 64, 512, 4096, 32768] {
            let amt = (market_pt / div).max(1);
            let o = f.run_wpl_base(amt);
            let logs = o.logs().join(" ");
            let why = ["Access violation", "panicked at", "Error Code:", "failed:"].iter()
                .find_map(|p| logs.find(p).map(|i| logs[i..].chars().take(80).collect::<String>()))
                .unwrap_or_else(|| "-".into());
            println!("wpl_base amount={amt:<12} (pt/{div:<6}) ok={:<5} {why}", o.is_success());
        }
    }
}


#[cfg(test)]
mod zz_key_map {
    use super::*;
    #[test]
    fn print_fixture_keys() {
        let f = ExponentCoreFixture::setup();
        for (n, k) in [
            ("sy_global", f.sy_global), ("sy_authority", f.sy_authority), ("sy_custody", f.sy_custody),
            ("base_custody", f.base_custody), ("vault_sy_position", f.vault_sy_position),
            ("market_sy_position", f.market_sy_position), ("vault", f.vault),
            ("vault_authority", f.vault_authority), ("market", f.market), ("alt", f.alt),
            ("escrow_sy", f.escrow_sy), ("escrow_yt", f.escrow_yt), ("mint_lp", f.mint_lp),
            ("market_escrow_pt", f.market_escrow_pt), ("market_escrow_sy", f.market_escrow_sy),
            ("market_escrow_lp", f.market_escrow_lp), ("sy_mint", f.sy_mint),
            ("base_mint", f.base_mint), ("mint_pt", f.mint_pt), ("mint_yt", f.mint_yt),
            ("token_treasury_fee_sy", f.token_treasury_fee_sy),
            ("vault_yield_position", f.vault_yield_position),
            ("ta_sy[0]", f.ta_sy[0]), ("ta_base[0]", f.ta_base[0]), ("ta_pt[0]", f.ta_pt[0]),
            ("ta_yt[0]", f.ta_yt[0]), ("ta_lp[0]", f.ta_lp[0]), ("lp_position[0]", f.lp_position[0]),
            ("yield_position[0]", f.yield_position[0]), ("user0", f.users[0].pubkey()),
        ] { println!("KEY {n:<24} {k}"); }
    }
}


/// issue-07: `wrapper_provide_liquidity` declares `authority` read-only and then self-CPIs into
/// `strip`, which declares the same account `#[account(mut)]` ("Needs to be mutable to be used in
/// deposit_sy CPI", `strip.rs:24-27`). That is a writable-privilege escalation, which the Solana
/// runtime refuses, so the instruction cannot succeed for any input from any caller.
///
/// The control is what makes this a declaration bug rather than a caller mistake:
/// `wrapper_strip` performs the SAME `do_cpi_strip` and DOES declare `authority` mutable, and it
/// succeeds from the identical world.
#[cfg(test)]
mod zz_wrapper_provide_liquidity_dead {
    use super::*;

    fn world() -> ExponentCoreFixture {
        let mut f = ExponentCoreFixture::setup();
        f.action_select_actor(0);
        assert!(f.action_acquire_sy(800_000_000), "acquire_sy");
        assert!(f.strip_exact(100_000_000), "strip");
        assert!(f.action_market_two_deposit_liquidity(), "seed liquidity");
        f
    }

    #[test]
    fn wrapper_provide_liquidity_can_never_succeed() {
        let mut f = world();
        let authority = f.vault_authority;

        let o = f.diag_wrapper_provide_liquidity();
        let logs = o.logs().join("\n");
        println!("[wrapper_provide_liquidity] ok={}\n{}", o.is_success(), logs);
        assert!(!o.is_success(), "expected the escalation to kill the transaction");
        assert!(logs.contains("writable privilege escalated"),
                "expected a writable-privilege escalation, got:\n{logs}");
        assert!(logs.contains(&authority.to_string()),
                "the escalated account must be the VAULT AUTHORITY ({authority}), got:\n{logs}");

        // CONTROL: the same self-CPI into `strip`, from a wrapper that declares `authority` mut.
        // Same world, same accounts, same CPI -- the only difference is the declaration.
        let mut g = world();
        let ok = g.action_wrapper_strip();
        println!("[control wrapper_strip] ok={ok}");
        assert!(ok, "wrapper_strip does the same do_cpi_strip and MUST succeed -- if it does not, \
                     the difference is not the `mut` declaration and this finding is wrong");
    }
}


/// issue-08: the MARKET has the same unchecked positional zip over third-party data that the vault
/// has (issue-02). `MarketTwo::update_emissions_from_position_state` (`market_two.rs:322-336`)
/// iterates the SY PROGRAM's emission list and indexes the MARKET's own tracker list:
///
///     for (index, current_position) in position_state.emissions.iter().enumerate() {
///         let difference = current_position.amount_claimable
///                        - self.emissions.trackers[index].last_seen_staged;
///
/// so one reward stream the market has not registered panics every market instruction that
/// refreshes. The subtraction on the same line is a bare u64 subtraction with no saturation.
#[cfg(test)]
mod zz_market_emission_length_mismatch {
    use super::*;

    #[test]
    fn an_unregistered_sy_stream_bricks_the_market() {
        let mut f = ExponentCoreFixture::setup();
        f.action_select_actor(0);
        assert!(f.action_acquire_sy(800_000_000), "acquire_sy");
        assert!(f.strip_exact(200_000_000), "strip");
        assert!(f.action_market_two_deposit_liquidity(), "seed liquidity");

        // BASELINE: the market is healthy, so any later failure is caused by the step between.
        assert!(f.action_market_deposit_lp(), "baseline market_deposit_lp must succeed");
        println!("[baseline] market_deposit_lp ok");

        let market = f.ctx.read_anchor_account::<exponent_core::state::MarketTwo>(&f.market)
            .expect("market");
        let trackers_before = market.emissions.trackers.len();
        println!("[baseline] market emission trackers = {trackers_before}");

        // Stream 0 appears and the market IS told about it, so the two lists agree at length 1.
        // `action_enable_emission` deliberately keeps the market in lockstep (see its comment), so
        // it is the right call for the healthy leg.
        assert!(f.action_enable_emission(), "stream 0 on the SY program, the vault and the market");
        assert!(f.action_market_deposit_lp(), "still healthy while the lists agree");
        println!("[agreed] market has 1 tracker, SY has 1 stream -- deposit_lp ok");

        // Now a SECOND stream appears on the SY program and the vault, and the market is NOT told.
        // The lockstep helper is bypassed on purpose: this is the state a third-party SY program
        // puts the market into, and nothing in the program makes the market check.
        assert!(f.mock_sy_add_emission_index(0, f.emission_mints[1]),
                "stream 1 on the SY program");
        let o = f.run_add_emission_stream(1, 0);
        assert!(o.is_success(), "stream 1 on the vault: {:#?}", o.logs());
        // Touch the market's SY position so its emission list actually grows to 2.
        assert!(f.action_market_two_deposit_liquidity(), "sync the market's SY position");

        let o = f.diag_market_withdraw_lp_probe();
        let logs = o.logs().join("\n");
        println!("[after] market_deposit_lp ok={}\n{}", o.is_success(), logs);
        assert!(!o.is_success(), "the market must now be bricked");
        assert!(logs.contains("market_two.rs:329") || logs.contains("index out of bounds"),
                "expected the positional-zip panic, got:\n{logs}");

        // RECOVERY: registering the stream on the market clears it.
        assert!(f.action_add_market_emission(), "add_market_emission");
        assert!(f.action_market_deposit_lp(), "market_deposit_lp works again once registered");
        println!("[recovery] market_deposit_lp ok");
    }
}


/// `initialize_yield_position` coverage.
///
/// `setup()` pre-creates positions for actors `0..N_USERS-1` so the value-flow actions work from
/// iteration 0, and deliberately leaves the LAST actor's uncreated so this instruction still has
/// something real to do. The generated action ranks LOW, so it is `#[cfg]`-stubbed out of the fuzz
/// build (`// disabled: build with --features admin_actions to enable`) and can never succeed
/// there -- which is why the gate reads 0/3 rather than a coverage number.
///
/// This test drives the real instruction end to end for the actor that has no position, and is the
/// evidence behind `scout credit initialize_yield_position`.
#[cfg(test)]
mod zz_initialize_yield_position {
    use super::*;

    #[test]
    fn the_last_actor_can_initialize_their_own_yield_position() {
        let mut f = ExponentCoreFixture::setup();
        let last = N_USERS - 1;
        let pos = f.yield_position[last];

        // Pre-state: the account genuinely does not exist, so a success below is a real init.
        assert!(f.ctx.account_data(&pos).map(|d| d.is_empty()).unwrap_or(true),
                "the last actor's position must not exist yet");
        println!("[before] position {pos} does not exist");

        f.action_select_actor(last as u8);
        // Call the REAL instruction through the same helper `setup()` uses, not the generated
        // action: without `--features admin_actions` that action is the `#[cfg]` stub and returns
        // `false` unconditionally, so asserting on it would fail in the default build (which is
        // exactly the build `scout verify` runs).
        let owner = f.users[last].clone();
        let (pid, vault_key) = (f.program_id, f.vault);
        ExponentCoreFixture::run_initialize_yield_position(&mut f.ctx, pid, &owner, vault_key, pos);

        // Post-state read back through TestContext: the account exists, is owned by the program,
        // and carries the vault it was opened against.
        let d = f.ctx.account_data(&pos).expect("position must exist now");
        assert!(!d.is_empty(), "position account is empty after init");
        let vault_field = Pubkey::new_from_array(d[40..72].try_into().unwrap());
        println!("[after] position exists, len={} vault={}", d.len(), vault_field);
        assert_eq!(vault_field, f.vault, "position must point at the vault it was opened against");

        // And it is genuinely usable, not just allocated.
        assert!(f.action_acquire_sy(200_000_000), "acquire_sy");
        assert!(f.strip_exact(50_000_000), "strip");
        assert!(f.deposit_yt_exact(10_000_000), "deposit_yt into the freshly created position");
        assert_eq!(f.position_yt_balance(last), 10_000_000, "the new position holds the YT");
        println!("[after] deposit_yt into the new position ok");
    }
}


/// Permissionlessness check: `stage_yt_yield` is constrained by `has_one = vault` on
/// `user_yield_position` and NOT by owner (`stage_yield.rs:23-31`), and its signer is a bare
/// `payer`. So any account can force any position to accrue. Asserted rather than read off the
/// constraint list, because "this account is not owner-gated" is exactly the kind of claim that
/// should not go into a writeup unmeasured.
#[cfg(test)]
mod zz_permissionless_stage {
    use super::*;

    #[test]
    fn any_account_can_stage_another_actors_position() {
        let mut f = ExponentCoreFixture::setup();

        // Actor 0 holds the position and the YT.
        f.action_select_actor(0);
        assert!(f.action_acquire_sy(500_000_000), "acquire_sy");
        assert!(f.strip_exact(200_000_000), "strip");
        assert!(f.deposit_yt_exact(200_000_000), "deposit_yt");
        let victim = f.yield_position[0];

        // Real yield appears.
        assert!(f.action_set_sy_exchange_rate(2_000), "rate -> 2.0");
        let before = f.position_interest_staged(0);

        // Actor 1 -- who owns nothing in that position -- stages it. The instruction is built
        // against `self.actor`'s own position, so drive the raw call with actor 1 as payer and
        // actor 0's position as the user position.
        let stranger = f.users[1].insecure_clone();
        let payer_kp = f.payer.clone();
        let (event_authority, _) =
            Pubkey::find_program_address(&[b"__event_authority"], &f.program_id);
        let acc = accounts::StageYtYield {
            payer: stranger.pubkey(),
            vault: f.vault,
            user_yield_position: victim,
            yield_position: f.vault_yield_position,
            sy_program: f.sy_program_id,
            address_lookup_table: f.alt,
            system_program: system_program::ID,
            event_authority,
            program: f.program_id,
        };
        let metas = vec![
            AccountMeta::new(f.sy_global, false),
            AccountMeta::new(f.vault_sy_position, false),
            AccountMeta::new(f.sy_custody, false),
            AccountMeta::new_readonly(f.sy_authority, false),
        ];
        let pid = f.program_id;
        let o = f.ctx.program(pid)
            .call(instruction::StageYtYield {})
            .accounts(acc)
            .remaining_accounts_metas(metas)
            .signers(&[&*payer_kp, &stranger])
            .send()
            .expect("send");
        let after = f.position_interest_staged(0);
        println!("[stranger stages victim] ok={} staged {before} -> {after}", o.is_success());
        assert!(o.is_success(), "a stranger must be able to stage it: {:#?}", o.logs());
        assert!(after > before, "the victim's position accrued, driven by an account that owns none of it");
    }
}


/// ESCALATION pass, Mode B probes for issue-01 and issue-06.
#[cfg(test)]
mod zz_escalation_emissions {
    use super::*;

    /// issue-01 + **#11 scale/iteration amplification**: the over-credit is
    /// `full_index * balance` per stream. With N streams each carrying history, does a single
    /// holder's total claim scale with N? If so the cap is "however many reward programmes the
    /// vault has adopted", not one stream.
    #[test]
    fn escalation_issue01_over_credit_scales_with_stream_count() {
        let mut f = ExponentCoreFixture::setup();
        f.action_select_actor(0);
        assert!(f.action_acquire_sy(500_000_000), "acquire_sy");
        assert!(f.strip_exact(200_000_000), "strip");
        assert!(f.deposit_yt_exact(200_000_000), "deposit_yt");

        const HISTORY: u128 = 5 * NUMBER_ONE;
        let mut totals = vec![];
        for s in 0..N_EMISSION_STREAMS {
            assert!(f.mock_sy_add_emission_index(HISTORY, f.emission_mints[s]),
                    "register stream {s} WITH history");
            let o = f.run_add_emission_stream(s, 0);
            assert!(o.is_success(), "add_emission {s}: {:#?}", o.logs());
            assert!(f.action_stage_yt_yield(), "stage after stream {s}");
            let total: u64 = (0..=s)
                .map(|i| f.position_staged_emission(&f.yield_position[0], i))
                .sum();
            println!("  after {} stream(s): holder's TOTAL staged across streams = {total}", s + 1);
            totals.push(total);
        }
        let (first, last) = (totals[0], *totals.last().unwrap());
        println!("  1 stream = {first}; {} streams = {last}", N_EMISSION_STREAMS);
        if last > first {
            println!("  >>> ESCALATION CONFIRMED: the over-credit scales with the number of streams");
        } else {
            println!("  >>> chain REFUTED: additional streams do not increase the holder's claim");
        }
    }

    /// issue-06 + **#10 config/value extreme**: the treasury re-sweep is
    /// `(cur - rewound) * escrow`. How much can one rewind take? Sweep the rewind depth.
    #[test]
    fn escalation_issue06_treasury_resweep_scales_with_rewind_depth() {
        println!("\n=== escalation: treasury emission re-sweep vs rewind depth ===");
        for rewind_num in [1u128, 2, 10, 1000] {
            let mut f = ExponentCoreFixture::setup();
            f.action_select_actor(0);
            assert!(f.action_acquire_sy(500_000_000), "acquire_sy");
            assert!(f.strip_exact(200_000_000), "strip");
            assert!(f.deposit_yt_exact(200_000_000), "deposit_yt");
            assert!(f.action_enable_emission(), "enable stream 0");
            assert!(f.mock_sy_set_emission_index(0, NUMBER_ONE), "index -> 1.0");
            assert!(f.action_stage_yt_yield(), "observe 1.0");
            assert!(f.action_advance_time(400), "cross maturity");
            // Rewind to 1/rewind_num of the honest index, then climb back to 2.0.
            assert!(f.mock_sy_set_emission_index(0, NUMBER_ONE / rewind_num), "rewind");
            assert!(f.action_stage_yt_yield(), "observe the rewind");
            assert!(f.mock_sy_set_emission_index(0, 2 * NUMBER_ONE), "index -> 2.0");
            assert!(f.action_stage_yt_yield(), "observe 2.0");
            let v = f.ctx.read_anchor_account::<exponent_core::state::Vault>(&f.vault).expect("vault");
            let treas = v.emissions[0].treasury_emission;
            let escrow = v.total_sy_in_escrow;
            println!("  rewind to 1/{rewind_num:<5} of the index -> treasury_emission={treas:<14} \
                      (stream will ever pay {})", 2 * escrow);
        }
    }
}


/// VERIFICATION of three claims added when the reports were rewritten. Each is either new or
/// contradicts something previously measured, so each is checked rather than assumed.
#[cfg(test)]
mod zz_report_claim_check {
    use super::*;

    /// issue-07 claims: "A caller that manually marks the outer account writable can succeed."
    /// The generated client marks `authority` read-only because the struct lacks `#[account(mut)]`,
    /// but the RUNTIME privilege comes from the transaction's account metas, not from Anchor. This
    /// builds the instruction by hand with account 1 (`authority`) WRITABLE and nothing else
    /// changed. If it succeeds, the finding is genuinely narrowed to generated clients.
    #[test]
    fn claim_issue07_manual_writable_authority_succeeds() {
        let mut f = ExponentCoreFixture::setup();
        f.action_select_actor(0);
        assert!(f.action_acquire_sy(800_000_000), "acquire_sy");
        assert!(f.strip_exact(100_000_000), "strip");
        assert!(f.action_market_two_deposit_liquidity(), "seed liquidity");

        let a = f.actor;
        let depositor = f.users[a].insecure_clone();
        let amount_base = (f.ctx.token_balance(&f.market_escrow_pt) / 64).max(1);
        let mut data = vec![28u8];                       // wrapper_provide_liquidity
        data.extend_from_slice(&amount_base.to_le_bytes());
        data.extend_from_slice(&0u64.to_le_bytes());     // min_lp_out
        data.push(MINT_SY_ACCOUNTS);                     // mint_base_accounts_until
        let (ea, _) = Pubkey::find_program_address(&[b"__event_authority"], &f.program_id);

        // IDL order. Index 1 is `authority` -- marked WRITABLE here, which is the whole experiment.
        let mut metas = vec![
            AccountMeta::new(depositor.pubkey(), true),
            AccountMeta::new(f.vault_authority, false),          // <-- writable, not readonly
            AccountMeta::new(f.vault, false),
            AccountMeta::new(f.market, false),
            AccountMeta::new(f.market_escrow_pt, false),
            AccountMeta::new(f.market_escrow_sy, false),
            AccountMeta::new(f.ta_lp[a], false),
            AccountMeta::new(f.mint_lp, false),
            AccountMeta::new(f.ta_sy[a], false),
            AccountMeta::new(f.escrow_sy, false),
            AccountMeta::new(f.ta_yt[a], false),
            AccountMeta::new(f.ta_pt[a], false),
            AccountMeta::new(f.mint_yt, false),
            AccountMeta::new(f.mint_pt, false),
            AccountMeta::new_readonly(SPL_TOKEN_ID, false),
            AccountMeta::new_readonly(f.alt, false),
            AccountMeta::new_readonly(f.alt, false),
            AccountMeta::new_readonly(f.sy_program_id, false),
            AccountMeta::new(f.yield_position[a], false),
            AccountMeta::new(f.escrow_yt, false),
            AccountMeta::new(f.market_escrow_lp, false),
            AccountMeta::new(f.lp_position[a], false),
            AccountMeta::new(f.vault_yield_position, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(ea, false),
            AccountMeta::new_readonly(f.program_id, false),
        ];
        metas.extend(f.wrapper_mint_metas());
        metas.extend(f.sy_cpi_metas_full());
        metas.push(AccountMeta::new(f.market_sy_position, false));

        let ix = Instruction { program_id: f.program_id, accounts: metas, data };
        let payer = f.payer.clone();
        let o = f.ctx.raw_call(ix).signers(&[&*payer, &depositor]).send().expect("send");
        let logs = o.logs().join("\n");
        println!("[issue-07 claim] manual-writable authority ok={}", o.is_success());
        if !o.is_success() {
            let esc = logs.contains("writable privilege escalated");
            println!("  still escalating? {esc}\n  {}",
                     logs.lines().rev().take(3).collect::<Vec<_>>().join(" / "));
        }
        println!("  >>> claim '{}' ", if o.is_success() {
            "a manually-writable caller CAN succeed' HOLDS"
        } else { "a manually-writable caller CAN succeed' IS NOT SUPPORTED by this run" });
    }

    /// issue-08 claims the `N = 1, M = 0` case fails. Previously MEASURED: with one SY stream and
    /// zero market trackers, `market_deposit_lp` SUCCEEDED -- because the market's SY position had
    /// not yet grown its emission list, so the loop ran zero times. The claim is only true once the
    /// POSITION carries an entry. This pins which reading is right.
    #[test]
    fn claim_issue08_one_stream_zero_trackers() {
        let mut f = ExponentCoreFixture::setup();
        f.action_select_actor(0);
        assert!(f.action_acquire_sy(800_000_000), "acquire_sy");
        assert!(f.strip_exact(200_000_000), "strip");
        assert!(f.action_market_two_deposit_liquidity(), "seed liquidity");

        // One stream on the SY program and the vault; the MARKET is deliberately not told.
        assert!(f.mock_sy_add_emission_index(0, f.emission_mints[0]), "SY stream 0");
        let o = f.run_add_emission_stream(0, 0);
        assert!(o.is_success(), "vault add_emission: {:#?}", o.logs());

        let market = f.ctx.read_anchor_account::<exponent_core::state::MarketTwo>(&f.market).unwrap();
        println!("[issue-08 claim] market trackers M={}", market.emissions.trackers.len());

        let immediate = f.action_market_deposit_lp();
        println!("  deposit_lp immediately after the stream appears: ok={immediate}");

        // Now force the market's SY position to sync, so N becomes 1 while M is still 0.
        assert!(f.action_market_two_deposit_liquidity(), "sync the market's SY position");
        let after_sync = f.action_market_deposit_lp();
        println!("  deposit_lp after the market's SY position syncs:  ok={after_sync}");
        println!("  >>> N=1,M=0 fails only once the POSITION has the entry: immediate={immediate} \
                  after_sync={after_sync}");
    }

    /// issue-06 now cites "user, robot, and treasury commitments total 525m against 420m". The
    /// figure previously measured was 515m (user + treasury only), i.e. the rewrite adds the vault's
    /// robot position. This measures all three.
    #[test]
    fn claim_issue06_commitment_total_includes_robot() {
        let mut f = ExponentCoreFixture::setup();
        f.action_select_actor(0);
        assert!(f.action_acquire_sy(500_000_000), "acquire_sy");
        assert!(f.strip_exact(200_000_000), "strip");
        assert!(f.deposit_yt_exact(200_000_000), "deposit_yt");
        assert!(f.action_enable_emission(), "enable stream 0");
        assert!(f.mock_sy_set_emission_index(0, NUMBER_ONE), "index -> 1.0");
        assert!(f.action_stage_yt_yield(), "observe 1.0");
        assert!(f.action_advance_time(400), "cross maturity");
        assert!(f.mock_sy_set_emission_index(0, NUMBER_ONE / 2), "rewind -> 0.5");
        assert!(f.action_stage_yt_yield(), "observe the rewind");
        assert!(f.mock_sy_set_emission_index(0, 2 * NUMBER_ONE), "index -> 2.0");
        assert!(f.action_stage_yt_yield(), "observe 2.0");

        let v = f.ctx.read_anchor_account::<exponent_core::state::Vault>(&f.vault).unwrap();
        let treasury = v.emissions[0].treasury_emission;
        let user: u64 = (0..N_USERS).map(|i| f.position_staged_emission(&f.yield_position[i], 0)).sum();
        let robot = f.position_staged_emission(&f.vault_yield_position, 0);
        let ever = 2 * v.total_sy_in_escrow;
        println!("[issue-06 claim] treasury={treasury} user={user} robot={robot}");
        println!("  total committed = {} ; stream will ever pay = {ever} ; shortfall = {}",
                 treasury + user + robot,
                 (treasury + user + robot) as i128 - ever as i128);
    }
}

// // #[cfg(test)] mod ... — fine-grained TestContext assertions per stage
// SCOUT:TESTS:END
// GENERATED by crucible-scout gen_actions.py — edit setup glue + SCOUT-TODOs by hand.
use crucible_test_context::*;
use crucible_fuzzer::anchor_lang::system_program;
use crucible_fuzzer::*;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use std::rc::Rc;

// SCOUT:CHECK-CONTRACT:BEGIN sha256=c4b20795d13638b9cbca54acc8669b4394eb8494fe1116eb26b75f0b968aaf9e
// Semantic invariant checks have two modes:
//   default / SCOUT_CHECK_MODE=enforce: record a real Crucible fuzz violation;
//   SCOUT_CHECK_MODE=observe: emit nonce-bound reachability markers, never a violation.
// This exact alias is part of the trusted contract.  Generated setup and the
// macros below use `crate::`/`$crate` paths so a mutable prelude cannot replace
// Crucible's TestContext or violation/session functions with local lookalikes.
#[doc(hidden)]
extern crate crucible_test_context as __scout_crucible_test_context;

fn __scout_check_observe_mode() -> bool {
    std::env::var("SCOUT_CHECK_MODE").as_deref() == Ok("observe")
}

// Mute a property whose finding is already investigated and written up. Such a property keeps
// firing on the SAME known defect and floods the objective, hiding every other property's first
// finding behind thousands of duplicates -- observed at ~160 crashes per 25s on one target.
//
// Muting is ALWAYS announced on stderr, once per process. A silently disabled check is the exact
// false-negative trap this pipeline exists to avoid: a muted property is indistinguishable from a
// passing one unless the run says so out loud. `SCOUT_CHECK_MUTE` is also stripped from ordinary
// fuzz subprocesses alongside the other audit switches, so a stray shell variable can never
// quietly disable a check -- a caller must pass it explicitly.
fn __scout_check_announce_mutes(list: &str) {
    static MUTE_ONCE: std::sync::Once = std::sync::Once::new();
    MUTE_ONCE.call_once(|| {
        eprintln!("[SCOUT_CHECK_MUTED] {}", list);
    });
}

fn __scout_check_muted(property: &str) -> bool {
    match std::env::var("SCOUT_CHECK_MUTE") {
        Ok(list) => {
            let muted = list.split(',').any(|entry| entry.trim() == property);
            if muted {
                __scout_check_announce_mutes(&list);
            }
            muted
        }
        Err(_) => false,
    }
}

fn __scout_check_selected(property: &str) -> bool {
    if __scout_check_muted(property) {
        return false;
    }
    match std::env::var("SCOUT_CHECK_ONLY") {
        Ok(selected) => selected == property,
        Err(_) => true,
    }
}

fn __scout_check_nonce() -> Result<String, &'static str> {
    let nonce = std::env::var("SCOUT_CHECK_RUN")
        .map_err(|_| "missing or non-Unicode SCOUT_CHECK_RUN")?;
    if nonce.is_empty() {
        return Err("empty SCOUT_CHECK_RUN");
    }
    if !nonce.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b':' | b'-')
    }) {
        return Err("SCOUT_CHECK_RUN contains unsafe characters");
    }
    Ok(nonce)
}

fn __scout_check_emit_error(reason: &str) {
    static ERROR_ONCE: std::sync::Once = std::sync::Once::new();
    ERROR_ONCE.call_once(|| {
        // Never echo an invalid value: whitespace/newlines would forge protocol fields.
        eprintln!("[SCOUT_CHECK_ERROR] INVALID {}", reason);
    });
}

macro_rules! scout_check_session {
    () => {{
        if $crate::__scout_check_observe_mode() {
            // Coverage-only replay runs before Crucible's stateful initializer.  Set
            // this per-thread flag here so failed actions terminate accumulated chains
            // exactly as they did in the stateful campaign that produced the corpus.
            $crate::__scout_crucible_test_context::set_stateful_chain_mode(true);
            static SESSION_ONCE: std::sync::Once = std::sync::Once::new();
            SESSION_ONCE.call_once(|| {
                match $crate::__scout_check_nonce() {
                    Ok(nonce) => eprintln!("[SCOUT_CHECK_SESSION] {}", nonce),
                    Err(reason) => $crate::__scout_check_emit_error(reason),
                }
            });
        }
    }};
}

// Gate the *entire* property computation, not only its final predicate.  This
// prevents another property's fallible reads, eligibility logic, or shadow-hook
// arithmetic from panicking/starving an isolated SCOUT_CHECK_ONLY replay.
macro_rules! scout_run_property {
    ($property:literal, $expression:expr $(,)?) => {{
        if $crate::__scout_check_selected($property) {
            let _ = $expression;
        }
    }};
}

#[doc(hidden)]
#[macro_export]
macro_rules! __scout_check_impl {
    ($property:literal, $site:literal, $predicate:expr, $message:expr) => {{
        let __scout_observe = $crate::__scout_check_observe_mode();
        if !$crate::__scout_check_selected($property) {
            true
        } else {
            let __scout_nonce = if __scout_observe {
                Some($crate::__scout_check_nonce())
            } else {
                None
            };
            if let Some(Err(ref __scout_error)) = __scout_nonce {
                // An invalid session can never produce an EVALUATED marker.  The
                // mechanical verifier therefore cannot mistake it for sound evidence.
                $crate::__scout_check_emit_error(__scout_error);
                false
            } else {
                // Keep the predicate in one lexical/runtime position.  Expressions
                // with reads or counters are evaluated exactly once per selected check.
                let __scout_check_result: bool = $predicate;
                if let Some(Ok(ref __scout_run)) = __scout_nonce {
                    eprintln!(
                        "[SCOUT_CHECK_EVALUATED] {} {} {} {}:{}",
                        __scout_run, $property, $site, file!(), line!()
                    );
                    if !__scout_check_result {
                        eprintln!(
                            "[SCOUT_CHECK_WOULD_VIOLATE] {} {} {} {}:{}",
                            __scout_run, $property, $site, file!(), line!()
                        );
                    }
                } else if !__scout_check_result {
                    $crate::__scout_crucible_test_context::record_violation($message);
                }
                __scout_check_result
            }
        }
    }};
}

macro_rules! scout_check {
    ($property:literal, $site:literal, $predicate:expr $(,)?) => {{
        $crate::__scout_check_impl!(
            $property,
            $site,
            $predicate,
            format!(
                "Invariant {} check {} failed at {}:{}",
                $property, $site, file!(), line!()
            )
        )
    }};
    ($property:literal, $site:literal, $predicate:expr, $($arg:tt)+) => {{
        $crate::__scout_check_impl!($property, $site, $predicate, format!($($arg)+))
    }};
}
// SCOUT:CHECK-CONTRACT:END

const SCOUT_TARGET_PROGRAM_ARTIFACT: &str = "programs/exponent_core.so";




// SCOUT:BINDINGS:BEGIN
// Shared world handles built by setup().
// authority = self.vault_authority
// vault = self.vault
// escrow_sy = self.escrow_sy
// escrow_yt = self.escrow_yt
// mint_yt = self.mint_yt
// mint_pt = self.mint_pt
// mint_sy = self.sy_mint
// token_program = SPL_TOKEN_ID
// address_lookup_table = self.alt
// sy_program = self.sy_program_id
// program = self.program_id
// admin = self.admin_account
// admin_state = self.admin_account
// treasury_token_account = self.treasury_sy_ta
// treasury_sy_token_account = self.treasury_sy_ta
//
// `yield_position` is the VAULT's own robot position everywhere (pinned by `has_one =
// yield_position`); the caller's own position is spelled `user_yield_position` / `position`.
// The one exception is initialize_yield_position, whose `yield_position` IS the init target --
// hence the instruction-qualified override.
// yield_position = self.vault_yield_position
// user_yield_position = self.yield_position[self.actor]
// position = self.yield_position[self.actor]
// InitializeYieldPosition.yield_position = self.yield_position[self.actor]
// collect_interest is the other exception: its `yield_position` carries `has_one = owner` against
// the Signer, so it is the CALLER's position, not the vault robot
// (`vault/collect_interest.rs:25-30`). collect_treasury_interest's IS the robot position
// (`vault` carries `has_one = yield_position` there), so it keeps the shared binding.
// CollectInterest.yield_position = self.yield_position[self.actor]
// THE SHARED ROOT CAUSE behind most of the dead market/wrapper actions. `add_market_emission`'s
// handler does `self.market.cpi_accounts = cpi_accounts` (`add_market_emission.rs:51-52`) -- it
// OVERWRITES the market's entire CPI account table with whatever it is handed. The generator left
// it as `Default::default()`, i.e. EMPTY, so the moment this action succeeded the market lost
// `get_position_state`, `deposit_sy` and `withdraw_sy`, and every later market instruction failed
// with "insufficient account keys for instruction".
//
// It succeeds often (the gate reports 100%, 8/8), so in a real campaign it bricked the market
// partway through nearly every sequence. That is why buy_yt (43 attempts), wrapper_buy_yt (166),
// wrapper_buy_pt, wrapper_provide_liquidity(_base) and market_collect_emission (67) had ZERO
// successes between them while their line coverage read as high.
// AddMarketEmission.cpi_accounts = Self::market_cpi_accounts()
// The buy/provide amounts were derived from the ACTOR'S WALLET, which has nothing to do with what
// the market can absorb: a user holds ~1e12 base, the market holds ~1e7 PT, so `b / 64` asked for
// roughly 1500x the entire pool and `time_curve` panicked on its own precondition
// (`math.rs:270`, `assert!(net_trader_pt < 0 || market_pt > net_trader_pt)`; `math.rs:279`,
// "Asset cannot be worth less than PT"). Clamp to the MARKET, the way `action_trade_pt_clamped`
// already does -- this is the same lesson as the strip/merge amount clamps in CLAUDE.md.
// `wrapper_collect_interest` self-CPIs into `collect_interest`, whose `yield_position` carries
// `has_one = owner`. Bound to the VAULT's robot position it fails ConstraintHasOne (2001) every
// time -- measured, 33 attempts 0 successes. It must be the CLAIMER's own position.
// WrapperCollectInterest.yield_position = self.yield_position[self.actor]
// `add_lp_tokens_metadata` checks its signer against an admin principle
// (`exponent_admin/src/lib.rs:314`); bound to a plain user it is Unauthorized (6000) every time --
// measured, 77 attempts 0 successes. The payer must be an admin, and the fixture seeds `payer`
// into all six roles.
// AddLpTokensMetadata.payer = self.payer.pubkey()
// `collect_treasury_interest` CPIs `withdraw_sy`, whose ALT slots are
// (sy_global, vault_sy_position, sy_custody, escrow_sy, sy_authority, token_program). The generated
// four were not enough -- measured, "An account required by the instruction is missing". Supplying
// the full set is safe because `do_withdraw_sy` filters the combined pool by key and drops anything
// it does not need.
//
// Actor-scoped: whichever user action_select_actor last chose. Routing every user-facing
// instruction through a selectable actor is what lets a property distinguish "the adversary got
// richer" from "everyone got richer because the vault genuinely earned yield".
// depositor = signer: self.users[self.actor].insecure_clone()
// owner = signer: self.users[self.actor].insecure_clone()
// payer = signer: self.users[self.actor].insecure_clone()
// sy_src = self.ta_sy[self.actor]
// sy_dst = self.ta_sy[self.actor]
// token_sy_dst = self.ta_sy[self.actor]
// yt_src = self.ta_yt[self.actor]
// yt_dst = self.ta_yt[self.actor]
// pt_src = self.ta_pt[self.actor]
// pt_dst = self.ta_pt[self.actor]
//
// Admin-gated instructions must be signed by the hot admin (the payer), not by an actor.
// signer = signer: self.payer.insecure_clone()
// fee_payer = signer: self.payer.insecure_clone()
//
// Every instruction that CPIs the SY program needs sy_global / the vault's SY position / SY
// custody in remaining_accounts: `do_deposit_sy`/`do_withdraw_sy` filter the combined account pool
// down to whatever `Vault.cpi_accounts` names, so an account it names that is in neither the
// instruction's own list nor remaining_accounts is simply absent and the CPI fails.
// `sy_authority` is required by the WITHDRAW side (withdraw_sy/claim_emission sign the transfer out
// of custody with it); omitting it made merge fail with 'An account required by the instruction is
// missing' and withdraw_yt fault.
// --- amount clamps: derive sizes from LIVE STATE, not from a raw fuzzer u64 -------------------
// Measured necessity, not a style choice. With the generated `amount: u64` left unbound, the
// fuzzer draws from the whole u64 range while an actor holds at most ~1e9 tokens, so essentially
// every draw exceeds the balance and the instruction fails at the transfer. Over 650k iterations
// the gate reported ACTION_NEVER_SUCCEEDED for strip, merge, deposit_yt, withdraw_yt,
// stage_yt_yield, sell_yt and trade_pt -- all of which succeed when called by hand. Clamping moves
// the fuzzer's leverage from "pick a number that is almost always invalid" to "pick a SEQUENCE",
// which is where the stateful bugs are anyway.
// Argument EXTREMES are not lost: `action_*_edge` in SCOUT:EXTRA-ACTIONS still passes raw
// fuzzer-chosen amounts (0, 1, u64::MAX) at the same instructions.
// Strip.amount = { let b = self.ctx.token_balance(&self.ta_sy[self.actor]); (b / 4).max(1) }
// Merge.amount = { let p = self.ctx.token_balance(&self.ta_pt[self.actor]); let y = self.ctx.token_balance(&self.ta_yt[self.actor]); (p.min(y) / 2).max(1) }
// DepositYt.amount = { let b = self.ctx.token_balance(&self.ta_yt[self.actor]); (b / 2).max(1) }
// WithdrawYt.amount = { let b = self.position_yt_balance(self.actor); (b / 2).max(1) }
// SellYt.yt_in = { let b = self.ctx.token_balance(&self.ta_yt[self.actor]); (b / 4).max(1) }
// SellYt.min_sy_out = 0
// MarketDepositLp.amount = { let b = self.ctx.token_balance(&self.ta_lp[self.actor]); (b / 2).max(1) }
// MarketWithdrawLp.amount = { let b = self.position_lp_balance(self.actor); (b / 2).max(1) }
// MarketTwoDepositLiquidity.pt_intent = { let b = self.ctx.token_balance(&self.ta_pt[self.actor]); (b / 8).max(1) }
// MarketTwoDepositLiquidity.sy_intent = { let b = self.ctx.token_balance(&self.ta_sy[self.actor]); (b / 8).max(1) }
// MarketTwoDepositLiquidity.min_lp_out = 0
// MarketTwoWithdrawLiquidity.lp_in = { let b = self.ctx.token_balance(&self.ta_lp[self.actor]); (b / 4).max(1) }
// MarketTwoWithdrawLiquidity.min_pt_out = 0
// MarketTwoWithdrawLiquidity.min_sy_out = 0
// buy_yt requires `sy_to_strip > sy_in` where sy_to_strip = ceil(yt_out / rate) (buy_yt.rs:200-205):
// the trader supplies a LITTLE SY and receives a LOT of YT, the rest flash-borrowed.
// BuyYt.yt_out = { let m = self.ctx.token_balance(&self.market_escrow_pt); (m / 16).max(4) }
// BuyYt.sy_in = { let m = self.ctx.token_balance(&self.market_escrow_pt); (m / 64).max(1) }
//
// --- emission family ---------------------------------------------------------------------------
// `add_emission` is NOT fuzzable as a generated action on this target: it succeeds only when the SY
// program has exactly one more stream than the vault, and the two must be registered together or
// every other instruction panics (confirmed bug issue-02). `action_enable_emission` does both
// halves atomically and self-guards; the generated `action_add_emission` is expected to stay dead.
// emission_escrow = self.emission_escrow
// emission_dst = self.ta_emission[self.actor]
// treasury_emission_token_account = self.treasury_emission_ta
// robot_token_account = self.emission_escrow
// CollectEmission.index = 0
// CollectTreasuryEmission.emission_index = 0
// MarketCollectEmission.emission_index = 0
// `collect_emission` passes ONLY ctx.remaining_accounts to cpi_claim_emission
// (`vault/collect_emission.rs:104`), so every account cpi_accounts.claim_emission names must be here.
// CollectEmission.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.emission_custody, false), AccountMeta::new(self.emission_escrow, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new_readonly(SPL_TOKEN_ID, false)]
// CollectTreasuryEmission.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.emission_custody, false), AccountMeta::new(self.emission_escrow, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new_readonly(SPL_TOKEN_ID, false)]
//
// --- farm / market-emission family ---------------------------------------------------------
// `token_farm` is the ATA of the MARKET and is shared by add_farm and add_market_emission.
// mint_new = self.farm_mint
// mint = self.farm_mint
// token_farm = self.token_farm
// token_emission = self.token_farm
// token_source = self.token_farm_source
// token_dst = self.ta_farm[self.actor]
// token_emission_escrow = self.token_farm
// token_emission_dst = self.ta_farm[self.actor]
// AddFarm.until_timestamp = self.vault_start_ts + self.vault_duration / 2
// ModifyFarm.until_timestamp = self.vault_start_ts + self.vault_duration / 2
//
// --- wrapper remaining accounts + split points ------------------------------------------------
// Each wrapper slices `remaining_accounts` at its `*_accounts_until` argument: the prefix goes to
// the SY program's mint_sy/redeem_sy, the suffix to the vault/market interface CPIs
// (e.g. `wrapper_strip.rs:109-110`, `wrapper_merge.rs:84-85`). Both halves must therefore be
// present, in that order, and the split argument must equal the prefix length exactly.
// MINT_SY_ACCOUNTS = 7, REDEEM_SY_ACCOUNTS = 8 (see mint_sy_metas / redeem_sy_metas).
// WrapperStrip.mint_sy_accounts_until = MINT_SY_ACCOUNTS
// WrapperStrip.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)]
// WrapperProvideLiquidity.mint_base_accounts_until = MINT_SY_ACCOUNTS
// WrapperProvideLiquidity.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)]
// WrapperProvideLiquidityClassic.mint_sy_accounts_until = MINT_SY_ACCOUNTS
// WrapperProvideLiquidityClassic.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)]
// WrapperProvideLiquidityBase.mint_sy_accounts_until = MINT_SY_ACCOUNTS
// WrapperProvideLiquidityBase.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)]
// WrapperBuyPt.mint_sy_rem_accounts_until = MINT_SY_ACCOUNTS
// WrapperBuyPt.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)]
// WrapperBuyYt.mint_sy_accounts_length = MINT_SY_ACCOUNTS
// WrapperBuyYt.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)]
// WrapperMerge.redeem_sy_accounts_until = REDEEM_SY_ACCOUNTS
// WrapperMerge.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)]
// WrapperSellPt.redeem_sy_rem_accounts_until = REDEEM_SY_ACCOUNTS
// WrapperSellPt.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)]
// WrapperSellYt.redeem_sy_accounts_until = REDEEM_SY_ACCOUNTS
// WrapperSellYt.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)]
// WrapperCollectInterest.redeem_sy_accounts_length = REDEEM_SY_ACCOUNTS
// WrapperCollectInterest.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)]
// WrapperWithdrawLiquidity.redeem_sy_accounts_length = REDEEM_SY_ACCOUNTS
// WrapperWithdrawLiquidity.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)]
// WrapperWithdrawLiquidityClassic.redeem_sy_accounts_length = REDEEM_SY_ACCOUNTS
// WrapperWithdrawLiquidityClassic.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)]
//
// Remaining actor-signer roles. An unbound signer silently defaults to `self.payer`, which then
// does not own the actor's token accounts -- that alone kept every wrapper trade failing after the
// accounts and amounts were already correct. Check for `= self.payer.pubkey()` in a generated
// action before diagnosing anything else.
// merger = signer: self.users[self.actor].insecure_clone()
// buyer = signer: self.users[self.actor].insecure_clone()
// seller = signer: self.users[self.actor].insecure_clone()
// claimer = signer: self.users[self.actor].insecure_clone()
//
// --- wrapper amount clamps ----------------------------------------------------------------------
// Same reason as the core clamps above, confirmed the same way: `wrapper_merge` succeeds when
// called by hand with a valid amount but never under fuzzing, because `amount_py` is a raw u64.
// The wrappers additionally need real PT/YT/LP holdings, which only exist after a strip or a
// liquidity deposit -- so the amounts must come from live balances, not from the fuzzer.
// WrapperStrip.amount_base = { let b = self.ctx.token_balance(&self.ta_base[self.actor]); (b / 16).max(1) }
// WrapperMerge.amount_py = { let p = self.ctx.token_balance(&self.ta_pt[self.actor]); let y = self.ctx.token_balance(&self.ta_yt[self.actor]); (p.min(y) / 2).max(1) }
// WrapperProvideLiquidity.amount_base = { let m = self.ctx.token_balance(&self.market_escrow_pt); (m / 64).max(1) }
// WrapperProvideLiquidity.min_lp_out = 0
// WrapperProvideLiquidityClassic.amount_base = { let b = self.ctx.token_balance(&self.ta_base[self.actor]); (b / 16).max(1) }
// WrapperProvideLiquidityClassic.amount_pt = { let b = self.ctx.token_balance(&self.ta_pt[self.actor]); (b / 8).max(1) }
// WrapperProvideLiquidityClassic.min_lp_out = 0
// WrapperProvideLiquidityBase.amount_base = { let m = self.ctx.token_balance(&self.market_escrow_pt); (m / 64).max(1) }
// WrapperProvideLiquidityBase.min_lp_out = 0
// WrapperProvideLiquidityBase.external_pt_to_buy = { let m = self.ctx.token_balance(&self.market_escrow_pt); (m / 64).max(1) }
// WrapperProvideLiquidityBase.external_sy_constraint = { self.ctx.token_balance(&self.ta_sy[self.actor]).max(1) }
// WrapperBuyPt.pt_amount = { let m = self.ctx.token_balance(&self.market_escrow_pt); (m / 16).max(1) }
// WrapperBuyPt.max_base_amount = { let b = self.ctx.token_balance(&self.ta_base[self.actor]); (b / 8).max(1) }
// WrapperSellPt.amount_pt = { let b = self.ctx.token_balance(&self.ta_pt[self.actor]); (b / 8).max(1) }
// WrapperSellPt.min_base_amount = 0
// WrapperBuyYt.yt_out = { let m = self.ctx.token_balance(&self.market_escrow_pt); (m / 256).max(4) }
// WrapperBuyYt.max_base_amount = { let b = self.ctx.token_balance(&self.ta_base[self.actor]); (b / 8).max(1) }
// WrapperSellYt.yt_amount = { let b = self.ctx.token_balance(&self.ta_yt[self.actor]); (b / 8).max(1) }
// WrapperSellYt.min_base_amount = 0
// WrapperWithdrawLiquidity.amount_lp = { let b = self.ctx.token_balance(&self.ta_lp[self.actor]); (b / 4).max(1) }
// WrapperWithdrawLiquidity.sy_constraint = 0
// WrapperWithdrawLiquidityClassic.amount_lp = { let b = self.ctx.token_balance(&self.ta_lp[self.actor]); (b / 4).max(1) }
//
// --- wrapper family aliases ---
// Remaining per-role token-account spellings. The wrappers name the same three token accounts
// with a different suffix per instruction (depositor / merger / withdrawer), so each has to be
// bound separately or it silently stays a `scout_placeholder()` random pubkey -- which is exactly
// why every wrapper failed after the remaining_accounts were already correct.
// token_yt_depositor = self.ta_yt[self.actor]
// token_yt_merger = self.ta_yt[self.actor]
// token_sy_merger = self.ta_sy[self.actor]
// token_pt_merger = self.ta_pt[self.actor]
// token_yt_withdrawer = self.ta_yt[self.actor]
// token_treasury_fee_sy = self.token_treasury_fee_sy
// system_program = system_program::ID
//----------------------------------------------------------------
// The wrappers spell the same accounts differently from the instructions they compose.
// market_address_lookup_table = self.alt
// vault_address_lookup_table = self.alt
// vault_robot_yield_position = self.vault_yield_position
// token_sy_depositor = self.ta_sy[self.actor]
// token_pt_depositor = self.ta_pt[self.actor]
// token_sy_withdrawer = self.ta_sy[self.actor]
// token_pt_withdrawer = self.ta_pt[self.actor]
// associated_token_program = ASSOCIATED_TOKEN_ID
// token_metadata_program = MPL_TOKEN_METADATA_ID
// metadata = self.lp_metadata
// `Amount::All` rather than a fuzzer-chosen number: these instructions read the caller's own
// staged balance, and `All` is the branch that exercises the full settle path. Must be
// instruction-qualified -- strip/merge/deposit_yt/withdraw_yt also have an `amount`, but a u64.
// CollectInterest.amount = exponent_core::types::Amount::All
// CollectEmission.amount = exponent_core::types::Amount::All
// CollectTreasuryEmission.amount = exponent_core::types::Amount::All
// CollectTreasuryInterest.amount = exponent_core::types::Amount::All
// ClaimFarmEmissions.amount = exponent_core::types::Amount::All
//
// --- market family ---------------------------------------------------------------------------
// The market and the vault deliberately SHARE one address lookup table in this fixture, so both
// `address_lookup_table` and `address_lookup_table_vault` resolve to it.
// market = self.market
// mint_lp = self.mint_lp
// address_lookup_table_vault = self.alt
// vault_authority = self.vault_authority
// authority_vault = self.vault_authority
// yield_position_vault = self.vault_yield_position
// token_sy_escrow_vault = self.escrow_sy
// token_sy_escrow = self.market_escrow_sy
// token_pt_escrow = self.market_escrow_pt
// token_lp_escrow = self.market_escrow_lp
// token_fee_treasury_sy = self.token_treasury_fee_sy
// lp_position = self.lp_position[self.actor]
// rent = RENT_SYSVAR_ID
//
// Actor-scoped market signers and token accounts.
// trader = signer: self.users[self.actor].insecure_clone()
// depositor = signer: self.users[self.actor].insecure_clone()
// withdrawer = signer: self.users[self.actor].insecure_clone()
// token_sy_trader = self.ta_sy[self.actor]
// token_pt_trader = self.ta_pt[self.actor]
// token_yt_trader = self.ta_yt[self.actor]
// token_sy_src = self.ta_sy[self.actor]
// token_sy_dst = self.ta_sy[self.actor]
// token_pt_src = self.ta_pt[self.actor]
// token_pt_dst = self.ta_pt[self.actor]
// token_lp_src = self.ta_lp[self.actor]
// token_lp_dst = self.ta_lp[self.actor]
//
// Market-side SY CPI accounts. These name the MARKET's own SY position (slot 7), not the vault's --
// `init_market_two` gave the market its own robot account and its own `cpi_accounts`. Binding the
// vault's here is why every market action failed at first: called by hand with the market's
// accounts, deposit_liquidity succeeded immediately.
// MarketTwoDepositLiquidity.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.market_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)]
// MarketTwoWithdrawLiquidity.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.market_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)]
// MarketDepositLp.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.market_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)]
// MarketWithdrawLp.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.market_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)]
// TradePt.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.market_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)]
// `market_collect_emission` CPIs `claim_emission` for the MARKET, so it needs THAT stream's
// accounts: the SY global, the market's SY position, stream MARKET_EMISSION_STREAM's custody as
// the SOURCE (the mock resolves which stream it pays from the custody's MINT), and the market's
// own `token_farm` as the destination. The generated list pointed at `sy_custody`, which is the
// SY token custody and holds none of the reward mint.
// MarketCollectEmission.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.market_sy_position, false), AccountMeta::new(self.emission_custodies[MARKET_EMISSION_STREAM], false), AccountMeta::new(self.token_farm, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new_readonly(SPL_TOKEN_ID, false)]
// buy_yt and sell_yt self-CPI into BOTH the vault (strip/merge) and the market (trade_pt), so they
// need both robot positions present.
// BuyYt.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.market_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)]
// SellYt.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.market_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)]
//
// trade_pt arg clamps. `sy_constraint` is a SIGNED bound whose sign depends on the trade direction
// (trade_pt.rs:198-208): buying PT makes net_trader_sy negative, so the constraint must be negative.
// i64::MIN always satisfies it -- deliberate, because that slippage assert only restates what the
// handler just computed and is worthless as a fuzz target.
// `net_trader_pt` is deliberately left UNBOUND so the fuzzer still drives it: binding an arg makes
// the generator drop it from the action signature entirely, which would silently stop fuzzing it.
// Note the fuzzer will find that `net_trader_pt == 0` panics the program at trade_pt.rs:192 (the
// opposite-signs assert holds for neither branch) -- that is a real liveness bug, not noise.
// `action_trade_pt_clamped` below drives realistic trades so coverage still gets depth.
// Instruction-qualified: wrapper_withdraw_liquidity also has a `sy_constraint`, but a u64.
// TradePt.sy_constraint = i64::MIN
//
// Strip.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)]
// Merge.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)]
// CollectInterest.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)]
// DepositYt.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)]
// WithdrawYt.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)]
// StageYtYield.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)]
// CollectTreasuryInterest.remaining_accounts = metas: vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new(self.escrow_sy, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new_readonly(SPL_TOKEN_ID, false)]
// SCOUT:BINDINGS:END

// SCOUT:PRELUDE:BEGIN
// NOTE: this region is emitted BEFORE `declare_fuzz_program!`, so nothing here may name a
// generated type (`exponent_core::types::*`). Keep those helpers in the impl block.
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};

/// Program ids the fixture deploys or references alongside the target.
pub const EXPONENT_ADMIN_ID: Pubkey = Pubkey::new_from_array([
    32, 208, 232, 125, 131, 44, 246, 240, 12, 37, 185, 149, 115, 182, 74, 226,
    132, 61, 25, 234, 255, 139, 13, 219, 12, 35, 235, 86, 162, 81, 175, 215,
]);
pub const MPL_TOKEN_METADATA_ID: Pubkey = Pubkey::new_from_array([
    11, 112, 101, 177, 227, 209, 124, 69, 56, 157, 82, 127, 107, 4, 195, 205,
    88, 184, 108, 115, 26, 160, 253, 181, 73, 182, 209, 188, 3, 248, 41, 70,
]);
pub const SPL_TOKEN_ID: Pubkey = Pubkey::new_from_array([
    6, 221, 246, 225, 215, 101, 161, 147, 217, 203, 225, 70, 206, 235, 121, 172,
    28, 180, 133, 237, 95, 91, 55, 145, 58, 140, 245, 133, 126, 255, 0, 169,
]);
pub const ASSOCIATED_TOKEN_ID: Pubkey = Pubkey::new_from_array([
    140, 151, 37, 143, 78, 36, 137, 241, 187, 61, 16, 41, 20, 142, 13, 131,
    11, 90, 19, 153, 218, 255, 16, 132, 4, 142, 123, 216, 219, 233, 248, 89,
]);
pub const RENT_SYSVAR_ID: Pubkey = Pubkey::new_from_array([
    6, 167, 213, 23, 25, 44, 92, 81, 33, 140, 201, 76, 61, 74, 241, 127,
    88, 218, 238, 8, 155, 161, 253, 68, 227, 219, 217, 138, 0, 0, 0, 0,
]);
pub const ADDRESS_LOOKUP_TABLE_ID: Pubkey = Pubkey::new_from_array([
    2, 119, 166, 175, 151, 51, 155, 122, 200, 141, 24, 146, 201, 4, 70, 245,
    0, 2, 48, 146, 102, 246, 46, 83, 193, 24, 36, 73, 130, 0, 0, 0,
]);

/// The mock SY program (`fuzz/mock_sy`) that stands in for Kamino/Jupiter/Hylo/Solstice.
pub const MOCK_SY_ID: Pubkey = Pubkey::new_from_array([
    71, 81, 55, 229, 215, 188, 216, 250, 57, 145, 69, 84, 238, 204, 88, 20,
    16, 165, 157, 161, 233, 34, 198, 89, 57, 199, 107, 225, 107, 106, 214, 1,
]);
/// Mock SY test-control discriminators (see `fuzz/mock_sy/src/lib.rs`, module `ix`).
pub const MOCK_SY_INIT_GLOBAL: u8 = 199;
pub const MOCK_SY_SET_EXCHANGE_RATE: u8 = 200;
/// `set_emission_index(u32 index, Number value)` -- absolute assignment of a global stream index.
pub const MOCK_SY_SET_EMISSION_INDEX: u8 = 201;
/// `add_emission_index(Number initial [, Pubkey mint])` -- registers a new global stream. The
/// 32-byte mint tail is REQUIRED if the stream is ever to be claimed: the mock's `claim_emission`
/// resolves the stream from the mint of `emission_custody` and fails with custom error 13 otherwise.
pub const MOCK_SY_ADD_EMISSION_INDEX: u8 = 202;
/// `fund_emission(u32 index, u64 amount)` -- credits `amount_claimable` directly, on top of the
/// ordinary accrual it performs first. Amount 0 is a pure "touch the position" call, which is how a
/// non-retroactive index move is turned into real claimable balance.
pub const MOCK_SY_FUND_EMISSION: u8 = 203;

/// Market seed id. NOT 0 -- see the comment in setup(): `MarketTwo::signer_seeds()` uses an empty
/// third seed for id 0 while `init` uses a one-byte `[0]`, so a seed_id-0 market cannot sign.
pub const MARKET_SEED_ID: u8 = 1;
/// AMM curve parameters for the seeded market.
pub const MARKET_LN_FEE_RATE_ROOT: f64 = 0.0003;
pub const MARKET_RATE_SCALAR_ROOT: f64 = 100.0;
pub const MARKET_INIT_RATE_ANCHOR: f64 = 1.05;
/// PT and SY the admin seeds the market with.
pub const MARKET_PT_INIT: u64 = 10_000_000;
pub const MARKET_SY_INIT: u64 = 10_000_000;

/// Account counts of the mock SY program's mint_sy / redeem_sy lists. The wrappers pass these as
/// the split point for `remaining_accounts`, so they must match `mint_sy_metas`/`redeem_sy_metas`.
pub const MINT_SY_ACCOUNTS: u8 = 7;
pub const REDEEM_SY_ACCOUNTS: u8 = 8;

/// Number of independent actors the fixture funds. `users[0]` is the designated adversary.
/// `users[N_USERS - 1]` deliberately gets NO pre-created yield position, so
/// `action_initialize_yield_position` has a live target instead of being disabled forever by
/// setup having already minted every position (see references/setup-glue.md on that trade).
pub const N_USERS: usize = 4;

/// How many SY-side reward streams the fixture provisions. Must be >= 2 or the whole multi-stream
/// class is unreachable (see BLIND-SPOTS.md #1). The mock caps at 8 (`mock_sy/src/state.rs:31`);
/// each one costs two ALT slots and four token accounts in `setup()`.
pub const N_EMISSION_STREAMS: usize = 3;

/// ALT slot holding the EXPONENT PROGRAM itself. Slots 0..=9 are fixed, then each emission stream
/// takes a pair at `(10 + 2i, 11 + 2i)`, so this lands immediately after them. It is forwarded to
/// the SY program on every `get_sy_state` purely so a CPI back into Exponent can be *dispatched* --
/// see the reentrancy probe. Must match the `alt_slot_exponent` computed in `setup()`.
pub const ALT_SLOT_EXPONENT: u8 = 10 + 2 * N_EMISSION_STREAMS as u8;

/// ALT slot holding `token_farm`, the MARKET's escrow for its reward stream. `market_collect_emission`
/// claims from the SY program into this account, so it has to be addressable by index like every
/// other CPI account.
pub const ALT_SLOT_TOKEN_FARM: u8 = ALT_SLOT_EXPONENT + 1;

/// Which emission stream the MARKET pays in. The mock SY program keeps ONE global emission list, so
/// a stream the market claims from must also be a stream the vault has registered -- otherwise the
/// two lists differ in length and every vault instruction panics (that is issue-02). Making the
/// market's reward token stream 2 of the vault's own list keeps them in lockstep.
pub const MARKET_EMISSION_STREAM: usize = 2;
/// Decimals shared by the base and SY mints.
pub const MINT_DECIMALS: u8 = 6;

/// Anchor account discriminator for `exponent_admin::Admin`, taken from the shipped IDL.
const ADMIN_DISCRIMINATOR: [u8; 8] = [244, 158, 220, 65, 8, 73, 4, 65];

/// Fixed-point scale of `precise_number::Number` (1e12) -- the SY exchange-rate denominator.
pub const NUMBER_ONE: u128 = 1_000_000_000_000;

/// `Number` is `#[repr(C)] struct Number([u64; 4])` -- 32 bytes little-endian, 1e12 fixed point.
pub fn number_bytes(value_1e12: u128) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[0..16].copy_from_slice(&value_1e12.to_le_bytes());
    out
}

/// A `Number` representing `whole` (i.e. `whole * 1e12`).
pub fn number_whole(whole: u64) -> [u8; 32] {
    number_bytes(whole as u128 * NUMBER_ONE)
}

/// Same value as `number_whole`, in the `[u64; 4]` word form the IDL-generated `Number` uses.
pub fn number_words(value_1e12: u128) -> [u64; 4] {
    [(value_1e12 & u64::MAX as u128) as u64, (value_1e12 >> 64) as u64, 0, 0]
}

/// Borsh bytes of an `exponent_admin::Admin` naming `admins` in EVERY principle role.
///
/// Different instructions check different roles and it is not guessable from the name:
/// `initialize_vault` and `add_farm` check `principles.hot_admin`, but `modify_farm` checks
/// `principles.cold_admin` (`market_two/admin/modify_farm.rs:33-40`). Seeding only the hot set
/// left `modify_farm` failing for a reason that looked like bad arguments. Populating all six keeps
/// the harness from silently losing whole instructions to an authorization detail; the roles
/// themselves are not what this harness is testing.
///
/// Layout: 8-byte discriminator, `uber_admin: Pubkey`, `proposed_uber_admin: Option<Pubkey>`
/// (1-byte `None` tag), then six `Vec<Pubkey>` (4-byte LE length each) --
/// hot, cold, pause, reserved1..3.
pub fn admin_account_data(uber_admin: &Pubkey, admins: &[Pubkey]) -> Vec<u8> {
    let mut d = Vec::with_capacity(256);
    d.extend_from_slice(&ADMIN_DISCRIMINATOR);
    d.extend_from_slice(uber_admin.as_ref());
    d.push(0); // proposed_uber_admin: None
    for _ in 0..6 {
        d.extend_from_slice(&(admins.len() as u32).to_le_bytes());
        for a in admins {
            d.extend_from_slice(a.as_ref());
        }
    }
    d
}

/// Bytes of a v1 Address Lookup Table account holding `addresses`.
///
/// Exponent only READS the ALT (`instructions/util.rs:4-9`) and indexes it by
/// `CpiInterfaceContext.alt_index`, so the real Address Lookup Table program need not be deployed --
/// the account can be written directly. Layout: 4-byte program-state discriminant (1 = LookupTable),
/// then `LookupTableMeta`, with the address array starting at `LOOKUP_TABLE_META_SIZE` (56). If this
/// layout were wrong, `deserialize_lookup_table`'s `unwrap()` panics, so it fails loudly rather than
/// silently yielding the wrong pubkeys.
pub fn build_alt_data(addresses: &[Pubkey], authority: &Pubkey) -> Vec<u8> {
    const LOOKUP_TABLE_META_SIZE: usize = 56;
    let mut data = vec![0u8; LOOKUP_TABLE_META_SIZE + addresses.len() * 32];
    data[0..4].copy_from_slice(&1u32.to_le_bytes()); // discriminant: LookupTable
    data[4..12].copy_from_slice(&u64::MAX.to_le_bytes()); // deactivation_slot: never deactivated
    data[12..20].copy_from_slice(&0u64.to_le_bytes()); // last_extended_slot
    data[20] = 0; // last_extended_slot_start_index
    data[21] = 1; // authority: Some
    data[22..54].copy_from_slice(authority.as_ref());
    // data[54..56] is padding and stays zero
    for (i, a) in addresses.iter().enumerate() {
        let off = LOOKUP_TABLE_META_SIZE + i * 32;
        data[off..off + 32].copy_from_slice(a.as_ref());
    }
    data
}
// SCOUT:PRELUDE:END

crucible_idl_gen::declare_fuzz_program!("idls/exponent_core.json");

use exponent_core::{accounts, instruction};

#[derive(Clone)]
struct ExponentCoreFixture {
    ctx: crate::__scout_crucible_test_context::TestContext,
    program_id: Pubkey,
    payer: Rc<Keypair>,
    // SCOUT:FIELDS:BEGIN
    /// Independent actors. `users[0]` is the designated ADVERSARY for value-conservation
    /// properties; the others exist so a property can tell "the attacker got richer" apart from
    /// "everyone got richer because the vault genuinely earned yield".
    users: Vec<Rc<Keypair>>,

    // --- mock SY program (fuzz/mock_sy): the external yield source Exponent CPIs into ---
    sy_program_id: Pubkey,
    sy_global: Pubkey,
    sy_authority: Pubkey,
    sy_custody: Pubkey,
    base_custody: Pubkey,
    /// The vault authority's position with the SY program (the "robot" account).
    vault_sy_position: Pubkey,

    // --- mints ---
    base_mint: Pubkey,
    sy_mint: Pubkey,
    mint_pt: Pubkey,
    mint_yt: Pubkey,

    // --- vault ---
    admin_account: Pubkey,
    vault: Pubkey,
    vault_authority: Pubkey,
    escrow_yt: Pubkey,
    escrow_sy: Pubkey,
    treasury_sy_ta: Pubkey,
    vault_yield_position: Pubkey,
    /// Synthesized address lookup table; `Vault.cpi_accounts` indexes into it.
    alt: Pubkey,

    // --- per-user token accounts and positions, indexed by user ---
    ta_base: Vec<Pubkey>,
    ta_sy: Vec<Pubkey>,
    ta_pt: Vec<Pubkey>,
    ta_yt: Vec<Pubkey>,
    yield_position: Vec<Pubkey>,

    // --- emission stream 0 (mock SY global stream 0 <-> vault.emissions[0]) ---
    /// SPL mint of the emission reward token.
    emission_mint: Pubkey,
    /// The mock SY program's custody for the emission token (SPL authority = sy_authority).
    emission_custody: Pubkey,
    /// The vault's emission escrow: `EmissionInfo.token_account`, SPL authority = vault authority.
    /// This is both `AddEmission.robot_token_account` and `CollectEmission.emission_escrow`.
    emission_escrow: Pubkey,
    /// Treasury destination for the emission fee (`EmissionInfo.treasury_token_account`).
    treasury_emission_ta: Pubkey,
    /// Per-user destination token account for collected emissions (stream 0).
    ta_emission: Vec<Pubkey>,

    // --- all emission streams, indexed by stream ---------------------------------------------
    // Index 0 of each is the scalar above. Kept as parallel vectors rather than replacing the
    // scalars so that the four shipped PoCs, which name `emission_mint` / `emission_custody` /
    // `emission_escrow` directly, keep compiling unchanged.
    emission_mints: Vec<Pubkey>,
    emission_custodies: Vec<Pubkey>,
    emission_escrows: Vec<Pubkey>,
    treasury_emission_tas: Vec<Pubkey>,
    /// `[stream][user]`.
    ta_emissions: Vec<Vec<Pubkey>>,

    // --- farm / market-emission reward tokens ---
    /// Reward mint used for both the LP farm and the market emission stream.
    farm_mint: Pubkey,
    /// ATA(market, farm_mint) -- `AddFarm.token_farm` and `AddMarketEmission.token_emission`
    /// both require `associated_token::authority = market`.
    token_farm: Pubkey,
    /// Admin-held source the farm is funded from.
    token_farm_source: Pubkey,
    /// Per-user destination for farm / market-emission claims.
    ta_farm: Vec<Pubkey>,

    // --- market ---
    market: Pubkey,
    mint_lp: Pubkey,
    market_escrow_pt: Pubkey,
    market_escrow_sy: Pubkey,
    market_escrow_lp: Pubkey,
    market_sy_position: Pubkey,
    token_treasury_fee_sy: Pubkey,
    /// Metaplex metadata PDA for the LP mint (`add_lp_tokens_metadata`'s target).
    lp_metadata: Pubkey,
    /// The hot admin's own token accounts. The admin seeds the market with real PT and SY, so it
    /// has to hold them like any other actor.
    payer_base: Pubkey,
    payer_sy: Pubkey,
    payer_pt: Pubkey,
    payer_lp: Pubkey,
    /// Per-user LP token accounts and personal LP positions.
    ta_lp: Vec<Pubkey>,
    lp_position: Vec<Pubkey>,

    // --- shadow ledger / bookkeeping ---
    vault_start_ts: u32,
    vault_duration: u32,
    /// Base tokens each actor was funded with, so a property can compare end state to start state.
    initial_base_per_user: u64,
    /// Current wall-clock, mirrored from every `action_advance_time`. Properties need it to ask
    /// `Vault::is_active`, and the Clock sysvar is not readable back through TestContext.
    current_ts: u32,
    /// Which actor the next user-facing action speaks for; moved by `action_select_actor`.
    actor: usize,

    // --- P-0004 / P-0007 baselines, captured at the end of setup() ---------------------------
    //
    // WHY FIXTURE FIELDS AND NOT A `thread_local!`. P-0007 was disabled because its high-water
    // mark lived in a thread-local, which SURVIVES the fuzzer's snapshot restore while the SVM
    // state does not -- so the mark stayed high, the vault's ATH reset to 1.0, and the property
    // reported a fall on a perfectly healthy run (100+ times). Fixture fields do not have that
    // problem: crucible stores the fixture ALONGSIDE the state delta
    // (`__FixtureWrapper(__iter_fixture.clone())`, crucible-fuzz-macro/src/stateful.rs:2358) and
    // restores them together (`__iter_fixture = wrapper.0.clone()`, :2138). A field is therefore
    // snapshot-consistent by construction, which is exactly the "per-iteration reset hook" the
    // disabled note asked for.
    //
    // One residual: :2140 falls back to the worker's template fixture when a pool entry carries
    // no fixture state. That resets these fields to their setup values while the SVM may be
    // further along. For `ath_seen` that can only SUPPRESS a finding (mark too low), never
    // manufacture one. For the P-0004 gate it could wrongly OPEN the gate, which is why every
    // load-bearing gate condition below is also re-derived from observable on-chain state and
    // `sy_rate_moved` only ever adds restriction on top.
    /// SY exchange rate at setup (1e12 fixed point, low 128 bits). P-0004 values the adversary at
    /// this mark and refuses to assert at any other.
    baseline_sy_rate: u128,
    /// `Vault.all_time_high_sy_exchange_rate` at setup.
    baseline_ath: u128,
    /// Market PT/SY escrow balances at setup. P-0004's counterparty guard: the market must be back
    /// in an identical position before an adversary gain can be called value creation rather than
    /// a transfer from whoever took the other side.
    baseline_market_pt: u64,
    baseline_market_sy: u64,
    /// The market's SY held with the SY PROGRAM rather than in its escrow token account. At setup
    /// `market_escrow_sy` is 0 and the market's SY sits in `market_sy_position`, so a guard that
    /// watched only the token account would be blind to every SY-side trade.
    baseline_market_sy_position: u64,
    /// LP mint supply at setup. A liquidity round trip that restores both escrows but leaves the
    /// supply changed means someone is holding LP nobody paid for.
    baseline_lp_supply: u64,
    /// The adversary's total value in py units at setup (see `adversary_value_py`).
    baseline_adversary_py: u128,
    /// Set once the SY rate has been moved off its baseline. Catches the one channel the observable
    /// gate cannot see: a rate that dips and returns. A dip-then-return credits
    /// `calc_earned_sy(yt, dipped, baseline)` (yield_token_position.rs:201-217) -- real interest, at
    /// an apparently unchanged rate -- and would read as adversary profit.
    sy_rate_moved: bool,
    /// P-0007's running high-water mark for `Vault.all_time_high_sy_exchange_rate`.
    ath_seen: u128,
    /// P-0011's per-stream high-water marks for `Vault.emissions[i].last_seen_index` and
    /// `.final_index`. Fixture fields, for the same snapshot-consistency reason as `ath_seen`.
    emission_index_seen: Vec<u128>,
    emission_final_seen: Vec<u128>,
    /// P-0010's high-water mark for `mint_yt.supply - mint_pt.supply`. The gap opens only when a
    /// post-maturity `merge` burns PT without burning YT, and nothing ever re-mints YT, so it is
    /// one-way by construction and must never shrink.
    yt_pt_gap_seen: u64,
    /// Highest wall-clock ever observed. P-0002 must not fire on state that was legitimately built
    /// while the vault was expired, and `merge` leaves PT and YT permanently unequal once that has
    /// happened. Solana's Clock is monotonic so on-chain this equals "now"; here it is a field for
    /// the same snapshot-consistency reason as `ath_seen`.
    ts_seen: u32,
    // SCOUT:FIELDS:END
}

#[fuzz_fixture]
impl ExponentCoreFixture {
    fn scout_placeholder(&self) -> Pubkey { Pubkey::new_unique() }

    pub fn setup() -> Self {
        let mut ctx = crate::__scout_crucible_test_context::TestContext::new();
        let program_id = Pubkey::new_from_array(exponent_core::ID.to_bytes());
        // SCOUT:TARGET-PROGRAM:BEGIN
        crate::__scout_crucible_test_context::TestContext::add_program(&mut ctx, &program_id, SCOUT_TARGET_PROGRAM_ARTIFACT).unwrap();
        // SCOUT:TARGET-PROGRAM:END
        let payer = Rc::new(Keypair::new());
        ctx.create_account().pubkey(payer.pubkey()).lamports(1_000_000_000)
            .owner(system_program::ID).create().unwrap();
        // SCOUT:SETUP-GLUE:BEGIN
        // ---- compute budget -------------------------------------------------------------------
        // 200k is the per-transaction default with NO ComputeBudget instruction; real Solana
        // clients request up to 1.4M, and every production caller of these instructions must. The
        // coverage build makes this bite harder than mainnet would: the target .so is compiled at
        // opt-level=1 (required for source-level LCOV), which burns far more CU than the shipped
        // opt-level=3 build. Leaving the default here would fail initialize_vault on a harness
        // artifact and read as a protocol defect.
        let mut ctx = ctx.with_compute_budget(1_400_000);

        // ---- heap frame -----------------------------------------------------------------------
        // Exponent ships a custom bump allocator with `HEAP_LENGTH = 8 * 32 KiB`
        // (`programs/exponent_core/src/allocator.rs:101`), and it bounds-checks against ITS OWN
        // 256 KiB length rather than the heap the VM actually mapped. So once an instruction
        // allocates past the default 32 KiB it hands back an unmapped pointer instead of null and
        // the VM faults with "Access violation in heap section" at HEAP_START + 0x8000.
        //
        // That is exactly where `buy_yt` and `wrapper_buy_yt` died -- for EVERY input size from
        // `market_pt / 4` down to 152 units, so it was never a clamping problem. The program's own
        // header says a `requestHeapFrame` is required; prepending one to the transaction does NOT
        // work here, because LiteSVM ignores a transaction's ComputeBudget instructions whenever a
        // fixed budget has been set (`litesvm/src/lib.rs:1035`, `self.compute_budget.unwrap_or_else`)
        // -- and `with_compute_budget(1_400_000)` above sets one. So the heap has to be raised on
        // the budget itself.
        //
        // The type is never named: `get_compute_budget()` hands back the budget crucible just
        // installed, and only `heap_size` is changed.
        {
            let mut budget = ctx.svm.get_compute_budget().expect("compute budget was just set");
            budget.heap_size = 8 * 32 * 1024;
            let svm = std::mem::replace(&mut ctx.svm, crucible_test_context::litesvm::LiteSVM::new());
            ctx.svm = svm.with_compute_budget(budget);
        }

        // ---- 0. deterministic clock ----------------------------------------------------------
        // The vault gates on wall-clock (`is_active`, `is_expired`, `now()` = unix_timestamp as
        // u32), so the whole world is pinned to a known start and the fuzzer moves time via an
        // explicit action rather than by accident.
        let vault_start_ts: u32 = 1_700_000_000;
        let vault_duration: u32 = 365 * 24 * 60 * 60;
        Self::warp_clock(&mut ctx, vault_start_ts as i64 + 1);

        // ---- 1. external programs -------------------------------------------------------------
        // Metaplex is REQUIRED, not optional: initialize_vault does a real CreateMetadataAccountV3
        // CPI for the PT mint. Its e_machine is 247 (normal for a mainnet-deployed program);
        // verified to load and execute in this VM.
        //
        // `FUZZ_PROGRAM_SO` (set by the coverage pass via `--program-so`) is honoured by
        // `TestContext::add_program` for EVERY program, not just the target -- it never compares
        // the program id (crucible-test-context/src/lib.rs:1996-2008). In a harness with fixture
        // programs that silently replaces them with the target's binary: the coverage run failed
        // with `DeclaredProgramIdMismatch` from the mock SY's address, because the bytes loaded
        // there were exponent_core's. The canonical SCOUT:TARGET-PROGRAM load above runs BEFORE
        // this and must keep the override, so suppress it only across the fixture loads.
        let __scout_program_so_override = std::env::var("FUZZ_PROGRAM_SO").ok();
        std::env::remove_var("FUZZ_PROGRAM_SO");
        crucible_test_context::TestContext::add_program(
            &mut ctx, &MPL_TOKEN_METADATA_ID, "fixtures/mpl_token_metadata.so").unwrap();
        let sy_program_id = MOCK_SY_ID;
        crucible_test_context::TestContext::add_program(
            &mut ctx, &sy_program_id, "fixtures/mock_sy.so").unwrap();
        if let Some(v) = __scout_program_so_override {
            std::env::set_var("FUZZ_PROGRAM_SO", v);
        }

        // ---- 2. actors -------------------------------------------------------------------------
        // users[0] is the designated adversary for value-conservation properties.
        let users: Vec<Rc<Keypair>> = (0..N_USERS).map(|_| Rc::new(Keypair::new())).collect();
        for u in &users {
            ctx.create_account().pubkey(u.pubkey()).lamports(1_000_000_000)
                .owner(system_program::ID).create().unwrap();
        }

        // ---- 3. Admin account ------------------------------------------------------------------
        // Synthesized rather than initialized: `Account<'info, Admin>` only requires the account be
        // owned by exponent_admin and carry the right discriminator, so the admin program itself
        // never has to be deployed. The payer is the hot admin -- the sole authorization check in
        // initialize_vault's `validate()`.
        let admin_account = Pubkey::new_unique();
        let admin_data = admin_account_data(&payer.pubkey(), &[payer.pubkey()]);
        ctx.create_account().pubkey(admin_account).owner(EXPONENT_ADMIN_ID)
            .data(&admin_data).create().unwrap();

        // ---- 4. mints and the mock SY world ----------------------------------------------------
        let (sy_global, _) = Pubkey::find_program_address(&[b"sy_global"], &sy_program_id);
        let (sy_authority, _) = Pubkey::find_program_address(&[b"sy_authority"], &sy_program_id);

        let base_mint = Pubkey::new_unique();
        ctx.create_mint().pubkey(base_mint).decimals(MINT_DECIMALS)
            .mint_authority(payer.pubkey()).is_initialized(true).create().unwrap();

        // SY is a real SPL mint so supply moves for real and token-balance conservation properties
        // are meaningful. Its mint authority is `sy_global`, NOT `sy_authority`: mint_sy's account
        // list does not include sy_authority, so sy_global is the only program-owned signer
        // available to it for the MintTo CPI (fuzz/mock_sy/src/lib.rs, `mint_sy`).
        let sy_mint = Pubkey::new_unique();
        ctx.create_mint().pubkey(sy_mint).decimals(MINT_DECIMALS)
            .mint_authority(sy_global).is_initialized(true).create().unwrap();

        // Custody token accounts are owned by sy_authority, which signs their transfers.
        let sy_custody = Self::make_token_account(&mut ctx, sy_mint, sy_authority, 0);
        let base_custody = Self::make_token_account(&mut ctx, base_mint, sy_authority, 0);

        Self::init_mock_sy(&mut ctx, &payer, sy_program_id, sy_global);

        // ---- 5. vault PDAs ---------------------------------------------------------------------
        // `vault` is `init` with no seeds, so it is a keypair account and must co-sign.
        let vault_kp = Keypair::new();
        let vault = vault_kp.pubkey();
        let (vault_authority, _) = Pubkey::find_program_address(&[b"authority", vault.as_ref()], &program_id);
        let (mint_pt, _) = Pubkey::find_program_address(&[b"mint_pt", vault.as_ref()], &program_id);
        let (mint_yt, _) = Pubkey::find_program_address(&[b"mint_yt", vault.as_ref()], &program_id);
        let (escrow_yt, _) = Pubkey::find_program_address(&[b"escrow_yt", vault.as_ref()], &program_id);
        let (vault_yield_position, _) = Pubkey::find_program_address(
            &[b"yield_position", vault.as_ref(), vault_authority.as_ref()], &program_id);
        let (metadata, _) = Pubkey::find_program_address(
            &[b"metadata", MPL_TOKEN_METADATA_ID.as_ref(), mint_pt.as_ref()], &MPL_TOKEN_METADATA_ID);
        // escrow_sy is created by the handler as the vault authority's ATA for mint_sy.
        let escrow_sy = Self::ata(&vault_authority, &sy_mint);
        let (vault_sy_position, _) = Pubkey::find_program_address(
            &[b"sy_position", vault_authority.as_ref()], &sy_program_id);

        let treasury_sy_ta = Self::make_token_account(&mut ctx, sy_mint, payer.pubkey(), 0);

        // Market PDAs are derived here, before the ALT, because the market's own SY-program
        // accounts have to occupy ALT slots too -- `init_market_two` deposits the seeded SY
        // through `market.cpi_accounts`, signed by `market.signer_seeds()`.
        //
        // seed_id is 1, NOT 0, deliberately: `MarketTwo::signer_seeds()` (state/market_two.rs:119)
        // returns an EMPTY third seed when `seed_id[0] == 0`, while the account is `init`ed at
        // `seeds = [MARKET_SEED, vault, &[seed_id]]` -- i.e. a one-byte `[0]`. Those derive
        // different addresses, so a seed_id-0 market cannot sign its own CPIs. seed_id >= 1 is
        // self-consistent.
        let (market, _) = Pubkey::find_program_address(
            &[b"market", vault.as_ref(), &[MARKET_SEED_ID]], &program_id);
        let (mint_lp, _) = Pubkey::find_program_address(&[b"mint_lp", market.as_ref()], &program_id);
        let (market_escrow_pt, _) =
            Pubkey::find_program_address(&[b"escrow_pt", market.as_ref()], &program_id);
        let (market_escrow_sy, _) =
            Pubkey::find_program_address(&[b"escrow_sy", market.as_ref()], &program_id);
        let (market_escrow_lp, _) =
            Pubkey::find_program_address(&[b"escrow_lp", market.as_ref()], &program_id);
        let (market_sy_position, _) = Pubkey::find_program_address(
            &[b"sy_position", market.as_ref()], &sy_program_id);
        let token_treasury_fee_sy = Self::make_token_account(&mut ctx, sy_mint, payer.pubkey(), 0);
        let (lp_metadata, _) = Pubkey::find_program_address(
            &[b"metadata", MPL_TOKEN_METADATA_ID.as_ref(), mint_lp.as_ref()],
            &MPL_TOKEN_METADATA_ID);

        // ---- 5b. emission stream 0 -------------------------------------------------------------
        // Provisioned here, not by an action, because the ALT is fixed at this point and
        // `claim_emission`'s account list is addressed by ALT INDEX -- an emission whose custody and
        // escrow are not in the table can never be collected. `add_emission` itself is still a real
        // admin instruction driven later; this only creates the accounts it will name.
        let emission_mint = Pubkey::new_unique();
        ctx.create_mint().pubkey(emission_mint).decimals(MINT_DECIMALS)
            .mint_authority(payer.pubkey()).is_initialized(true).create().unwrap();
        // Funded up front: the SY program can only pay out emissions it actually holds, and an
        // unfunded custody makes every `collect_emission` fail for a reason that has nothing to do
        // with Exponent's accounting.
        let emission_custody =
            Self::make_token_account(&mut ctx, emission_mint, sy_authority, 1_000_000_000_000);
        let emission_escrow = Self::make_token_account(&mut ctx, emission_mint, vault_authority, 0);
        let treasury_emission_ta =
            Self::make_token_account(&mut ctx, emission_mint, payer.pubkey(), 0);

        // ---- 5c. the remaining emission streams -------------------------------------------------
        // A SECOND stream is not a nicety. With only one, `collect_emission`'s `index` argument is
        // always 0, `calc_emission_surpluses`'s zip over two vectors (vault.rs:281-289) never sees
        // them differ in length, the positional-shift class behind OOS-01/OOS-02 has nothing to
        // shift, and issue-01's own claim that the defect "recurs independently for each emission"
        // is untestable. Everything here mirrors stream 0 exactly; only the mint differs.
        //
        // Index 0 of each vector IS the scalar above, so existing PoCs that name `emission_mint`
        // keep working unchanged.
        let mut emission_mints = vec![emission_mint];
        let mut emission_custodies = vec![emission_custody];
        let mut emission_escrows = vec![emission_escrow];
        let mut treasury_emission_tas = vec![treasury_emission_ta];
        // `farm_mint` is stream MARKET_EMISSION_STREAM's mint, created here rather than later so the
        // stream provisioning below can give it an SY-side custody like any other reward token.
        let farm_mint = Pubkey::new_unique();
        ctx.create_mint().pubkey(farm_mint).decimals(MINT_DECIMALS)
            .mint_authority(payer.pubkey()).is_initialized(true).create().unwrap();
        for s in 1..N_EMISSION_STREAMS {
            let m = if s == MARKET_EMISSION_STREAM { farm_mint } else { Pubkey::new_unique() };
            if s != MARKET_EMISSION_STREAM {
                ctx.create_mint().pubkey(m).decimals(MINT_DECIMALS)
                    .mint_authority(payer.pubkey()).is_initialized(true).create().unwrap();
            }
            emission_custodies.push(
                Self::make_token_account(&mut ctx, m, sy_authority, 1_000_000_000_000));
            emission_escrows.push(Self::make_token_account(&mut ctx, m, vault_authority, 0));
            treasury_emission_tas.push(Self::make_token_account(&mut ctx, m, payer.pubkey(), 0));
            emission_mints.push(m);
        }

        // ---- 6. address lookup table -----------------------------------------------------------
        // Vault.cpi_accounts addresses the SY program's accounts by INDEX into this table, so these
        // slots are the contract between the harness and the mock SY's account order.
        //
        // Emission stream `i` owns the PAIR (10 + 2i, 11 + 2i). They must be a pair, and they must
        // be per-stream: `claim_emission` is resolved by the MINT of the custody it is handed
        // (`collect_emission.rs`, custom error 13 otherwise), so pointing two streams at one
        // custody would make the second one collect the first one's token.
        let mut alt_addresses = vec![
            sy_global,        // 0
            vault_sy_position,// 1
            escrow_sy,        // 2
            sy_custody,       // 3
            vault_authority,  // 4
            SPL_TOKEN_ID,     // 5
            sy_authority,     // 6
            market_sy_position,// 7  -- the MARKET's own SY-program position
            market_escrow_sy, // 8
            market,           // 9  -- signs the market's SY CPIs
        ];
        for i in 0..N_EMISSION_STREAMS {
            alt_addresses.push(emission_custodies[i]); // 10 + 2i -- claim_emission source (SY side)
            alt_addresses.push(emission_escrows[i]);   // 11 + 2i -- destination (vault side)
        }
        // 10 + 2N -- the EXPONENT PROGRAM itself, forwarded to the SY program on every
        // `get_sy_state`. It exists so the mock can attempt a CPI back into Exponent: a callee's
        // own program account must be in `account_infos` for an invoke to be dispatched at all, so
        // without this the reentrancy probe fails with "Unknown program" and answers a question
        // nobody asked. Read-only, non-signer, and ignored unless `[205] arm_reentrancy` is set.
        let alt_slot_exponent = alt_addresses.len() as u8;
        assert_eq!(alt_slot_exponent, ALT_SLOT_EXPONENT,
                   "ALT_SLOT_EXPONENT is out of step with the table built here");
        alt_addresses.push(program_id);
        // 10 + 2N + 1 -- the MARKET's escrow for its reward stream, so `market_collect_emission`
        // can name a claim destination by ALT index.
        assert_eq!(alt_addresses.len() as u8, ALT_SLOT_TOKEN_FARM,
                   "ALT_SLOT_TOKEN_FARM is out of step with the table built here");
        alt_addresses.push(Self::ata(&market, &farm_mint));
        let alt = Pubkey::new_unique();
        ctx.create_account().pubkey(alt).owner(ADDRESS_LOOKUP_TABLE_ID)
            .data(&build_alt_data(&alt_addresses, &payer.pubkey())).create().unwrap();

        // ---- 7. initialize the vault -----------------------------------------------------------
        Self::run_initialize_vault(
            &mut ctx, program_id, &payer, &vault_kp, admin_account, vault_authority,
            mint_pt, mint_yt, escrow_yt, escrow_sy, sy_mint, treasury_sy_ta,
            sy_program_id, alt, vault_yield_position, metadata, vault_sy_position,
            vault_start_ts, vault_duration,
        );

        // ---- 8. per-user token accounts and yield positions -------------------------------------
        let initial_base_per_user: u64 = 1_000_000 * 10u64.pow(MINT_DECIMALS as u32);
        let mut ta_base = Vec::new();
        let mut ta_sy = Vec::new();
        let mut ta_pt = Vec::new();
        let mut ta_yt = Vec::new();
        let mut ta_emission = Vec::new();
        let mut yield_position = Vec::new();
        for u in &users {
            ta_base.push(Self::make_token_account(&mut ctx, base_mint, u.pubkey(), initial_base_per_user));
            ta_sy.push(Self::make_token_account(&mut ctx, sy_mint, u.pubkey(), 0));
            ta_pt.push(Self::make_token_account(&mut ctx, mint_pt, u.pubkey(), 0));
            ta_yt.push(Self::make_token_account(&mut ctx, mint_yt, u.pubkey(), 0));
            ta_emission.push(Self::make_token_account(&mut ctx, emission_mint, u.pubkey(), 0));
            let (yp, _) = Pubkey::find_program_address(
                &[b"yield_position", vault.as_ref(), u.pubkey().as_ref()], &program_id);
            yield_position.push(yp);
        }
        // Per-user destinations for every OTHER stream. Row 0 is `ta_emission`, so the vector and
        // the scalar always agree.
        let mut ta_emissions: Vec<Vec<Pubkey>> = vec![ta_emission.clone()];
        for s in 1..N_EMISSION_STREAMS {
            let mut row = Vec::with_capacity(N_USERS);
            for u in &users {
                row.push(Self::make_token_account(&mut ctx, emission_mints[s], u.pubkey(), 0));
            }
            ta_emissions.push(row);
        }

        // Pre-create every position EXCEPT the last actor's, so the value-flow actions work from
        // iteration 0 while `initialize_yield_position` still has something real to initialize.
        for i in 0..N_USERS - 1 {
            Self::run_initialize_yield_position(&mut ctx, program_id, &users[i], vault,
                                                yield_position[i]);
        }

        // ---- 9. seed the market ---------------------------------------------------------------
        // The admin must hold real PT and SY to seed it. PT is obtained by actually stripping --
        // never by fabricating a token balance, which would desynchronise `mint_pt.supply` and
        // `Vault.pt_supply` and silently invalidate every conservation property we intend to write.
        let payer_base = Self::make_token_account(&mut ctx, base_mint, payer.pubkey(),
                                                  initial_base_per_user);
        let payer_sy = Self::make_token_account(&mut ctx, sy_mint, payer.pubkey(), 0);
        let payer_pt = Self::make_token_account(&mut ctx, mint_pt, payer.pubkey(), 0);
        let payer_yt = Self::make_token_account(&mut ctx, mint_yt, payer.pubkey(), 0);
        let payer_lp = Self::ata(&payer.pubkey(), &mint_lp);

        let seed_sy = (MARKET_PT_INIT + MARKET_SY_INIT) * 4;
        Self::run_mock_sy_mint(&mut ctx, &payer, sy_program_id, sy_global, payer_base,
                               base_custody, sy_mint, payer_sy, seed_sy);
        Self::run_strip(&mut ctx, program_id, &payer, vault, vault_authority, payer_sy, escrow_sy,
                        payer_yt, payer_pt, mint_yt, mint_pt, alt, sy_program_id,
                        vault_yield_position, sy_global, vault_sy_position, sy_custody,
                        sy_authority, MARKET_PT_INIT);
        Self::run_init_market_two(
            &mut ctx, program_id, &payer, market, vault, sy_mint, mint_pt, mint_lp,
            market_escrow_pt, market_escrow_sy, market_escrow_lp, payer_pt, payer_sy, payer_lp,
            sy_program_id, alt, admin_account, token_treasury_fee_sy, market_sy_position,
            sy_global, sy_custody,
        );

        // ---- 9b. farm / market-emission reward token --------------------------------------------
        // `token_farm` must be the ATA of the MARKET (`add_farm.rs:29-33`,
        // `add_market_emission.rs:28-33` both use `associated_token::authority = market`), so it can
        // only be derived once the market key exists. Both instructions reuse the same account.
        // farm_mint is created earlier, with the emission streams (it IS stream 2's mint).
        let token_farm = Self::ata(&market, &farm_mint);
        ctx.create_token_account().pubkey(token_farm).mint(farm_mint).token_owner(market)
            .amount(0).create().unwrap();
        let token_farm_source =
            Self::make_token_account(&mut ctx, farm_mint, payer.pubkey(), 1_000_000_000_000);

        // ---- 10. per-user LP handles ------------------------------------------------------------
        // `init_lp_position` ranks LOW, so the generator gates its action behind `admin_actions`
        // and it is never fuzzed; creating every position here is therefore the right trade, not a
        // lost action. LP token accounts are minted directly at their ATA addresses -- `mint_lp`
        // already exists by now, so this changes no supply, only who can hold it.
        let mut ta_lp = Vec::new();
        let mut ta_farm = Vec::new();
        let mut lp_position = Vec::new();
        for u in &users {
            let lp_ta = Self::ata(&u.pubkey(), &mint_lp);
            ctx.create_token_account().pubkey(lp_ta).mint(mint_lp).token_owner(u.pubkey())
                .amount(0).create().unwrap();
            ta_lp.push(lp_ta);
            ta_farm.push(Self::make_token_account(&mut ctx, farm_mint, u.pubkey(), 0));
            let (lp, _) = Pubkey::find_program_address(
                &[b"lp_position", market.as_ref(), u.pubkey().as_ref()], &program_id);
            Self::run_init_lp_position(&mut ctx, program_id, &payer, u, market, lp);
            lp_position.push(lp);
        }


        // The struct construction must live INSIDE this region: once SCOUT:FIELDS is non-empty the
        // generator can no longer emit a correct `Self { .. }` tail, so it emits none and the glue
        // region is the function's tail expression.
        let mut fixture = Self {
            ctx, program_id, payer, users,
            sy_program_id, sy_global, sy_authority, sy_custody, base_custody, vault_sy_position,
            base_mint, sy_mint, mint_pt, mint_yt,
            admin_account, vault, vault_authority, escrow_yt, escrow_sy, treasury_sy_ta,
            vault_yield_position, alt,
            ta_base, ta_sy, ta_pt, ta_yt, yield_position,
            emission_mint, emission_custody, emission_escrow, treasury_emission_ta, ta_emission,
            emission_mints, emission_custodies, emission_escrows, treasury_emission_tas,
            ta_emissions,
            farm_mint, token_farm, token_farm_source, ta_farm,
            market, mint_lp, market_escrow_pt, market_escrow_sy, market_escrow_lp,
            market_sy_position, token_treasury_fee_sy, lp_metadata,
            payer_base, payer_sy, payer_pt, payer_lp,
            ta_lp, lp_position,
            vault_start_ts, vault_duration, initial_base_per_user,
            actor: 0,
            current_ts: vault_start_ts + 1,
            // Filled in immediately below -- they need `&self` readers, so they cannot be computed
            // inside the struct literal that creates the `self` they read from.
            baseline_sy_rate: 0,
            baseline_ath: 0,
            baseline_market_pt: 0,
            baseline_market_sy: 0,
            baseline_market_sy_position: 0,
            baseline_lp_supply: 0,
            baseline_adversary_py: 0,
            sy_rate_moved: false,
            ath_seen: 0,
            ts_seen: vault_start_ts + 1,
            emission_index_seen: vec![0; N_EMISSION_STREAMS],
            emission_final_seen: vec![0; N_EMISSION_STREAMS],
            yt_pt_gap_seen: 0,
        };

        // ---- 11. P-0004 / P-0007 baselines ------------------------------------------------------
        // Captured AFTER the world is fully built, so "the adversary's starting value" means their
        // value in the state every fuzz iteration actually begins from -- not some earlier
        // half-constructed state that no action sequence can ever return to.
        fixture.baseline_sy_rate = fixture.sy_exchange_rate();
        fixture.baseline_ath = fixture.vault_ath();
        fixture.ath_seen = fixture.baseline_ath;
        fixture.baseline_market_pt = fixture.ctx.token_balance(&fixture.market_escrow_pt);
        fixture.baseline_market_sy = fixture.ctx.token_balance(&fixture.market_escrow_sy);
        fixture.baseline_market_sy_position = fixture.market_sy_position_balance();
        fixture.baseline_lp_supply = fixture.mint_supply(&fixture.mint_lp).unwrap_or(0);
        fixture.baseline_adversary_py = fixture.adversary_value_py();

        // ---- 12. exclude ONE instruction from account-mutation probing ------------------------
        // `add_lp_tokens_metadata` is the only instruction here that carries the RENT SYSVAR as a
        // named account (Metaplex requires it). The `--mutate-accounts` engine's sysvar class
        // mutates it -- correctly, that is the check it exists to make -- but LiteSVM then reads
        // the corrupted Rent in its OWN `validate_fee_payer`, before the program runs, and
        // `Rent::minimum_balance` panics ("Maximum permitted data length exceeded",
        // solana-rent-4.3.0/src/lib.rs:120). That aborts the entire sweep in under 30s and no
        // finding is ever reported, for any instruction.
        //
        // Keyed by (program, discriminator), so this skips exactly that one instruction and leaves
        // the other 41 fully probed. Discriminator 41 is `add_lp_tokens_metadata`
        // (Anchor 0.31 custom 1-byte discriminators, per the IDL).
        //
        // NOTE this loses sysvar-validation coverage for that instruction specifically. It is the
        // narrowest available exclusion: the engine has no per-account skip.
        {
            let skip = Instruction { program_id, accounts: vec![], data: vec![41u8] };
            fixture.ctx.skip_account_mutation_for(&skip);
        }

        fixture
        // SCOUT:SETUP-GLUE:END
    }

    #[cfg(feature = "admin_actions")]
    pub fn action_initialize_vault(&mut self, start_timestamp: u32, duration: u32, interest_bps_fee: u16, min_op_size_strip: u64, min_op_size_merge: u64) -> bool {
        // SCOUT-TODO: arg cpi_accounts: exponent_core::types::CpiAccounts; arg pt_metadata_name: String; arg pt_metadata_symbol: String; arg pt_metadata_uri: String; remaining_accounts: reads ctx.remaining_accounts (src/instructions/vault/admin/initialize_vault.rs:303)
        let cpi_accounts: exponent_core::types::CpiAccounts = Default::default(); // SCOUT-TODO: construct arg cpi_accounts: exponent_core::types::CpiAccounts
        let pt_metadata_name: String = String::new(); // SCOUT-TODO: value for arg pt_metadata_name: String
        let pt_metadata_symbol: String = String::new(); // SCOUT-TODO: value for arg pt_metadata_symbol: String
        let pt_metadata_uri: String = String::new(); // SCOUT-TODO: value for arg pt_metadata_uri: String
        let __scout_signer_payer = self.users[self.actor].insecure_clone();
        let payer = __scout_signer_payer.pubkey();
        let admin = self.admin_account;
        let vault = self.vault;
        let mint_pt = self.mint_pt;
        let mint_yt = self.mint_yt;
        let escrow_yt = self.escrow_yt;
        let escrow_sy = self.escrow_sy;
        let mint_sy = self.sy_mint;
        let system_program = system_program::ID;
        let token_program = SPL_TOKEN_ID;
        let treasury_token_account = self.treasury_sy_ta;
        let associated_token_program = ASSOCIATED_TOKEN_ID;
        let sy_program = self.sy_program_id;
        let address_lookup_table = self.alt;
        let metadata = self.lp_metadata;
        let token_metadata_program = MPL_TOKEN_METADATA_ID;
        let authority = self.vault_authority;
        let yield_position = self.vault_yield_position;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::InitializeVault { start_timestamp, duration, interest_bps_fee, cpi_accounts, min_op_size_strip, min_op_size_merge, pt_metadata_name, pt_metadata_symbol, pt_metadata_uri })
            .accounts(accounts::InitializeVault {
                payer: payer,
                admin: admin,
                authority: authority,
                vault: vault,
                mint_pt: mint_pt,
                mint_yt: mint_yt,
                escrow_yt: escrow_yt,
                escrow_sy: escrow_sy,
                mint_sy: mint_sy,
                system_program: system_program,
                token_program: token_program,
                treasury_token_account: treasury_token_account,
                associated_token_program: associated_token_program,
                sy_program: sy_program,
                address_lookup_table: address_lookup_table,
                yield_position: yield_position,
                metadata: metadata,
                token_metadata_program: token_metadata_program,
            })
            // SCOUT-TODO: InitializeVault reads ctx.remaining_accounts (src/instructions/vault/admin/initialize_vault.rs:303); until they are supplied this
            // instruction fails account validation BEFORE its handler runs -- it executes no logic and covers no lines.
            // Bind `InitializeVault.remaining_accounts = vec![..]` in SCOUT:BINDINGS (Vec<Pubkey>, appended read-only;
            // prefix the value with `metas:` for a Vec<AccountMeta>). Do not guess -- supply the real accounts.
            .signers(&[&*self.payer, &__scout_signer_payer])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:initialize_vault:BEGIN
            // update shadow-ledger state after successful initialize_vault
            // SCOUT:ACTION-HOOK:initialize_vault:END
        }
        __scout_success
    }
    #[cfg(not(feature = "admin_actions"))]
    pub fn action_initialize_vault(&mut self, start_timestamp: u32, duration: u32, interest_bps_fee: u16, min_op_size_strip: u64, min_op_size_merge: u64) -> bool {
        // disabled: build with --features admin_actions to enable
        false
    }

    #[cfg(feature = "admin_actions")]
    pub fn action_initialize_yield_position(&mut self) -> bool {
        let __scout_signer_owner = self.users[self.actor].insecure_clone();
        let owner = __scout_signer_owner.pubkey();
        let vault = self.vault;
        let yield_position = self.yield_position[self.actor];
        let system_program = system_program::ID;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::InitializeYieldPosition {  })
            .accounts(accounts::InitializeYieldPosition {
                owner: owner,
                vault: vault,
                yield_position: yield_position,
                system_program: system_program,
                event_authority: event_authority,
                program: program,
            })
            .signers(&[&*self.payer, &__scout_signer_owner])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:initialize_yield_position:BEGIN
            // update shadow-ledger state after successful initialize_yield_position
            // SCOUT:ACTION-HOOK:initialize_yield_position:END
        }
        __scout_success
    }
    #[cfg(not(feature = "admin_actions"))]
    pub fn action_initialize_yield_position(&mut self) -> bool {
        // disabled: build with --features admin_actions to enable
        false
    }

    pub fn action_strip(&mut self) -> bool {
        let amount: u64 = { let b = self.ctx.token_balance(&self.ta_sy[self.actor]); (b / 4).max(1) };
        let __scout_signer_depositor = self.users[self.actor].insecure_clone();
        let depositor = __scout_signer_depositor.pubkey();
        let authority = self.vault_authority;
        let vault = self.vault;
        let sy_src = self.ta_sy[self.actor];
        let escrow_sy = self.escrow_sy;
        let yt_dst = self.ta_yt[self.actor];
        let pt_dst = self.ta_pt[self.actor];
        let mint_yt = self.mint_yt;
        let mint_pt = self.mint_pt;
        let token_program = SPL_TOKEN_ID;
        let address_lookup_table = self.alt;
        let sy_program = self.sy_program_id;
        let yield_position = self.vault_yield_position;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::Strip { amount })
            .accounts(accounts::Strip {
                depositor: depositor,
                authority: authority,
                vault: vault,
                sy_src: sy_src,
                escrow_sy: escrow_sy,
                yt_dst: yt_dst,
                pt_dst: pt_dst,
                mint_yt: mint_yt,
                mint_pt: mint_pt,
                token_program: token_program,
                address_lookup_table: address_lookup_table,
                sy_program: sy_program,
                yield_position: yield_position,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)])
            .signers(&[&*self.payer, &__scout_signer_depositor])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:strip:BEGIN
            // update shadow-ledger state after successful strip
            // SCOUT:ACTION-HOOK:strip:END
        }
        __scout_success
    }

    pub fn action_merge(&mut self) -> bool {
        let amount: u64 = { let p = self.ctx.token_balance(&self.ta_pt[self.actor]); let y = self.ctx.token_balance(&self.ta_yt[self.actor]); (p.min(y) / 2).max(1) };
        let __scout_signer_owner = self.users[self.actor].insecure_clone();
        let owner = __scout_signer_owner.pubkey();
        let authority = self.vault_authority;
        let vault = self.vault;
        let sy_dst = self.ta_sy[self.actor];
        let escrow_sy = self.escrow_sy;
        let yt_src = self.ta_yt[self.actor];
        let pt_src = self.ta_pt[self.actor];
        let mint_yt = self.mint_yt;
        let mint_pt = self.mint_pt;
        let token_program = SPL_TOKEN_ID;
        let sy_program = self.sy_program_id;
        let address_lookup_table = self.alt;
        let yield_position = self.vault_yield_position;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::Merge { amount })
            .accounts(accounts::Merge {
                owner: owner,
                authority: authority,
                vault: vault,
                sy_dst: sy_dst,
                escrow_sy: escrow_sy,
                yt_src: yt_src,
                pt_src: pt_src,
                mint_yt: mint_yt,
                mint_pt: mint_pt,
                token_program: token_program,
                sy_program: sy_program,
                address_lookup_table: address_lookup_table,
                yield_position: yield_position,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)])
            .signers(&[&*self.payer, &__scout_signer_owner])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:merge:BEGIN
            // update shadow-ledger state after successful merge
            // SCOUT:ACTION-HOOK:merge:END
        }
        __scout_success
    }

    pub fn action_collect_interest(&mut self) -> bool {
        let amount: exponent_core::types::Amount = exponent_core::types::Amount::All;
        let __scout_signer_owner = self.users[self.actor].insecure_clone();
        let owner = __scout_signer_owner.pubkey();
        let yield_position = self.yield_position[self.actor];
        let vault = self.vault;
        let token_sy_dst = self.ta_sy[self.actor];
        let escrow_sy = self.escrow_sy;
        let authority = self.vault_authority;
        let token_program = SPL_TOKEN_ID;
        let sy_program = self.sy_program_id;
        let treasury_sy_token_account = self.treasury_sy_ta;
        let address_lookup_table = self.alt;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::CollectInterest { amount })
            .accounts(accounts::CollectInterest {
                owner: owner,
                yield_position: yield_position,
                vault: vault,
                token_sy_dst: token_sy_dst,
                escrow_sy: escrow_sy,
                authority: authority,
                token_program: token_program,
                sy_program: sy_program,
                treasury_sy_token_account: treasury_sy_token_account,
                address_lookup_table: address_lookup_table,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)])
            .signers(&[&*self.payer, &__scout_signer_owner])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:collect_interest:BEGIN
            // update shadow-ledger state after successful collect_interest
            // SCOUT:ACTION-HOOK:collect_interest:END
        }
        __scout_success
    }

    pub fn action_deposit_yt(&mut self) -> bool {
        let amount: u64 = { let b = self.ctx.token_balance(&self.ta_yt[self.actor]); (b / 2).max(1) };
        let __scout_signer_depositor = self.users[self.actor].insecure_clone();
        let depositor = __scout_signer_depositor.pubkey();
        let vault = self.vault;
        let user_yield_position = self.yield_position[self.actor];
        let yt_src = self.ta_yt[self.actor];
        let escrow_yt = self.escrow_yt;
        let token_program = SPL_TOKEN_ID;
        let sy_program = self.sy_program_id;
        let address_lookup_table = self.alt;
        let yield_position = self.vault_yield_position;
        let system_program = system_program::ID;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::DepositYt { amount })
            .accounts(accounts::DepositYt {
                depositor: depositor,
                vault: vault,
                user_yield_position: user_yield_position,
                yt_src: yt_src,
                escrow_yt: escrow_yt,
                token_program: token_program,
                sy_program: sy_program,
                address_lookup_table: address_lookup_table,
                yield_position: yield_position,
                system_program: system_program,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)])
            .signers(&[&*self.payer, &__scout_signer_depositor])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:deposit_yt:BEGIN
            // update shadow-ledger state after successful deposit_yt
            // SCOUT:ACTION-HOOK:deposit_yt:END
        }
        __scout_success
    }

    pub fn action_withdraw_yt(&mut self) -> bool {
        let amount: u64 = { let b = self.position_yt_balance(self.actor); (b / 2).max(1) };
        let __scout_signer_owner = self.users[self.actor].insecure_clone();
        let owner = __scout_signer_owner.pubkey();
        let vault = self.vault;
        let user_yield_position = self.yield_position[self.actor];
        let yt_dst = self.ta_yt[self.actor];
        let escrow_yt = self.escrow_yt;
        let token_program = SPL_TOKEN_ID;
        let authority = self.vault_authority;
        let sy_program = self.sy_program_id;
        let address_lookup_table = self.alt;
        let yield_position = self.vault_yield_position;
        let system_program = system_program::ID;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::WithdrawYt { amount })
            .accounts(accounts::WithdrawYt {
                owner: owner,
                vault: vault,
                user_yield_position: user_yield_position,
                yt_dst: yt_dst,
                escrow_yt: escrow_yt,
                token_program: token_program,
                authority: authority,
                sy_program: sy_program,
                address_lookup_table: address_lookup_table,
                yield_position: yield_position,
                system_program: system_program,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)])
            .signers(&[&*self.payer, &__scout_signer_owner])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:withdraw_yt:BEGIN
            // update shadow-ledger state after successful withdraw_yt
            // SCOUT:ACTION-HOOK:withdraw_yt:END
        }
        __scout_success
    }

    pub fn action_stage_yt_yield(&mut self) -> bool {
        let __scout_signer_payer = self.users[self.actor].insecure_clone();
        let payer = __scout_signer_payer.pubkey();
        let vault = self.vault;
        let user_yield_position = self.yield_position[self.actor];
        let yield_position = self.vault_yield_position;
        let sy_program = self.sy_program_id;
        let address_lookup_table = self.alt;
        let system_program = system_program::ID;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::StageYtYield {  })
            .accounts(accounts::StageYtYield {
                payer: payer,
                vault: vault,
                user_yield_position: user_yield_position,
                yield_position: yield_position,
                sy_program: sy_program,
                address_lookup_table: address_lookup_table,
                system_program: system_program,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)])
            .signers(&[&*self.payer, &__scout_signer_payer])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:stage_yt_yield:BEGIN
            // update shadow-ledger state after successful stage_yt_yield
            // SCOUT:ACTION-HOOK:stage_yt_yield:END
        }
        __scout_success
    }

    #[cfg(feature = "admin_actions")]
    pub fn action_init_market_two(&mut self, pt_init: u64, sy_init: u64, fee_treasury_sy_bps: u16, seed_id: u8) -> bool {
        // SCOUT-TODO: arg ln_fee_rate_root: f64; arg rate_scalar_root: f64; arg init_rate_anchor: f64; arg sy_exchange_rate: exponent_core::types::Number; arg cpi_accounts: exponent_core::types::CpiAccounts; account escrow_pt (unchecked); account escrow_lp (unchecked); account lp_dst (unchecked); 1 extra signer(s): ['admin_signer']; remaining_accounts: reads ctx.remaining_accounts (src/instructions/market_two/admin/market_two_init.rs:389)
        let ln_fee_rate_root: f64 = Default::default(); // SCOUT-TODO: construct arg ln_fee_rate_root: f64
        let rate_scalar_root: f64 = Default::default(); // SCOUT-TODO: construct arg rate_scalar_root: f64
        let init_rate_anchor: f64 = Default::default(); // SCOUT-TODO: construct arg init_rate_anchor: f64
        let sy_exchange_rate: exponent_core::types::Number = Default::default(); // SCOUT-TODO: construct arg sy_exchange_rate: exponent_core::types::Number
        let cpi_accounts: exponent_core::types::CpiAccounts = Default::default(); // SCOUT-TODO: construct arg cpi_accounts: exponent_core::types::CpiAccounts
        let __scout_signer_payer = self.users[self.actor].insecure_clone();
        let payer = __scout_signer_payer.pubkey();
        let admin_signer = self.payer.pubkey();
        let vault = self.vault;
        let mint_sy = self.sy_mint;
        let mint_pt = self.mint_pt;
        let mint_lp = self.mint_lp;
        let escrow_pt = self.scout_placeholder(); // SCOUT-TODO: real account for escrow_pt (unchecked)
        let escrow_sy = self.escrow_sy;
        let escrow_lp = self.scout_placeholder(); // SCOUT-TODO: real account for escrow_lp (unchecked)
        let pt_src = self.ta_pt[self.actor];
        let sy_src = self.ta_sy[self.actor];
        let lp_dst = self.scout_placeholder(); // SCOUT-TODO: real account for lp_dst (unchecked)
        let token_program = SPL_TOKEN_ID;
        let system_program = system_program::ID;
        let sy_program = self.sy_program_id;
        let associated_token_program = ASSOCIATED_TOKEN_ID;
        let address_lookup_table = self.alt;
        let admin = self.admin_account;
        let token_treasury_fee_sy = self.token_treasury_fee_sy;
        let market = self.market;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::InitMarketTwo { ln_fee_rate_root, rate_scalar_root, init_rate_anchor, sy_exchange_rate, pt_init, sy_init, fee_treasury_sy_bps, cpi_accounts, seed_id })
            .accounts(accounts::InitMarketTwo {
                payer: payer,
                admin_signer: admin_signer,
                market: market,
                vault: vault,
                mint_sy: mint_sy,
                mint_pt: mint_pt,
                mint_lp: mint_lp,
                escrow_pt: escrow_pt,
                escrow_sy: escrow_sy,
                escrow_lp: escrow_lp,
                pt_src: pt_src,
                sy_src: sy_src,
                lp_dst: lp_dst,
                token_program: token_program,
                system_program: system_program,
                sy_program: sy_program,
                associated_token_program: associated_token_program,
                address_lookup_table: address_lookup_table,
                admin: admin,
                token_treasury_fee_sy: token_treasury_fee_sy,
            })
            // SCOUT-TODO: InitMarketTwo reads ctx.remaining_accounts (src/instructions/market_two/admin/market_two_init.rs:389 +1 more); until they are supplied this
            // instruction fails account validation BEFORE its handler runs -- it executes no logic and covers no lines.
            // Bind `InitMarketTwo.remaining_accounts = vec![..]` in SCOUT:BINDINGS (Vec<Pubkey>, appended read-only;
            // prefix the value with `metas:` for a Vec<AccountMeta>). Do not guess -- supply the real accounts.
            .signers(&[&*self.payer, &__scout_signer_payer])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:init_market_two:BEGIN
            // update shadow-ledger state after successful init_market_two
            // SCOUT:ACTION-HOOK:init_market_two:END
        }
        __scout_success
    }
    #[cfg(not(feature = "admin_actions"))]
    pub fn action_init_market_two(&mut self, pt_init: u64, sy_init: u64, fee_treasury_sy_bps: u16, seed_id: u8) -> bool {
        // disabled: build with --features admin_actions to enable
        false
    }

    pub fn action_market_two_deposit_liquidity(&mut self) -> bool {
        let pt_intent: u64 = { let b = self.ctx.token_balance(&self.ta_pt[self.actor]); (b / 8).max(1) };
        let sy_intent: u64 = { let b = self.ctx.token_balance(&self.ta_sy[self.actor]); (b / 8).max(1) };
        let min_lp_out: u64 = 0;
        let __scout_signer_depositor = self.users[self.actor].insecure_clone();
        let depositor = __scout_signer_depositor.pubkey();
        let market = self.market;
        let token_pt_src = self.ta_pt[self.actor];
        let token_sy_src = self.ta_sy[self.actor];
        let token_pt_escrow = self.market_escrow_pt;
        let token_sy_escrow = self.market_escrow_sy;
        let token_lp_dst = self.ta_lp[self.actor];
        let mint_lp = self.mint_lp;
        let address_lookup_table = self.alt;
        let token_program = SPL_TOKEN_ID;
        let sy_program = self.sy_program_id;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::MarketTwoDepositLiquidity { pt_intent, sy_intent, min_lp_out })
            .accounts(accounts::MarketTwoDepositLiquidity {
                depositor: depositor,
                market: market,
                token_pt_src: token_pt_src,
                token_sy_src: token_sy_src,
                token_pt_escrow: token_pt_escrow,
                token_sy_escrow: token_sy_escrow,
                token_lp_dst: token_lp_dst,
                mint_lp: mint_lp,
                address_lookup_table: address_lookup_table,
                token_program: token_program,
                sy_program: sy_program,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.market_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)])
            .signers(&[&*self.payer, &__scout_signer_depositor])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:market_two_deposit_liquidity:BEGIN
            // update shadow-ledger state after successful market_two_deposit_liquidity
            // SCOUT:ACTION-HOOK:market_two_deposit_liquidity:END
        }
        __scout_success
    }

    pub fn action_market_two_withdraw_liquidity(&mut self) -> bool {
        let lp_in: u64 = { let b = self.ctx.token_balance(&self.ta_lp[self.actor]); (b / 4).max(1) };
        let min_pt_out: u64 = 0;
        let min_sy_out: u64 = 0;
        let __scout_signer_withdrawer = self.users[self.actor].insecure_clone();
        let withdrawer = __scout_signer_withdrawer.pubkey();
        let market = self.market;
        let token_pt_dst = self.ta_pt[self.actor];
        let token_sy_dst = self.ta_sy[self.actor];
        let token_pt_escrow = self.market_escrow_pt;
        let token_sy_escrow = self.market_escrow_sy;
        let token_lp_src = self.ta_lp[self.actor];
        let mint_lp = self.mint_lp;
        let address_lookup_table = self.alt;
        let token_program = SPL_TOKEN_ID;
        let sy_program = self.sy_program_id;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::MarketTwoWithdrawLiquidity { lp_in, min_pt_out, min_sy_out })
            .accounts(accounts::MarketTwoWithdrawLiquidity {
                withdrawer: withdrawer,
                market: market,
                token_pt_dst: token_pt_dst,
                token_sy_dst: token_sy_dst,
                token_pt_escrow: token_pt_escrow,
                token_sy_escrow: token_sy_escrow,
                token_lp_src: token_lp_src,
                mint_lp: mint_lp,
                address_lookup_table: address_lookup_table,
                token_program: token_program,
                sy_program: sy_program,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.market_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)])
            .signers(&[&*self.payer, &__scout_signer_withdrawer])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:market_two_withdraw_liquidity:BEGIN
            // update shadow-ledger state after successful market_two_withdraw_liquidity
            // SCOUT:ACTION-HOOK:market_two_withdraw_liquidity:END
        }
        __scout_success
    }

    #[cfg(feature = "admin_actions")]
    pub fn action_init_lp_position(&mut self) -> bool {
        let __scout_signer_fee_payer = self.payer.insecure_clone();
        let fee_payer = __scout_signer_fee_payer.pubkey();
        let __scout_signer_owner = self.users[self.actor].insecure_clone();
        let owner = __scout_signer_owner.pubkey();
        let market = self.market;
        let lp_position = self.lp_position[self.actor];
        let system_program = system_program::ID;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::InitLpPosition {  })
            .accounts(accounts::InitLpPosition {
                fee_payer: fee_payer,
                owner: owner,
                market: market,
                lp_position: lp_position,
                system_program: system_program,
                event_authority: event_authority,
                program: program,
            })
            .signers(&[&*self.payer, &__scout_signer_fee_payer, &__scout_signer_owner])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:init_lp_position:BEGIN
            // update shadow-ledger state after successful init_lp_position
            // SCOUT:ACTION-HOOK:init_lp_position:END
        }
        __scout_success
    }
    #[cfg(not(feature = "admin_actions"))]
    pub fn action_init_lp_position(&mut self) -> bool {
        // disabled: build with --features admin_actions to enable
        false
    }

    pub fn action_market_deposit_lp(&mut self) -> bool {
        let amount: u64 = { let b = self.ctx.token_balance(&self.ta_lp[self.actor]); (b / 2).max(1) };
        let __scout_signer_owner = self.users[self.actor].insecure_clone();
        let owner = __scout_signer_owner.pubkey();
        let market = self.market;
        let lp_position = self.lp_position[self.actor];
        let token_lp_src = self.ta_lp[self.actor];
        let token_lp_escrow = self.market_escrow_lp;
        let mint_lp = self.mint_lp;
        let sy_program = self.sy_program_id;
        let address_lookup_table = self.alt;
        let token_program = SPL_TOKEN_ID;
        let system_program = system_program::ID;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::MarketDepositLp { amount })
            .accounts(accounts::MarketDepositLp {
                owner: owner,
                market: market,
                lp_position: lp_position,
                token_lp_src: token_lp_src,
                token_lp_escrow: token_lp_escrow,
                mint_lp: mint_lp,
                sy_program: sy_program,
                address_lookup_table: address_lookup_table,
                token_program: token_program,
                system_program: system_program,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.market_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)])
            .signers(&[&*self.payer, &__scout_signer_owner])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:market_deposit_lp:BEGIN
            // update shadow-ledger state after successful market_deposit_lp
            // SCOUT:ACTION-HOOK:market_deposit_lp:END
        }
        __scout_success
    }

    pub fn action_market_withdraw_lp(&mut self) -> bool {
        let amount: u64 = { let b = self.position_lp_balance(self.actor); (b / 2).max(1) };
        let __scout_signer_owner = self.users[self.actor].insecure_clone();
        let owner = __scout_signer_owner.pubkey();
        let market = self.market;
        let mint_lp = self.mint_lp;
        let lp_position = self.lp_position[self.actor];
        let token_lp_dst = self.ta_lp[self.actor];
        let token_lp_escrow = self.market_escrow_lp;
        let sy_program = self.sy_program_id;
        let address_lookup_table = self.alt;
        let token_program = SPL_TOKEN_ID;
        let system_program = system_program::ID;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::MarketWithdrawLp { amount })
            .accounts(accounts::MarketWithdrawLp {
                owner: owner,
                market: market,
                mint_lp: mint_lp,
                lp_position: lp_position,
                token_lp_dst: token_lp_dst,
                token_lp_escrow: token_lp_escrow,
                sy_program: sy_program,
                address_lookup_table: address_lookup_table,
                token_program: token_program,
                system_program: system_program,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.market_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)])
            .signers(&[&*self.payer, &__scout_signer_owner])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:market_withdraw_lp:BEGIN
            // update shadow-ledger state after successful market_withdraw_lp
            // SCOUT:ACTION-HOOK:market_withdraw_lp:END
        }
        __scout_success
    }

    pub fn action_market_collect_emission(&mut self) -> bool {
        let emission_index: u16 = 0;
        let __scout_signer_owner = self.users[self.actor].insecure_clone();
        let owner = __scout_signer_owner.pubkey();
        let market = self.market;
        let lp_position = self.lp_position[self.actor];
        let token_emission_escrow = self.token_farm;
        let token_emission_dst = self.ta_farm[self.actor];
        let token_program = SPL_TOKEN_ID;
        let address_lookup_table = self.alt;
        let sy_program = self.sy_program_id;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::MarketCollectEmission { emission_index })
            .accounts(accounts::MarketCollectEmission {
                owner: owner,
                market: market,
                lp_position: lp_position,
                token_emission_escrow: token_emission_escrow,
                token_emission_dst: token_emission_dst,
                token_program: token_program,
                address_lookup_table: address_lookup_table,
                sy_program: sy_program,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.market_sy_position, false), AccountMeta::new(self.emission_custodies[MARKET_EMISSION_STREAM], false), AccountMeta::new(self.token_farm, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new_readonly(SPL_TOKEN_ID, false)])
            .signers(&[&*self.payer, &__scout_signer_owner])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:market_collect_emission:BEGIN
            // update shadow-ledger state after successful market_collect_emission
            // SCOUT:ACTION-HOOK:market_collect_emission:END
        }
        __scout_success
    }

    pub fn action_trade_pt(&mut self, net_trader_pt: i64) -> bool {
        let sy_constraint: i64 = i64::MIN;
        let __scout_signer_trader = self.users[self.actor].insecure_clone();
        let trader = __scout_signer_trader.pubkey();
        let market = self.market;
        let token_sy_trader = self.ta_sy[self.actor];
        let token_pt_trader = self.ta_pt[self.actor];
        let token_sy_escrow = self.market_escrow_sy;
        let token_pt_escrow = self.market_escrow_pt;
        let address_lookup_table = self.alt;
        let token_program = SPL_TOKEN_ID;
        let sy_program = self.sy_program_id;
        let token_fee_treasury_sy = self.token_treasury_fee_sy;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::TradePt { net_trader_pt, sy_constraint })
            .accounts(accounts::TradePt {
                trader: trader,
                market: market,
                token_sy_trader: token_sy_trader,
                token_pt_trader: token_pt_trader,
                token_sy_escrow: token_sy_escrow,
                token_pt_escrow: token_pt_escrow,
                address_lookup_table: address_lookup_table,
                token_program: token_program,
                sy_program: sy_program,
                token_fee_treasury_sy: token_fee_treasury_sy,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.market_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)])
            .signers(&[&*self.payer, &__scout_signer_trader])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:trade_pt:BEGIN
            // update shadow-ledger state after successful trade_pt
            // SCOUT:ACTION-HOOK:trade_pt:END
        }
        __scout_success
    }

    pub fn action_sell_yt(&mut self) -> bool {
        let yt_in: u64 = { let b = self.ctx.token_balance(&self.ta_yt[self.actor]); (b / 4).max(1) };
        let min_sy_out: u64 = 0;
        let __scout_signer_trader = self.users[self.actor].insecure_clone();
        let trader = __scout_signer_trader.pubkey();
        let market = self.market;
        let token_yt_trader = self.ta_yt[self.actor];
        let token_pt_trader = self.ta_pt[self.actor];
        let token_sy_trader = self.ta_sy[self.actor];
        let token_sy_escrow = self.market_escrow_sy;
        let token_pt_escrow = self.market_escrow_pt;
        let address_lookup_table = self.alt;
        let token_fee_treasury_sy = self.token_treasury_fee_sy;
        let token_program = SPL_TOKEN_ID;
        let vault = self.vault;
        let authority_vault = self.vault_authority;
        let token_sy_escrow_vault = self.escrow_sy;
        let mint_yt = self.mint_yt;
        let mint_pt = self.mint_pt;
        let address_lookup_table_vault = self.alt;
        let yield_position_vault = self.vault_yield_position;
        let sy_program = self.sy_program_id;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::SellYt { yt_in, min_sy_out })
            .accounts(accounts::SellYt {
                trader: trader,
                market: market,
                token_yt_trader: token_yt_trader,
                token_pt_trader: token_pt_trader,
                token_sy_trader: token_sy_trader,
                token_sy_escrow: token_sy_escrow,
                token_pt_escrow: token_pt_escrow,
                address_lookup_table: address_lookup_table,
                token_fee_treasury_sy: token_fee_treasury_sy,
                token_program: token_program,
                vault: vault,
                authority_vault: authority_vault,
                token_sy_escrow_vault: token_sy_escrow_vault,
                mint_yt: mint_yt,
                mint_pt: mint_pt,
                address_lookup_table_vault: address_lookup_table_vault,
                yield_position_vault: yield_position_vault,
                sy_program: sy_program,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.market_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)])
            .signers(&[&*self.payer, &__scout_signer_trader])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:sell_yt:BEGIN
            // update shadow-ledger state after successful sell_yt
            // SCOUT:ACTION-HOOK:sell_yt:END
        }
        __scout_success
    }

    pub fn action_buy_yt(&mut self) -> bool {
        let sy_in: u64 = { let m = self.ctx.token_balance(&self.market_escrow_pt); (m / 64).max(1) };
        let yt_out: u64 = { let m = self.ctx.token_balance(&self.market_escrow_pt); (m / 16).max(4) };
        let __scout_signer_trader = self.users[self.actor].insecure_clone();
        let trader = __scout_signer_trader.pubkey();
        let market = self.market;
        let token_sy_trader = self.ta_sy[self.actor];
        let token_yt_trader = self.ta_yt[self.actor];
        let token_pt_trader = self.ta_pt[self.actor];
        let token_sy_escrow = self.market_escrow_sy;
        let token_pt_escrow = self.market_escrow_pt;
        let token_fee_treasury_sy = self.token_treasury_fee_sy;
        let token_program = SPL_TOKEN_ID;
        let address_lookup_table = self.alt;
        let sy_program = self.sy_program_id;
        let vault_authority = self.vault_authority;
        let vault = self.vault;
        let token_sy_escrow_vault = self.escrow_sy;
        let mint_yt = self.mint_yt;
        let mint_pt = self.mint_pt;
        let address_lookup_table_vault = self.alt;
        let yield_position = self.vault_yield_position;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::BuyYt { sy_in, yt_out })
            .accounts(accounts::BuyYt {
                trader: trader,
                market: market,
                token_sy_trader: token_sy_trader,
                token_yt_trader: token_yt_trader,
                token_pt_trader: token_pt_trader,
                token_sy_escrow: token_sy_escrow,
                token_pt_escrow: token_pt_escrow,
                token_fee_treasury_sy: token_fee_treasury_sy,
                token_program: token_program,
                address_lookup_table: address_lookup_table,
                sy_program: sy_program,
                vault_authority: vault_authority,
                vault: vault,
                token_sy_escrow_vault: token_sy_escrow_vault,
                mint_yt: mint_yt,
                mint_pt: mint_pt,
                address_lookup_table_vault: address_lookup_table_vault,
                yield_position: yield_position,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.market_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)])
            .signers(&[&*self.payer, &__scout_signer_trader])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:buy_yt:BEGIN
            // update shadow-ledger state after successful buy_yt
            // SCOUT:ACTION-HOOK:buy_yt:END
        }
        __scout_success
    }

    pub fn action_add_emission(&mut self, treasury_fee_bps: u16) -> bool {
        // SCOUT-TODO: arg cpi_accounts: exponent_core::types::CpiAccounts; 1 extra signer(s): ['fee_payer']; remaining_accounts: reads ctx.remaining_accounts (src/instructions/vault/admin/add_emission.rs:78)
        let cpi_accounts: exponent_core::types::CpiAccounts = Default::default(); // SCOUT-TODO: construct arg cpi_accounts: exponent_core::types::CpiAccounts
        let authority = self.vault_authority;
        let __scout_signer_fee_payer = self.payer.insecure_clone();
        let fee_payer = __scout_signer_fee_payer.pubkey();
        let vault = self.vault;
        let admin = self.admin_account;
        let sy_program = self.sy_program_id;
        let address_lookup_table = self.alt;
        let robot_token_account = self.emission_escrow;
        let treasury_token_account = self.treasury_sy_ta;
        let yield_position = self.vault_yield_position;
        let system_program = system_program::ID;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::AddEmission { cpi_accounts, treasury_fee_bps })
            .accounts(accounts::AddEmission {
                authority: authority,
                fee_payer: fee_payer,
                vault: vault,
                admin: admin,
                sy_program: sy_program,
                address_lookup_table: address_lookup_table,
                robot_token_account: robot_token_account,
                treasury_token_account: treasury_token_account,
                yield_position: yield_position,
                system_program: system_program,
            })
            // SCOUT-TODO: AddEmission reads ctx.remaining_accounts (src/instructions/vault/admin/add_emission.rs:78); until they are supplied this
            // instruction fails account validation BEFORE its handler runs -- it executes no logic and covers no lines.
            // Bind `AddEmission.remaining_accounts = vec![..]` in SCOUT:BINDINGS (Vec<Pubkey>, appended read-only;
            // prefix the value with `metas:` for a Vec<AccountMeta>). Do not guess -- supply the real accounts.
            .signers(&[&*self.payer, &__scout_signer_fee_payer])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:add_emission:BEGIN
            // update shadow-ledger state after successful add_emission
            // SCOUT:ACTION-HOOK:add_emission:END
        }
        __scout_success
    }

    pub fn action_collect_emission(&mut self) -> bool {
        let index: u16 = 0;
        let amount: exponent_core::types::Amount = exponent_core::types::Amount::All;
        let __scout_signer_owner = self.users[self.actor].insecure_clone();
        let owner = __scout_signer_owner.pubkey();
        let vault = self.vault;
        let position = self.yield_position[self.actor];
        let sy_program = self.sy_program_id;
        let authority = self.vault_authority;
        let emission_escrow = self.emission_escrow;
        let emission_dst = self.ta_emission[self.actor];
        let address_lookup_table = self.alt;
        let treasury_emission_token_account = self.treasury_emission_ta;
        let token_program = SPL_TOKEN_ID;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::CollectEmission { index, amount })
            .accounts(accounts::CollectEmission {
                owner: owner,
                vault: vault,
                position: position,
                sy_program: sy_program,
                authority: authority,
                emission_escrow: emission_escrow,
                emission_dst: emission_dst,
                address_lookup_table: address_lookup_table,
                treasury_emission_token_account: treasury_emission_token_account,
                token_program: token_program,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.emission_custody, false), AccountMeta::new(self.emission_escrow, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new_readonly(SPL_TOKEN_ID, false)])
            .signers(&[&*self.payer, &__scout_signer_owner])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:collect_emission:BEGIN
            // update shadow-ledger state after successful collect_emission
            // SCOUT:ACTION-HOOK:collect_emission:END
        }
        __scout_success
    }

    pub fn action_collect_treasury_emission(&mut self) -> bool {
        // SCOUT-TODO: arg kind: exponent_core::types::CollectTreasuryEmissionKind
        let emission_index: u16 = 0;
        let amount: exponent_core::types::Amount = exponent_core::types::Amount::All;
        let kind: exponent_core::types::CollectTreasuryEmissionKind = Default::default(); // SCOUT-TODO: construct arg kind: exponent_core::types::CollectTreasuryEmissionKind
        let __scout_signer_signer = self.payer.insecure_clone();
        let signer = __scout_signer_signer.pubkey();
        let yield_position = self.vault_yield_position;
        let vault = self.vault;
        let sy_program = self.sy_program_id;
        let authority = self.vault_authority;
        let emission_escrow = self.emission_escrow;
        let emission_dst = self.ta_emission[self.actor];
        let address_lookup_table = self.alt;
        let token_program = SPL_TOKEN_ID;
        let admin = self.admin_account;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::CollectTreasuryEmission { emission_index, amount, kind })
            .accounts(accounts::CollectTreasuryEmission {
                signer: signer,
                yield_position: yield_position,
                vault: vault,
                sy_program: sy_program,
                authority: authority,
                emission_escrow: emission_escrow,
                emission_dst: emission_dst,
                address_lookup_table: address_lookup_table,
                token_program: token_program,
                admin: admin,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.emission_custody, false), AccountMeta::new(self.emission_escrow, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new_readonly(SPL_TOKEN_ID, false)])
            .signers(&[&*self.payer, &__scout_signer_signer])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:collect_treasury_emission:BEGIN
            // update shadow-ledger state after successful collect_treasury_emission
            // SCOUT:ACTION-HOOK:collect_treasury_emission:END
        }
        __scout_success
    }

    pub fn action_collect_treasury_interest(&mut self) -> bool {
        // SCOUT-TODO: arg kind: exponent_core::types::CollectTreasuryInterestKind
        let amount: exponent_core::types::Amount = exponent_core::types::Amount::All;
        let kind: exponent_core::types::CollectTreasuryInterestKind = Default::default(); // SCOUT-TODO: construct arg kind: exponent_core::types::CollectTreasuryInterestKind
        let __scout_signer_signer = self.payer.insecure_clone();
        let signer = __scout_signer_signer.pubkey();
        let yield_position = self.vault_yield_position;
        let vault = self.vault;
        let sy_dst = self.ta_sy[self.actor];
        let escrow_sy = self.escrow_sy;
        let authority = self.vault_authority;
        let token_program = SPL_TOKEN_ID;
        let sy_program = self.sy_program_id;
        let address_lookup_table = self.alt;
        let admin = self.admin_account;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::CollectTreasuryInterest { amount, kind })
            .accounts(accounts::CollectTreasuryInterest {
                signer: signer,
                yield_position: yield_position,
                vault: vault,
                sy_dst: sy_dst,
                escrow_sy: escrow_sy,
                authority: authority,
                token_program: token_program,
                sy_program: sy_program,
                address_lookup_table: address_lookup_table,
                admin: admin,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new(self.escrow_sy, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new_readonly(SPL_TOKEN_ID, false)])
            .signers(&[&*self.payer, &__scout_signer_signer])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:collect_treasury_interest:BEGIN
            // update shadow-ledger state after successful collect_treasury_interest
            // SCOUT:ACTION-HOOK:collect_treasury_interest:END
        }
        __scout_success
    }

    pub fn action_add_farm(&mut self, token_rate: u64) -> bool {
        // SCOUT-TODO: 1 extra signer(s): ['fee_payer']
        let until_timestamp: u32 = self.vault_start_ts + self.vault_duration / 2;
        let market = self.market;
        let __scout_signer_signer = self.payer.insecure_clone();
        let signer = __scout_signer_signer.pubkey();
        let __scout_signer_fee_payer = self.payer.insecure_clone();
        let fee_payer = __scout_signer_fee_payer.pubkey();
        let mint_new = self.farm_mint;
        let admin_state = self.admin_account;
        let token_source = self.token_farm_source;
        let token_farm = self.token_farm;
        let token_program = SPL_TOKEN_ID;
        let system_program = system_program::ID;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::AddFarm { token_rate, until_timestamp })
            .accounts(accounts::AddFarm {
                market: market,
                signer: signer,
                fee_payer: fee_payer,
                mint_new: mint_new,
                admin_state: admin_state,
                token_source: token_source,
                token_farm: token_farm,
                token_program: token_program,
                system_program: system_program,
            })
            .signers(&[&*self.payer, &__scout_signer_signer, &__scout_signer_fee_payer])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:add_farm:BEGIN
            // update shadow-ledger state after successful add_farm
            // SCOUT:ACTION-HOOK:add_farm:END
        }
        __scout_success
    }

    pub fn action_modify_farm(&mut self, new_rate: u64) -> bool {
        let until_timestamp: u32 = self.vault_start_ts + self.vault_duration / 2;
        let market = self.market;
        let __scout_signer_signer = self.payer.insecure_clone();
        let signer = __scout_signer_signer.pubkey();
        let mint = self.farm_mint;
        let admin_state = self.admin_account;
        let token_source = self.token_farm_source;
        let token_farm = self.token_farm;
        let token_program = SPL_TOKEN_ID;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::ModifyFarm { until_timestamp, new_rate })
            .accounts(accounts::ModifyFarm {
                market: market,
                signer: signer,
                mint: mint,
                admin_state: admin_state,
                token_source: token_source,
                token_farm: token_farm,
                token_program: token_program,
            })
            .signers(&[&*self.payer, &__scout_signer_signer])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:modify_farm:BEGIN
            // update shadow-ledger state after successful modify_farm
            // SCOUT:ACTION-HOOK:modify_farm:END
        }
        __scout_success
    }

    pub fn action_claim_farm_emissions(&mut self) -> bool {
        let amount: exponent_core::types::Amount = exponent_core::types::Amount::All;
        let __scout_signer_owner = self.users[self.actor].insecure_clone();
        let owner = __scout_signer_owner.pubkey();
        let market = self.market;
        let lp_position = self.lp_position[self.actor];
        let token_dst = self.ta_farm[self.actor];
        let mint = self.farm_mint;
        let token_farm = self.token_farm;
        let token_program = SPL_TOKEN_ID;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::ClaimFarmEmissions { amount })
            .accounts(accounts::ClaimFarmEmissions {
                owner: owner,
                market: market,
                lp_position: lp_position,
                token_dst: token_dst,
                mint: mint,
                token_farm: token_farm,
                token_program: token_program,
                event_authority: event_authority,
                program: program,
            })
            .signers(&[&*self.payer, &__scout_signer_owner])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:claim_farm_emissions:BEGIN
            // update shadow-ledger state after successful claim_farm_emissions
            // SCOUT:ACTION-HOOK:claim_farm_emissions:END
        }
        __scout_success
    }

    pub fn action_add_market_emission(&mut self) -> bool {
        // SCOUT-TODO: 1 extra signer(s): ['fee_payer']
        let cpi_accounts: exponent_core::types::CpiAccounts = Self::market_cpi_accounts();
        let market = self.market;
        let __scout_signer_signer = self.payer.insecure_clone();
        let signer = __scout_signer_signer.pubkey();
        let __scout_signer_fee_payer = self.payer.insecure_clone();
        let fee_payer = __scout_signer_fee_payer.pubkey();
        let mint_new = self.farm_mint;
        let admin_state = self.admin_account;
        let token_emission = self.token_farm;
        let token_program = SPL_TOKEN_ID;
        let system_program = system_program::ID;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::AddMarketEmission { cpi_accounts })
            .accounts(accounts::AddMarketEmission {
                market: market,
                signer: signer,
                fee_payer: fee_payer,
                mint_new: mint_new,
                admin_state: admin_state,
                token_emission: token_emission,
                token_program: token_program,
                system_program: system_program,
            })
            .signers(&[&*self.payer, &__scout_signer_signer, &__scout_signer_fee_payer])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:add_market_emission:BEGIN
            // update shadow-ledger state after successful add_market_emission
            // SCOUT:ACTION-HOOK:add_market_emission:END
        }
        __scout_success
    }

    pub fn action_modify_vault_setting(&mut self) -> bool {
        // SCOUT-TODO: arg action: exponent_core::types::AdminAction
        let action: exponent_core::types::AdminAction = Default::default(); // SCOUT-TODO: construct arg action: exponent_core::types::AdminAction
        let vault = self.vault;
        let __scout_signer_signer = self.payer.insecure_clone();
        let signer = __scout_signer_signer.pubkey();
        let admin_state = self.admin_account;
        let system_program = system_program::ID;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::ModifyVaultSetting { action })
            .accounts(accounts::ModifyVaultSetting {
                vault: vault,
                signer: signer,
                admin_state: admin_state,
                system_program: system_program,
            })
            .signers(&[&*self.payer, &__scout_signer_signer])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:modify_vault_setting:BEGIN
            // update shadow-ledger state after successful modify_vault_setting
            // SCOUT:ACTION-HOOK:modify_vault_setting:END
        }
        __scout_success
    }

    pub fn action_modify_market_setting(&mut self) -> bool {
        // SCOUT-TODO: arg action: exponent_core::types::MarketAdminAction
        let action: exponent_core::types::MarketAdminAction = Default::default(); // SCOUT-TODO: construct arg action: exponent_core::types::MarketAdminAction
        let market = self.market;
        let __scout_signer_signer = self.payer.insecure_clone();
        let signer = __scout_signer_signer.pubkey();
        let admin_state = self.admin_account;
        let system_program = system_program::ID;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::ModifyMarketSetting { action })
            .accounts(accounts::ModifyMarketSetting {
                market: market,
                signer: signer,
                admin_state: admin_state,
                system_program: system_program,
            })
            .signers(&[&*self.payer, &__scout_signer_signer])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:modify_market_setting:BEGIN
            // update shadow-ledger state after successful modify_market_setting
            // SCOUT:ACTION-HOOK:modify_market_setting:END
        }
        __scout_success
    }

    pub fn action_wrapper_provide_liquidity(&mut self) -> bool {
        let amount_base: u64 = { let m = self.ctx.token_balance(&self.market_escrow_pt); (m / 64).max(1) };
        let min_lp_out: u64 = 0;
        let mint_base_accounts_until: u8 = MINT_SY_ACCOUNTS;
        let __scout_signer_depositor = self.users[self.actor].insecure_clone();
        let depositor = __scout_signer_depositor.pubkey();
        let authority = self.vault_authority;
        let vault = self.vault;
        let market = self.market;
        let token_pt_escrow = self.market_escrow_pt;
        let token_sy_escrow = self.market_escrow_sy;
        let token_lp_dst = self.ta_lp[self.actor];
        let mint_lp = self.mint_lp;
        let token_sy_depositor = self.ta_sy[self.actor];
        let escrow_sy = self.escrow_sy;
        let token_yt_depositor = self.ta_yt[self.actor];
        let token_pt_depositor = self.ta_pt[self.actor];
        let mint_yt = self.mint_yt;
        let mint_pt = self.mint_pt;
        let token_program = SPL_TOKEN_ID;
        let vault_address_lookup_table = self.alt;
        let market_address_lookup_table = self.alt;
        let sy_program = self.sy_program_id;
        let user_yield_position = self.yield_position[self.actor];
        let escrow_yt = self.escrow_yt;
        let token_lp_escrow = self.market_escrow_lp;
        let lp_position = self.lp_position[self.actor];
        let vault_robot_yield_position = self.vault_yield_position;
        let system_program = system_program::ID;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::WrapperProvideLiquidity { amount_base, min_lp_out, mint_base_accounts_until })
            .accounts(accounts::WrapperProvideLiquidity {
                depositor: depositor,
                authority: authority,
                vault: vault,
                market: market,
                token_pt_escrow: token_pt_escrow,
                token_sy_escrow: token_sy_escrow,
                token_lp_dst: token_lp_dst,
                mint_lp: mint_lp,
                token_sy_depositor: token_sy_depositor,
                escrow_sy: escrow_sy,
                token_yt_depositor: token_yt_depositor,
                token_pt_depositor: token_pt_depositor,
                mint_yt: mint_yt,
                mint_pt: mint_pt,
                token_program: token_program,
                vault_address_lookup_table: vault_address_lookup_table,
                market_address_lookup_table: market_address_lookup_table,
                sy_program: sy_program,
                user_yield_position: user_yield_position,
                escrow_yt: escrow_yt,
                token_lp_escrow: token_lp_escrow,
                lp_position: lp_position,
                vault_robot_yield_position: vault_robot_yield_position,
                system_program: system_program,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)])
            .signers(&[&*self.payer, &__scout_signer_depositor])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:wrapper_provide_liquidity:BEGIN
            // update shadow-ledger state after successful wrapper_provide_liquidity
            // SCOUT:ACTION-HOOK:wrapper_provide_liquidity:END
        }
        __scout_success
    }

    pub fn action_wrapper_buy_pt(&mut self) -> bool {
        let pt_amount: u64 = { let m = self.ctx.token_balance(&self.market_escrow_pt); (m / 16).max(1) };
        let max_base_amount: u64 = { let b = self.ctx.token_balance(&self.ta_base[self.actor]); (b / 8).max(1) };
        let mint_sy_rem_accounts_until: u8 = MINT_SY_ACCOUNTS;
        let __scout_signer_buyer = self.users[self.actor].insecure_clone();
        let buyer = __scout_signer_buyer.pubkey();
        let market = self.market;
        let token_sy_trader = self.ta_sy[self.actor];
        let token_pt_trader = self.ta_pt[self.actor];
        let token_sy_escrow = self.market_escrow_sy;
        let token_pt_escrow = self.market_escrow_pt;
        let address_lookup_table = self.alt;
        let token_program = SPL_TOKEN_ID;
        let sy_program = self.sy_program_id;
        let token_fee_treasury_sy = self.token_treasury_fee_sy;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::WrapperBuyPt { pt_amount, max_base_amount, mint_sy_rem_accounts_until })
            .accounts(accounts::WrapperBuyPt {
                buyer: buyer,
                market: market,
                token_sy_trader: token_sy_trader,
                token_pt_trader: token_pt_trader,
                token_sy_escrow: token_sy_escrow,
                token_pt_escrow: token_pt_escrow,
                address_lookup_table: address_lookup_table,
                token_program: token_program,
                sy_program: sy_program,
                token_fee_treasury_sy: token_fee_treasury_sy,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)])
            .signers(&[&*self.payer, &__scout_signer_buyer])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:wrapper_buy_pt:BEGIN
            // update shadow-ledger state after successful wrapper_buy_pt
            // SCOUT:ACTION-HOOK:wrapper_buy_pt:END
        }
        __scout_success
    }

    pub fn action_wrapper_sell_pt(&mut self) -> bool {
        let amount_pt: u64 = { let b = self.ctx.token_balance(&self.ta_pt[self.actor]); (b / 8).max(1) };
        let min_base_amount: u64 = 0;
        let redeem_sy_rem_accounts_until: u8 = REDEEM_SY_ACCOUNTS;
        let __scout_signer_seller = self.users[self.actor].insecure_clone();
        let seller = __scout_signer_seller.pubkey();
        let market = self.market;
        let token_sy_trader = self.ta_sy[self.actor];
        let token_pt_trader = self.ta_pt[self.actor];
        let token_sy_escrow = self.market_escrow_sy;
        let token_pt_escrow = self.market_escrow_pt;
        let address_lookup_table = self.alt;
        let token_program = SPL_TOKEN_ID;
        let sy_program = self.sy_program_id;
        let token_fee_treasury_sy = self.token_treasury_fee_sy;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::WrapperSellPt { amount_pt, min_base_amount, redeem_sy_rem_accounts_until })
            .accounts(accounts::WrapperSellPt {
                seller: seller,
                market: market,
                token_sy_trader: token_sy_trader,
                token_pt_trader: token_pt_trader,
                token_sy_escrow: token_sy_escrow,
                token_pt_escrow: token_pt_escrow,
                address_lookup_table: address_lookup_table,
                token_program: token_program,
                sy_program: sy_program,
                token_fee_treasury_sy: token_fee_treasury_sy,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)])
            .signers(&[&*self.payer, &__scout_signer_seller])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:wrapper_sell_pt:BEGIN
            // update shadow-ledger state after successful wrapper_sell_pt
            // SCOUT:ACTION-HOOK:wrapper_sell_pt:END
        }
        __scout_success
    }

    pub fn action_wrapper_buy_yt(&mut self) -> bool {
        let yt_out: u64 = { let m = self.ctx.token_balance(&self.market_escrow_pt); (m / 256).max(4) };
        let max_base_amount: u64 = { let b = self.ctx.token_balance(&self.ta_base[self.actor]); (b / 8).max(1) };
        let mint_sy_accounts_length: u8 = MINT_SY_ACCOUNTS;
        let __scout_signer_buyer = self.users[self.actor].insecure_clone();
        let buyer = __scout_signer_buyer.pubkey();
        let market = self.market;
        let token_sy_trader = self.ta_sy[self.actor];
        let token_yt_trader = self.ta_yt[self.actor];
        let token_pt_trader = self.ta_pt[self.actor];
        let token_sy_escrow = self.market_escrow_sy;
        let token_pt_escrow = self.market_escrow_pt;
        let market_address_lookup_table = self.alt;
        let token_program = SPL_TOKEN_ID;
        let sy_program = self.sy_program_id;
        let vault_authority = self.vault_authority;
        let vault = self.vault;
        let token_sy_escrow_vault = self.escrow_sy;
        let mint_yt = self.mint_yt;
        let mint_pt = self.mint_pt;
        let vault_address_lookup_table = self.alt;
        let user_yield_position = self.yield_position[self.actor];
        let yield_position = self.vault_yield_position;
        let escrow_yt = self.escrow_yt;
        let system_program = system_program::ID;
        let token_fee_treasury_sy = self.token_treasury_fee_sy;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::WrapperBuyYt { yt_out, max_base_amount, mint_sy_accounts_length })
            .accounts(accounts::WrapperBuyYt {
                buyer: buyer,
                market: market,
                token_sy_trader: token_sy_trader,
                token_yt_trader: token_yt_trader,
                token_pt_trader: token_pt_trader,
                token_sy_escrow: token_sy_escrow,
                token_pt_escrow: token_pt_escrow,
                market_address_lookup_table: market_address_lookup_table,
                token_program: token_program,
                sy_program: sy_program,
                vault_authority: vault_authority,
                vault: vault,
                token_sy_escrow_vault: token_sy_escrow_vault,
                mint_yt: mint_yt,
                mint_pt: mint_pt,
                vault_address_lookup_table: vault_address_lookup_table,
                user_yield_position: user_yield_position,
                yield_position: yield_position,
                escrow_yt: escrow_yt,
                system_program: system_program,
                token_fee_treasury_sy: token_fee_treasury_sy,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)])
            .signers(&[&*self.payer, &__scout_signer_buyer])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:wrapper_buy_yt:BEGIN
            // update shadow-ledger state after successful wrapper_buy_yt
            // SCOUT:ACTION-HOOK:wrapper_buy_yt:END
        }
        __scout_success
    }

    pub fn action_wrapper_sell_yt(&mut self) -> bool {
        let yt_amount: u64 = { let b = self.ctx.token_balance(&self.ta_yt[self.actor]); (b / 8).max(1) };
        let min_base_amount: u64 = 0;
        let redeem_sy_accounts_until: u8 = REDEEM_SY_ACCOUNTS;
        let __scout_signer_seller = self.users[self.actor].insecure_clone();
        let seller = __scout_signer_seller.pubkey();
        let market = self.market;
        let token_sy_trader = self.ta_sy[self.actor];
        let token_yt_trader = self.ta_yt[self.actor];
        let token_pt_trader = self.ta_pt[self.actor];
        let token_sy_escrow = self.market_escrow_sy;
        let token_pt_escrow = self.market_escrow_pt;
        let market_address_lookup_table = self.alt;
        let token_program = SPL_TOKEN_ID;
        let sy_program = self.sy_program_id;
        let vault_authority = self.vault_authority;
        let vault = self.vault;
        let token_sy_escrow_vault = self.escrow_sy;
        let mint_yt = self.mint_yt;
        let mint_pt = self.mint_pt;
        let vault_address_lookup_table = self.alt;
        let yield_position = self.vault_yield_position;
        let token_fee_treasury_sy = self.token_treasury_fee_sy;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::WrapperSellYt { yt_amount, min_base_amount, redeem_sy_accounts_until })
            .accounts(accounts::WrapperSellYt {
                seller: seller,
                market: market,
                token_sy_trader: token_sy_trader,
                token_yt_trader: token_yt_trader,
                token_pt_trader: token_pt_trader,
                token_sy_escrow: token_sy_escrow,
                token_pt_escrow: token_pt_escrow,
                market_address_lookup_table: market_address_lookup_table,
                token_program: token_program,
                sy_program: sy_program,
                vault_authority: vault_authority,
                vault: vault,
                token_sy_escrow_vault: token_sy_escrow_vault,
                mint_yt: mint_yt,
                mint_pt: mint_pt,
                vault_address_lookup_table: vault_address_lookup_table,
                yield_position: yield_position,
                token_fee_treasury_sy: token_fee_treasury_sy,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)])
            .signers(&[&*self.payer, &__scout_signer_seller])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:wrapper_sell_yt:BEGIN
            // update shadow-ledger state after successful wrapper_sell_yt
            // SCOUT:ACTION-HOOK:wrapper_sell_yt:END
        }
        __scout_success
    }

    pub fn action_wrapper_collect_interest(&mut self) -> bool {
        let redeem_sy_accounts_length: u8 = REDEEM_SY_ACCOUNTS;
        let __scout_signer_claimer = self.users[self.actor].insecure_clone();
        let claimer = __scout_signer_claimer.pubkey();
        let authority = self.vault_authority;
        let vault = self.vault;
        let address_lookup_table = self.alt;
        let escrow_sy = self.escrow_sy;
        let sy_program = self.sy_program_id;
        let token_program = SPL_TOKEN_ID;
        let yield_position = self.yield_position[self.actor];
        let token_sy_dst = self.ta_sy[self.actor];
        let treasury_sy_token_account = self.treasury_sy_ta;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::WrapperCollectInterest { redeem_sy_accounts_length })
            .accounts(accounts::WrapperCollectInterest {
                claimer: claimer,
                authority: authority,
                vault: vault,
                address_lookup_table: address_lookup_table,
                escrow_sy: escrow_sy,
                sy_program: sy_program,
                token_program: token_program,
                yield_position: yield_position,
                token_sy_dst: token_sy_dst,
                treasury_sy_token_account: treasury_sy_token_account,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)])
            .signers(&[&*self.payer, &__scout_signer_claimer])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:wrapper_collect_interest:BEGIN
            // update shadow-ledger state after successful wrapper_collect_interest
            // SCOUT:ACTION-HOOK:wrapper_collect_interest:END
        }
        __scout_success
    }

    pub fn action_wrapper_withdraw_liquidity(&mut self) -> bool {
        let amount_lp: u64 = { let b = self.ctx.token_balance(&self.ta_lp[self.actor]); (b / 4).max(1) };
        let sy_constraint: u64 = 0;
        let redeem_sy_accounts_length: u8 = REDEEM_SY_ACCOUNTS;
        let __scout_signer_withdrawer = self.users[self.actor].insecure_clone();
        let withdrawer = __scout_signer_withdrawer.pubkey();
        let market = self.market;
        let token_pt_escrow = self.market_escrow_pt;
        let token_sy_escrow = self.market_escrow_sy;
        let token_lp_src = self.ta_lp[self.actor];
        let mint_lp = self.mint_lp;
        let token_sy_withdrawer = self.ta_sy[self.actor];
        let token_pt_withdrawer = self.ta_pt[self.actor];
        let token_program = SPL_TOKEN_ID;
        let market_address_lookup_table = self.alt;
        let sy_program = self.sy_program_id;
        let token_lp_escrow = self.market_escrow_lp;
        let token_fee_treasury_sy = self.token_treasury_fee_sy;
        let lp_position = self.lp_position[self.actor];
        let system_program = system_program::ID;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::WrapperWithdrawLiquidity { amount_lp, sy_constraint, redeem_sy_accounts_length })
            .accounts(accounts::WrapperWithdrawLiquidity {
                withdrawer: withdrawer,
                market: market,
                token_pt_escrow: token_pt_escrow,
                token_sy_escrow: token_sy_escrow,
                token_lp_src: token_lp_src,
                mint_lp: mint_lp,
                token_sy_withdrawer: token_sy_withdrawer,
                token_pt_withdrawer: token_pt_withdrawer,
                token_program: token_program,
                market_address_lookup_table: market_address_lookup_table,
                sy_program: sy_program,
                token_lp_escrow: token_lp_escrow,
                token_fee_treasury_sy: token_fee_treasury_sy,
                lp_position: lp_position,
                system_program: system_program,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)])
            .signers(&[&*self.payer, &__scout_signer_withdrawer])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:wrapper_withdraw_liquidity:BEGIN
            // update shadow-ledger state after successful wrapper_withdraw_liquidity
            // SCOUT:ACTION-HOOK:wrapper_withdraw_liquidity:END
        }
        __scout_success
    }

    pub fn action_wrapper_withdraw_liquidity_classic(&mut self) -> bool {
        let amount_lp: u64 = { let b = self.ctx.token_balance(&self.ta_lp[self.actor]); (b / 4).max(1) };
        let redeem_sy_accounts_length: u8 = REDEEM_SY_ACCOUNTS;
        let __scout_signer_withdrawer = self.users[self.actor].insecure_clone();
        let withdrawer = __scout_signer_withdrawer.pubkey();
        let market = self.market;
        let token_pt_escrow = self.market_escrow_pt;
        let token_sy_escrow = self.market_escrow_sy;
        let token_lp_src = self.ta_lp[self.actor];
        let mint_lp = self.mint_lp;
        let token_sy_withdrawer = self.ta_sy[self.actor];
        let token_pt_withdrawer = self.ta_pt[self.actor];
        let token_program = SPL_TOKEN_ID;
        let market_address_lookup_table = self.alt;
        let sy_program = self.sy_program_id;
        let token_lp_escrow = self.market_escrow_lp;
        let lp_position = self.lp_position[self.actor];
        let system_program = system_program::ID;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::WrapperWithdrawLiquidityClassic { amount_lp, redeem_sy_accounts_length })
            .accounts(accounts::WrapperWithdrawLiquidityClassic {
                withdrawer: withdrawer,
                market: market,
                token_pt_escrow: token_pt_escrow,
                token_sy_escrow: token_sy_escrow,
                token_lp_src: token_lp_src,
                mint_lp: mint_lp,
                token_sy_withdrawer: token_sy_withdrawer,
                token_pt_withdrawer: token_pt_withdrawer,
                token_program: token_program,
                market_address_lookup_table: market_address_lookup_table,
                sy_program: sy_program,
                token_lp_escrow: token_lp_escrow,
                lp_position: lp_position,
                system_program: system_program,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)])
            .signers(&[&*self.payer, &__scout_signer_withdrawer])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:wrapper_withdraw_liquidity_classic:BEGIN
            // update shadow-ledger state after successful wrapper_withdraw_liquidity_classic
            // SCOUT:ACTION-HOOK:wrapper_withdraw_liquidity_classic:END
        }
        __scout_success
    }

    pub fn action_wrapper_provide_liquidity_base(&mut self) -> bool {
        let amount_base: u64 = { let m = self.ctx.token_balance(&self.market_escrow_pt); (m / 64).max(1) };
        let min_lp_out: u64 = 0;
        let mint_sy_accounts_until: u8 = MINT_SY_ACCOUNTS;
        let external_pt_to_buy: u64 = { let m = self.ctx.token_balance(&self.market_escrow_pt); (m / 64).max(1) };
        let external_sy_constraint: u64 = { self.ctx.token_balance(&self.ta_sy[self.actor]).max(1) };
        let __scout_signer_depositor = self.users[self.actor].insecure_clone();
        let depositor = __scout_signer_depositor.pubkey();
        let market = self.market;
        let token_pt_escrow = self.market_escrow_pt;
        let token_sy_escrow = self.market_escrow_sy;
        let token_lp_dst = self.ta_lp[self.actor];
        let mint_lp = self.mint_lp;
        let token_sy_depositor = self.ta_sy[self.actor];
        let token_pt_depositor = self.ta_pt[self.actor];
        let token_program = SPL_TOKEN_ID;
        let market_address_lookup_table = self.alt;
        let sy_program = self.sy_program_id;
        let token_fee_treasury_sy = self.token_treasury_fee_sy;
        let token_lp_escrow = self.market_escrow_lp;
        let lp_position = self.lp_position[self.actor];
        let system_program = system_program::ID;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::WrapperProvideLiquidityBase { amount_base, min_lp_out, mint_sy_accounts_until, external_pt_to_buy, external_sy_constraint })
            .accounts(accounts::WrapperProvideLiquidityBase {
                depositor: depositor,
                market: market,
                token_pt_escrow: token_pt_escrow,
                token_sy_escrow: token_sy_escrow,
                token_lp_dst: token_lp_dst,
                mint_lp: mint_lp,
                token_sy_depositor: token_sy_depositor,
                token_pt_depositor: token_pt_depositor,
                token_program: token_program,
                market_address_lookup_table: market_address_lookup_table,
                sy_program: sy_program,
                token_fee_treasury_sy: token_fee_treasury_sy,
                token_lp_escrow: token_lp_escrow,
                lp_position: lp_position,
                system_program: system_program,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)])
            .signers(&[&*self.payer, &__scout_signer_depositor])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:wrapper_provide_liquidity_base:BEGIN
            // update shadow-ledger state after successful wrapper_provide_liquidity_base
            // SCOUT:ACTION-HOOK:wrapper_provide_liquidity_base:END
        }
        __scout_success
    }

    pub fn action_wrapper_provide_liquidity_classic(&mut self) -> bool {
        let amount_base: u64 = { let b = self.ctx.token_balance(&self.ta_base[self.actor]); (b / 16).max(1) };
        let amount_pt: u64 = { let b = self.ctx.token_balance(&self.ta_pt[self.actor]); (b / 8).max(1) };
        let min_lp_out: u64 = 0;
        let mint_sy_accounts_until: u8 = MINT_SY_ACCOUNTS;
        let __scout_signer_depositor = self.users[self.actor].insecure_clone();
        let depositor = __scout_signer_depositor.pubkey();
        let market = self.market;
        let token_pt_escrow = self.market_escrow_pt;
        let token_sy_escrow = self.market_escrow_sy;
        let token_lp_dst = self.ta_lp[self.actor];
        let mint_lp = self.mint_lp;
        let token_sy_depositor = self.ta_sy[self.actor];
        let token_pt_depositor = self.ta_pt[self.actor];
        let token_program = SPL_TOKEN_ID;
        let market_address_lookup_table = self.alt;
        let sy_program = self.sy_program_id;
        let token_lp_escrow = self.market_escrow_lp;
        let lp_position = self.lp_position[self.actor];
        let system_program = system_program::ID;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::WrapperProvideLiquidityClassic { amount_base, amount_pt, min_lp_out, mint_sy_accounts_until })
            .accounts(accounts::WrapperProvideLiquidityClassic {
                depositor: depositor,
                market: market,
                token_pt_escrow: token_pt_escrow,
                token_sy_escrow: token_sy_escrow,
                token_lp_dst: token_lp_dst,
                mint_lp: mint_lp,
                token_sy_depositor: token_sy_depositor,
                token_pt_depositor: token_pt_depositor,
                token_program: token_program,
                market_address_lookup_table: market_address_lookup_table,
                sy_program: sy_program,
                token_lp_escrow: token_lp_escrow,
                lp_position: lp_position,
                system_program: system_program,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)])
            .signers(&[&*self.payer, &__scout_signer_depositor])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:wrapper_provide_liquidity_classic:BEGIN
            // update shadow-ledger state after successful wrapper_provide_liquidity_classic
            // SCOUT:ACTION-HOOK:wrapper_provide_liquidity_classic:END
        }
        __scout_success
    }

    pub fn action_wrapper_strip(&mut self) -> bool {
        let amount_base: u64 = { let b = self.ctx.token_balance(&self.ta_base[self.actor]); (b / 16).max(1) };
        let mint_sy_accounts_until: u8 = MINT_SY_ACCOUNTS;
        let __scout_signer_depositor = self.users[self.actor].insecure_clone();
        let depositor = __scout_signer_depositor.pubkey();
        let token_sy_depositor = self.ta_sy[self.actor];
        let vault = self.vault;
        let escrow_sy = self.escrow_sy;
        let token_yt_depositor = self.ta_yt[self.actor];
        let token_pt_depositor = self.ta_pt[self.actor];
        let mint_yt = self.mint_yt;
        let mint_pt = self.mint_pt;
        let authority = self.vault_authority;
        let vault_address_lookup_table = self.alt;
        let token_program = SPL_TOKEN_ID;
        let escrow_yt = self.escrow_yt;
        let user_yield_position = self.yield_position[self.actor];
        let vault_robot_yield_position = self.vault_yield_position;
        let sy_program = self.sy_program_id;
        let system_program = system_program::ID;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::WrapperStrip { amount_base, mint_sy_accounts_until })
            .accounts(accounts::WrapperStrip {
                depositor: depositor,
                token_sy_depositor: token_sy_depositor,
                vault: vault,
                escrow_sy: escrow_sy,
                token_yt_depositor: token_yt_depositor,
                token_pt_depositor: token_pt_depositor,
                mint_yt: mint_yt,
                mint_pt: mint_pt,
                authority: authority,
                vault_address_lookup_table: vault_address_lookup_table,
                token_program: token_program,
                escrow_yt: escrow_yt,
                user_yield_position: user_yield_position,
                vault_robot_yield_position: vault_robot_yield_position,
                sy_program: sy_program,
                system_program: system_program,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)])
            .signers(&[&*self.payer, &__scout_signer_depositor])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:wrapper_strip:BEGIN
            // update shadow-ledger state after successful wrapper_strip
            // SCOUT:ACTION-HOOK:wrapper_strip:END
        }
        __scout_success
    }

    pub fn action_wrapper_merge(&mut self) -> bool {
        let amount_py: u64 = { let p = self.ctx.token_balance(&self.ta_pt[self.actor]); let y = self.ctx.token_balance(&self.ta_yt[self.actor]); (p.min(y) / 2).max(1) };
        let redeem_sy_accounts_until: u8 = REDEEM_SY_ACCOUNTS;
        let __scout_signer_merger = self.users[self.actor].insecure_clone();
        let merger = __scout_signer_merger.pubkey();
        let token_sy_merger = self.ta_sy[self.actor];
        let vault = self.vault;
        let escrow_sy = self.escrow_sy;
        let token_yt_merger = self.ta_yt[self.actor];
        let token_pt_merger = self.ta_pt[self.actor];
        let mint_yt = self.mint_yt;
        let mint_pt = self.mint_pt;
        let authority = self.vault_authority;
        let vault_address_lookup_table = self.alt;
        let token_program = SPL_TOKEN_ID;
        let vault_robot_yield_position = self.vault_yield_position;
        let sy_program = self.sy_program_id;
        let system_program = system_program::ID;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::WrapperMerge { amount_py, redeem_sy_accounts_until })
            .accounts(accounts::WrapperMerge {
                merger: merger,
                token_sy_merger: token_sy_merger,
                vault: vault,
                escrow_sy: escrow_sy,
                token_yt_merger: token_yt_merger,
                token_pt_merger: token_pt_merger,
                mint_yt: mint_yt,
                mint_pt: mint_pt,
                authority: authority,
                vault_address_lookup_table: vault_address_lookup_table,
                token_program: token_program,
                vault_robot_yield_position: vault_robot_yield_position,
                sy_program: sy_program,
                system_program: system_program,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)])
            .signers(&[&*self.payer, &__scout_signer_merger])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:wrapper_merge:BEGIN
            // update shadow-ledger state after successful wrapper_merge
            // SCOUT:ACTION-HOOK:wrapper_merge:END
        }
        __scout_success
    }

    pub fn action_realloc_market(&mut self, additional_bytes: u64) -> bool {
        let market = self.market;
        let __scout_signer_signer = self.payer.insecure_clone();
        let signer = __scout_signer_signer.pubkey();
        let admin_state = self.admin_account;
        let system_program = system_program::ID;
        let rent = RENT_SYSVAR_ID;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::ReallocMarket { additional_bytes })
            .accounts(accounts::ReallocMarket {
                market: market,
                signer: signer,
                admin_state: admin_state,
                system_program: system_program,
                rent: rent,
            })
            .signers(&[&*self.payer, &__scout_signer_signer])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:realloc_market:BEGIN
            // update shadow-ledger state after successful realloc_market
            // SCOUT:ACTION-HOOK:realloc_market:END
        }
        __scout_success
    }

    pub fn action_add_lp_tokens_metadata(&mut self) -> bool {
        // SCOUT-TODO: arg name: String; arg symbol: String; arg uri: String
        let name: String = String::new(); // SCOUT-TODO: value for arg name: String
        let symbol: String = String::new(); // SCOUT-TODO: value for arg symbol: String
        let uri: String = String::new(); // SCOUT-TODO: value for arg uri: String
        let payer = self.payer.pubkey();
        let admin = self.admin_account;
        let market = self.market;
        let mint_lp = self.mint_lp;
        let metadata = self.lp_metadata;
        let token_metadata_program = MPL_TOKEN_METADATA_ID;
        let system_program = system_program::ID;
        let __scout_success = self.ctx
            .program(self.program_id)
            .call(instruction::AddLpTokensMetadata { name, symbol, uri })
            .accounts(accounts::AddLpTokensMetadata {
                payer: payer,
                admin: admin,
                market: market,
                mint_lp: mint_lp,
                metadata: metadata,
                token_metadata_program: token_metadata_program,
                system_program: system_program,
            })
            .signers(&[&*self.payer])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false);
        if __scout_success {
            // SCOUT:ACTION-HOOK:add_lp_tokens_metadata:BEGIN
            // update shadow-ledger state after successful add_lp_tokens_metadata
            // SCOUT:ACTION-HOOK:add_lp_tokens_metadata:END
        }
        __scout_success
    }

    // SCOUT:EXTRA-ACTIONS:BEGIN
    /// Mint SY to `user` by spending `base_amount` of their base tokens through the mock SY
    /// program. Done via a real CPI rather than a direct balance poke so SY mint supply and
    /// base custody stay consistent -- conservation properties depend on that.
    fn mock_sy_mint(&mut self, user_index: usize, base_amount: u64) -> bool {
        let user = self.users[user_index].clone();
        let mut data = vec![1u8]; // ix::MINT_SY
        data.extend_from_slice(&base_amount.to_le_bytes());
        let ix = Instruction {
            program_id: self.sy_program_id,
            accounts: vec![
                AccountMeta::new(self.sy_global, false),
                AccountMeta::new(self.ta_base[user_index], false),
                AccountMeta::new(self.base_custody, false),
                AccountMeta::new(self.sy_mint, false),
                AccountMeta::new(self.ta_sy[user_index], false),
                AccountMeta::new_readonly(user.pubkey(), true),
                AccountMeta::new_readonly(SPL_TOKEN_ID, false),
            ],
            data,
        };
        self.ctx.raw_call(ix).signers(&[&*user]).send()
            .map(|o| o.is_success()).unwrap_or(false)
    }


    /// Accounts the SY-program CPI needs that no Exponent instruction carries in its own list.
    /// Order is irrelevant here (the callee's order comes from `Vault.cpi_accounts`); what matters
    /// is that each is PRESENT and writable, because `do_deposit_sy`/`do_withdraw_sy` filter the
    /// combined pool by key and silently drop anything missing.
    fn sy_cpi_metas(&self) -> Vec<AccountMeta> {
        vec![
            AccountMeta::new(self.sy_global, false),
            AccountMeta::new(self.vault_sy_position, false),
            AccountMeta::new(self.sy_custody, false),
        ]
    }

    fn run_initialize_yield_position(
        ctx: &mut crucible_test_context::TestContext,
        program_id: Pubkey, owner: &Rc<Keypair>, vault: Pubkey, yield_position: Pubkey,
    ) {
        let outcome = ctx
            .program(program_id)
            .call(instruction::InitializeYieldPosition {})
            .accounts(accounts::InitializeYieldPosition {
                owner: owner.pubkey(),
                vault,
                yield_position,
                system_program: system_program::ID,
                event_authority: Pubkey::find_program_address(
                    &[b"__event_authority"], &program_id).0,
                program: program_id,
            })
            .signers(&[&**owner])
            .send()
            .expect("initialize_yield_position send failed");
        assert!(outcome.is_success(),
                "initialize_yield_position failed: {:#?}", outcome.logs());
    }


    /// A trade with realistic magnitude and the correct constraint sign, so the market actually
    /// moves. The generated `action_trade_pt` is left fully fuzzer-driven to explore the argument
    /// space (including the values that panic); this one exists so the AMM's real paths get
    /// exercised rather than the harness only ever finding argument-validation failures.
    pub fn action_trade_pt_clamped(
        &mut self, #[range(1..400_000)] magnitude: u32, buy_pt: bool,
    ) -> bool {
        let net_trader_pt = if buy_pt { magnitude as i64 } else { -(magnitude as i64) };
        let trader = self.users[self.actor].insecure_clone();
        let ea = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id).0;
        let metas = vec![
            AccountMeta::new(self.sy_global, false),
            AccountMeta::new(self.market_sy_position, false),
            AccountMeta::new(self.sy_custody, false),
            AccountMeta::new_readonly(self.sy_authority, false),
        ];
        let (market, alt, sy_program) = (self.market, self.alt, self.sy_program_id);
        let (ta_sy, ta_pt) = (self.ta_sy[self.actor], self.ta_pt[self.actor]);
        let (esc_sy, esc_pt) = (self.market_escrow_sy, self.market_escrow_pt);
        let fee_ta = self.token_treasury_fee_sy;
        let program_id = self.program_id;
        self.ctx
            .program(program_id)
            .call(instruction::TradePt { net_trader_pt, sy_constraint: i64::MIN })
            .accounts(accounts::TradePt {
                trader: trader.pubkey(), market,
                token_sy_trader: ta_sy, token_pt_trader: ta_pt,
                token_sy_escrow: esc_sy, token_pt_escrow: esc_pt,
                address_lookup_table: alt, token_program: SPL_TOKEN_ID,
                sy_program, token_fee_treasury_sy: fee_ta,
                event_authority: ea, program: program_id,
            })
            .remaining_accounts_metas(metas)
            .signers(&[&trader])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }


    /// Raw, UNCLAMPED amounts at the value-flow instructions. The generated actions now derive
    /// their sizes from live state so they reliably reach the handler; this one deliberately keeps
    /// the fuzzer's full range so argument extremes (0, 1, u64::MAX, and everything that overflows)
    /// are still explored. `overflow-checks = true` is on for this program, so an arithmetic edge
    /// here is a real abort, not a wrap.
    pub fn action_value_flow_edge(&mut self, which: u8, amount: u64) -> bool {
        match which % 4 {
            0 => self.action_strip(),
            1 => self.action_merge(),
            2 => self.action_deposit_yt(),
            _ => {
                // trade_pt with a raw signed amount, including 0 -- which the opposite-signs
                // assert at trade_pt.rs:192 does not admit.
                let net = amount as i64;
                let trader = self.users[self.actor].insecure_clone();
                let ea = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id).0;
                let metas = vec![
                    AccountMeta::new(self.sy_global, false),
                    AccountMeta::new(self.market_sy_position, false),
                    AccountMeta::new(self.sy_custody, false),
                    AccountMeta::new_readonly(self.sy_authority, false),
                ];
                let (market, alt, syp) = (self.market, self.alt, self.sy_program_id);
                let (ta_sy, ta_pt) = (self.ta_sy[self.actor], self.ta_pt[self.actor]);
                let (esy, ept) = (self.market_escrow_sy, self.market_escrow_pt);
                let fee = self.token_treasury_fee_sy;
                let pid = self.program_id;
                self.ctx.program(pid)
                    .call(instruction::TradePt { net_trader_pt: net, sy_constraint: i64::MIN })
                    .accounts(accounts::TradePt {
                        trader: trader.pubkey(), market,
                        token_sy_trader: ta_sy, token_pt_trader: ta_pt,
                        token_sy_escrow: esy, token_pt_escrow: ept,
                        address_lookup_table: alt, token_program: SPL_TOKEN_ID,
                        sy_program: syp, token_fee_treasury_sy: fee,
                        event_authority: ea, program: pid,
                    })
                    .remaining_accounts_metas(metas)
                    .signers(&[&trader])
                    .send().map(|o| o.is_success()).unwrap_or(false)
            }
        }
    }


    /// Turn on the emission stream: register it on the SY program AND register it on the vault, in
    /// ONE action. Both halves must happen without any other Exponent instruction in between,
    /// because the vault indexes its own emission list by the SY program's list length
    /// (`state/vault.rs:356-357`) -- that is confirmed bug issue-02, and it constrains the harness
    /// exactly as it would constrain an operator.
    ///
    /// Registers the NEXT stream, so repeated calls walk 0, 1, ... up to `N_EMISSION_STREAMS`.
    /// This used to hard-refuse unless the vault had zero emissions, which meant the vault could
    /// never hold more than one -- see BLIND-SPOTS.md #1 for what that hid.
    ///
    /// The guard that remains is still load-bearing: adding a stream the fixture has no ALT slots,
    /// custody or escrow for would be registered on the vault and then be uncollectable, and
    /// calling this when the two lists are already out of step panics at `vault.rs:377`
    /// (`sy_state.emission_indexes[self.emissions.len()]` out of bounds) -- the mirror of issue-02,
    /// recorded as OOS-03 -- so every iteration after the first would log a spurious crash.
    pub fn action_enable_emission(&mut self) -> bool {
        let n = self.ctx
            .read_anchor_account::<exponent_core::state::Vault>(&self.vault)
            .map(|v| v.emissions.len())
            .unwrap_or(usize::MAX);
        if n >= N_EMISSION_STREAMS {
            return false;
        }
        let mint = self.emission_mints[n];
        if !self.mock_sy_add_emission_index(0, mint) {
            return false;
        }
        if !self.run_add_emission_stream(n, 0).is_success() {
            return false;
        }
        // Keep the MARKET's tracker list the same length as the SY program's global list.
        //
        // Not cosmetic: `MarketTwo::update_emissions_from_position_state` (`market_two.rs:322-336`)
        // walks the SY position's emission list and indexes the market's trackers with no bounds
        // check, so any gap panics `deposit_lp` and `withdraw_lp`. That is confirmed bug issue-08;
        // it already has a PoC, and leaving the fuzzer free to re-enter it would flood every
        // campaign with one known panic instead of exploring past it.
        self.action_add_market_emission()
    }

    /// Move the emission stream's cumulative index, then touch the vault's SY position so the move
    /// becomes claimable. The mock accrues non-retroactively, so without the touch an index move
    /// credits nothing.
    pub fn action_accrue_emission(&mut self, #[range(1..1_000)] milli: u32) -> bool {
        let cur = self.ctx
            .read_anchor_account::<exponent_core::state::Vault>(&self.vault)
            .map(|v| v.emissions.len())
            .unwrap_or(0);
        if cur == 0 {
            return false;
        }
        self.mock_sy_fund_vault_emission(0, 0);
        let next = (milli as u128) * NUMBER_ONE / 1_000;
        if !self.mock_sy_set_emission_index(0, next) {
            return false;
        }
        self.mock_sy_fund_vault_emission(0, 0)
    }

    // ================= harness-side actions ====================================================

    /// Choose which actor speaks for the next user-facing instruction. Not an IDL instruction --
    /// it is how the fuzzer gets multi-actor sequences out of single-actor action signatures.
    pub fn action_select_actor(&mut self, index: u8) {
        self.actor = (index as usize) % N_USERS;
    }

    /// Move the external SY exchange rate. This is the single most important harness capability on
    /// this target: Exponent's entire yield accounting is driven by the rate the SY program
    /// reports, and pinning it to one value deletes every bug that only appears when the rate MOVES
    /// between two user actions. Decreases are allowed on purpose -- the protocol has an explicit
    /// emergency mode keyed on `all_time_high > last_seen`, so a falling rate is in scope.
    pub fn action_set_sy_exchange_rate(&mut self, #[range(1..4_000)] rate_milli: u32) -> bool {
        let rate = number_bytes(rate_milli as u128 * NUMBER_ONE / 1_000);
        let mut data = vec![MOCK_SY_SET_EXCHANGE_RATE];
        data.extend_from_slice(&rate);
        let ix = Instruction {
            program_id: self.sy_program_id,
            accounts: vec![AccountMeta::new(self.sy_global, false)],
            data,
        };
        let payer = self.payer.clone();
        let ok = self.ctx.raw_call(ix).signers(&[&*payer]).send()
            .map(|o| o.is_success()).unwrap_or(false);
        // P-0004 gate. A rate that moves off baseline and comes back is invisible to the observable
        // gate but is NOT value-neutral: the position's `sy_exchange_rate_last_seen` follows the
        // rate down, so the climb back credits `calc_earned_sy` a second time. Latch it.
        if ok && rate_milli as u128 * NUMBER_ONE / 1_000 != self.baseline_sy_rate {
            self.sy_rate_moved = true;
        }
        ok
    }





























































    /// `buy_yt` with EXPLICIT amounts, so the working range can be swept rather than guessed.
    #[allow(dead_code)]
    pub fn run_buy_yt(&mut self, sy_in: u64, yt_out: u64) -> crucible_test_context::TxOutcome {
        let __scout_signer_trader = self.users[self.actor].insecure_clone();
        let trader = __scout_signer_trader.pubkey();
        let market = self.market;
        let token_sy_trader = self.ta_sy[self.actor];
        let token_yt_trader = self.ta_yt[self.actor];
        let token_pt_trader = self.ta_pt[self.actor];
        let token_sy_escrow = self.market_escrow_sy;
        let token_pt_escrow = self.market_escrow_pt;
        let token_fee_treasury_sy = self.token_treasury_fee_sy;
        let token_program = SPL_TOKEN_ID;
        let address_lookup_table = self.alt;
        let sy_program = self.sy_program_id;
        let vault_authority = self.vault_authority;
        let vault = self.vault;
        let token_sy_escrow_vault = self.escrow_sy;
        let mint_yt = self.mint_yt;
        let mint_pt = self.mint_pt;
        let address_lookup_table_vault = self.alt;
        let yield_position = self.vault_yield_position;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        self.queue_heap_frame();
        let _ = self.ctx
            .program(self.program_id)
            .call(instruction::BuyYt { sy_in, yt_out })
            .accounts(accounts::BuyYt {
                trader: trader,
                market: market,
                token_sy_trader: token_sy_trader,
                token_yt_trader: token_yt_trader,
                token_pt_trader: token_pt_trader,
                token_sy_escrow: token_sy_escrow,
                token_pt_escrow: token_pt_escrow,
                token_fee_treasury_sy: token_fee_treasury_sy,
                token_program: token_program,
                address_lookup_table: address_lookup_table,
                sy_program: sy_program,
                vault_authority: vault_authority,
                vault: vault,
                token_sy_escrow_vault: token_sy_escrow_vault,
                mint_yt: mint_yt,
                mint_pt: mint_pt,
                address_lookup_table_vault: address_lookup_table_vault,
                yield_position: yield_position,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.market_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)])
            .signers(&[&*self.payer, &__scout_signer_trader])
            .add_transaction();
        let __outcome = self.ctx.send_batch().expect("send_batch failed").expect("no tx");
        __outcome
    }

    /// Queue a `ComputeBudget::RequestHeapFrame` so the NEXT `.send()` carries it in the same
    /// transaction.
    ///
    /// Exponent ships a custom bump allocator with `HEAP_LENGTH = 8 * 32 KiB`
    /// (`programs/exponent_core/src/allocator.rs:101`) and its own header says so in as many words:
    /// *"Access violation occurs without requestHeapFrame, requiring it for every transaction."*
    /// Without the prelude anything allocating past the default 32 KiB dies at
    /// `HEAP_START + 0x8000` -- which is exactly the address `buy_yt` and `wrapper_buy_yt` fault at,
    /// for EVERY input size from `market_pt / 4` down to 152 units.
    ///
    /// Discriminator 1 is `RequestHeapFrame(u32)`; 256 KiB matches the program's own HEAP_LENGTH.
    #[allow(dead_code)]
    pub fn queue_heap_frame(&mut self) {
        let mut data = vec![1u8];
        data.extend_from_slice(&(8u32 * 32 * 1024).to_le_bytes());
        let ix = Instruction {
            program_id: Pubkey::new_from_array([
                3, 6, 70, 111, 229, 33, 23, 50, 255, 236, 173, 186, 114, 195, 155, 231,
                188, 140, 229, 187, 197, 247, 18, 107, 44, 67, 155, 58, 64, 0, 0, 0,
            ]), // ComputeBudget111111111111111111111111111111
            accounts: vec![],
            data,
        };
        let payer = self.payer.clone();
        let _ = self.ctx.raw_call(ix).signers(&[&*payer]).add_transaction();
    }











    /// `wrapper_provide_liquidity_base` with an EXPLICIT base amount, for sweeping.
    #[allow(dead_code)]
    pub fn run_wpl_base(&mut self, amount_base: u64) -> crucible_test_context::TxOutcome {
        let min_lp_out: u64 = 0;
        let mint_sy_accounts_until: u8 = MINT_SY_ACCOUNTS;
        let external_pt_to_buy: u64 = 0;
        let external_sy_constraint: u64 = 0;
        let __scout_signer_depositor = self.users[self.actor].insecure_clone();
        let depositor = __scout_signer_depositor.pubkey();
        let market = self.market;
        let token_pt_escrow = self.market_escrow_pt;
        let token_sy_escrow = self.market_escrow_sy;
        let token_lp_dst = self.ta_lp[self.actor];
        let mint_lp = self.mint_lp;
        let token_sy_depositor = self.ta_sy[self.actor];
        let token_pt_depositor = self.ta_pt[self.actor];
        let token_program = SPL_TOKEN_ID;
        let market_address_lookup_table = self.alt;
        let sy_program = self.sy_program_id;
        let token_fee_treasury_sy = self.token_treasury_fee_sy;
        let token_lp_escrow = self.market_escrow_lp;
        let lp_position = self.lp_position[self.actor];
        let system_program = system_program::ID;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __outcome = self.ctx
            .program(self.program_id)
            .call(instruction::WrapperProvideLiquidityBase { amount_base, min_lp_out, mint_sy_accounts_until, external_pt_to_buy, external_sy_constraint })
            .accounts(accounts::WrapperProvideLiquidityBase {
                depositor: depositor,
                market: market,
                token_pt_escrow: token_pt_escrow,
                token_sy_escrow: token_sy_escrow,
                token_lp_dst: token_lp_dst,
                mint_lp: mint_lp,
                token_sy_depositor: token_sy_depositor,
                token_pt_depositor: token_pt_depositor,
                token_program: token_program,
                market_address_lookup_table: market_address_lookup_table,
                sy_program: sy_program,
                token_fee_treasury_sy: token_fee_treasury_sy,
                token_lp_escrow: token_lp_escrow,
                lp_position: lp_position,
                system_program: system_program,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)])
            .signers(&[&*self.payer, &__scout_signer_depositor])
            .send()
            .expect("send failed");
        __outcome
    }





















    /// Log-returning twin of `action_market_deposit_lp`, for issue-08's PoC.
    #[allow(dead_code)]
    pub fn diag_market_withdraw_lp_probe(&mut self) -> crucible_test_context::TxOutcome {
        let amount: u64 = { let b = self.ctx.token_balance(&self.ta_lp[self.actor]); (b / 2).max(1) };
        let __scout_signer_owner = self.users[self.actor].insecure_clone();
        let owner = __scout_signer_owner.pubkey();
        let market = self.market;
        let lp_position = self.lp_position[self.actor];
        let token_lp_src = self.ta_lp[self.actor];
        let token_lp_escrow = self.market_escrow_lp;
        let mint_lp = self.mint_lp;
        let sy_program = self.sy_program_id;
        let address_lookup_table = self.alt;
        let token_program = SPL_TOKEN_ID;
        let system_program = system_program::ID;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __outcome = self.ctx
            .program(self.program_id)
            .call(instruction::MarketDepositLp { amount })
            .accounts(accounts::MarketDepositLp {
                owner: owner,
                market: market,
                lp_position: lp_position,
                token_lp_src: token_lp_src,
                token_lp_escrow: token_lp_escrow,
                mint_lp: mint_lp,
                sy_program: sy_program,
                address_lookup_table: address_lookup_table,
                token_program: token_program,
                system_program: system_program,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.market_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)])
            .signers(&[&*self.payer, &__scout_signer_owner])
            .send()
            .expect("send failed");
        __outcome
    }











    /// DIAGNOSTIC twin of `action_buy_yt`, regenerated from the CURRENT generated body.
    #[allow(dead_code)]
    pub fn diag_buy_yt(&mut self) -> crucible_test_context::TxOutcome {
        let sy_in: u64 = { let m = self.ctx.token_balance(&self.market_escrow_pt); (m / 64).max(1) };
        let yt_out: u64 = { let m = self.ctx.token_balance(&self.market_escrow_pt); (m / 16).max(4) };
        let __scout_signer_trader = self.users[self.actor].insecure_clone();
        let trader = __scout_signer_trader.pubkey();
        let market = self.market;
        let token_sy_trader = self.ta_sy[self.actor];
        let token_yt_trader = self.ta_yt[self.actor];
        let token_pt_trader = self.ta_pt[self.actor];
        let token_sy_escrow = self.market_escrow_sy;
        let token_pt_escrow = self.market_escrow_pt;
        let token_fee_treasury_sy = self.token_treasury_fee_sy;
        let token_program = SPL_TOKEN_ID;
        let address_lookup_table = self.alt;
        let sy_program = self.sy_program_id;
        let vault_authority = self.vault_authority;
        let vault = self.vault;
        let token_sy_escrow_vault = self.escrow_sy;
        let mint_yt = self.mint_yt;
        let mint_pt = self.mint_pt;
        let address_lookup_table_vault = self.alt;
        let yield_position = self.vault_yield_position;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __outcome = self.ctx
            .program(self.program_id)
            .call(instruction::BuyYt { sy_in, yt_out })
            .accounts(accounts::BuyYt {
                trader: trader,
                market: market,
                token_sy_trader: token_sy_trader,
                token_yt_trader: token_yt_trader,
                token_pt_trader: token_pt_trader,
                token_sy_escrow: token_sy_escrow,
                token_pt_escrow: token_pt_escrow,
                token_fee_treasury_sy: token_fee_treasury_sy,
                token_program: token_program,
                address_lookup_table: address_lookup_table,
                sy_program: sy_program,
                vault_authority: vault_authority,
                vault: vault,
                token_sy_escrow_vault: token_sy_escrow_vault,
                mint_yt: mint_yt,
                mint_pt: mint_pt,
                address_lookup_table_vault: address_lookup_table_vault,
                yield_position: yield_position,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.market_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false)])
            .signers(&[&*self.payer, &__scout_signer_trader])
            .send()
            .expect("send failed");
        __outcome
    }

    /// DIAGNOSTIC twin of `action_wrapper_buy_yt`, regenerated from the CURRENT generated body.
    #[allow(dead_code)]
    pub fn diag_wrapper_buy_yt(&mut self) -> crucible_test_context::TxOutcome {
        let yt_out: u64 = { let m = self.ctx.token_balance(&self.market_escrow_pt); (m / 256).max(4) };
        let max_base_amount: u64 = { let b = self.ctx.token_balance(&self.ta_base[self.actor]); (b / 8).max(1) };
        let mint_sy_accounts_length: u8 = MINT_SY_ACCOUNTS;
        let __scout_signer_buyer = self.users[self.actor].insecure_clone();
        let buyer = __scout_signer_buyer.pubkey();
        let market = self.market;
        let token_sy_trader = self.ta_sy[self.actor];
        let token_yt_trader = self.ta_yt[self.actor];
        let token_pt_trader = self.ta_pt[self.actor];
        let token_sy_escrow = self.market_escrow_sy;
        let token_pt_escrow = self.market_escrow_pt;
        let market_address_lookup_table = self.alt;
        let token_program = SPL_TOKEN_ID;
        let sy_program = self.sy_program_id;
        let vault_authority = self.vault_authority;
        let vault = self.vault;
        let token_sy_escrow_vault = self.escrow_sy;
        let mint_yt = self.mint_yt;
        let mint_pt = self.mint_pt;
        let vault_address_lookup_table = self.alt;
        let user_yield_position = self.yield_position[self.actor];
        let yield_position = self.vault_yield_position;
        let escrow_yt = self.escrow_yt;
        let system_program = system_program::ID;
        let token_fee_treasury_sy = self.token_treasury_fee_sy;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __outcome = self.ctx
            .program(self.program_id)
            .call(instruction::WrapperBuyYt { yt_out, max_base_amount, mint_sy_accounts_length })
            .accounts(accounts::WrapperBuyYt {
                buyer: buyer,
                market: market,
                token_sy_trader: token_sy_trader,
                token_yt_trader: token_yt_trader,
                token_pt_trader: token_pt_trader,
                token_sy_escrow: token_sy_escrow,
                token_pt_escrow: token_pt_escrow,
                market_address_lookup_table: market_address_lookup_table,
                token_program: token_program,
                sy_program: sy_program,
                vault_authority: vault_authority,
                vault: vault,
                token_sy_escrow_vault: token_sy_escrow_vault,
                mint_yt: mint_yt,
                mint_pt: mint_pt,
                vault_address_lookup_table: vault_address_lookup_table,
                user_yield_position: user_yield_position,
                yield_position: yield_position,
                escrow_yt: escrow_yt,
                system_program: system_program,
                token_fee_treasury_sy: token_fee_treasury_sy,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)])
            .signers(&[&*self.payer, &__scout_signer_buyer])
            .send()
            .expect("send failed");
        __outcome
    }

    /// DIAGNOSTIC twin of `action_wrapper_buy_pt`, regenerated from the CURRENT generated body.
    #[allow(dead_code)]
    pub fn diag_wrapper_buy_pt(&mut self) -> crucible_test_context::TxOutcome {
        let pt_amount: u64 = { let m = self.ctx.token_balance(&self.market_escrow_pt); (m / 16).max(1) };
        let max_base_amount: u64 = { let b = self.ctx.token_balance(&self.ta_base[self.actor]); (b / 8).max(1) };
        let mint_sy_rem_accounts_until: u8 = MINT_SY_ACCOUNTS;
        let __scout_signer_buyer = self.users[self.actor].insecure_clone();
        let buyer = __scout_signer_buyer.pubkey();
        let market = self.market;
        let token_sy_trader = self.ta_sy[self.actor];
        let token_pt_trader = self.ta_pt[self.actor];
        let token_sy_escrow = self.market_escrow_sy;
        let token_pt_escrow = self.market_escrow_pt;
        let address_lookup_table = self.alt;
        let token_program = SPL_TOKEN_ID;
        let sy_program = self.sy_program_id;
        let token_fee_treasury_sy = self.token_treasury_fee_sy;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __outcome = self.ctx
            .program(self.program_id)
            .call(instruction::WrapperBuyPt { pt_amount, max_base_amount, mint_sy_rem_accounts_until })
            .accounts(accounts::WrapperBuyPt {
                buyer: buyer,
                market: market,
                token_sy_trader: token_sy_trader,
                token_pt_trader: token_pt_trader,
                token_sy_escrow: token_sy_escrow,
                token_pt_escrow: token_pt_escrow,
                address_lookup_table: address_lookup_table,
                token_program: token_program,
                sy_program: sy_program,
                token_fee_treasury_sy: token_fee_treasury_sy,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)])
            .signers(&[&*self.payer, &__scout_signer_buyer])
            .send()
            .expect("send failed");
        __outcome
    }

    /// DIAGNOSTIC twin of `action_market_collect_emission`, regenerated from the CURRENT generated body.
    #[allow(dead_code)]
    pub fn diag_market_collect_emission(&mut self) -> crucible_test_context::TxOutcome {
        let emission_index: u16 = 0;
        let __scout_signer_owner = self.users[self.actor].insecure_clone();
        let owner = __scout_signer_owner.pubkey();
        let market = self.market;
        let lp_position = self.lp_position[self.actor];
        let token_emission_escrow = self.token_farm;
        let token_emission_dst = self.ta_farm[self.actor];
        let token_program = SPL_TOKEN_ID;
        let address_lookup_table = self.alt;
        let sy_program = self.sy_program_id;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __outcome = self.ctx
            .program(self.program_id)
            .call(instruction::MarketCollectEmission { emission_index })
            .accounts(accounts::MarketCollectEmission {
                owner: owner,
                market: market,
                lp_position: lp_position,
                token_emission_escrow: token_emission_escrow,
                token_emission_dst: token_emission_dst,
                token_program: token_program,
                address_lookup_table: address_lookup_table,
                sy_program: sy_program,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.market_sy_position, false), AccountMeta::new(self.emission_custodies[MARKET_EMISSION_STREAM], false), AccountMeta::new(self.token_farm, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new_readonly(SPL_TOKEN_ID, false)])
            .signers(&[&*self.payer, &__scout_signer_owner])
            .send()
            .expect("send failed");
        __outcome
    }

    /// DIAGNOSTIC twin of `action_wrapper_provide_liquidity`, regenerated from the CURRENT generated body.
    #[allow(dead_code)]
    pub fn diag_wrapper_provide_liquidity(&mut self) -> crucible_test_context::TxOutcome {
        let amount_base: u64 = { let m = self.ctx.token_balance(&self.market_escrow_pt); (m / 64).max(1) };
        let min_lp_out: u64 = 0;
        let mint_base_accounts_until: u8 = MINT_SY_ACCOUNTS;
        let __scout_signer_depositor = self.users[self.actor].insecure_clone();
        let depositor = __scout_signer_depositor.pubkey();
        let authority = self.vault_authority;
        let vault = self.vault;
        let market = self.market;
        let token_pt_escrow = self.market_escrow_pt;
        let token_sy_escrow = self.market_escrow_sy;
        let token_lp_dst = self.ta_lp[self.actor];
        let mint_lp = self.mint_lp;
        let token_sy_depositor = self.ta_sy[self.actor];
        let escrow_sy = self.escrow_sy;
        let token_yt_depositor = self.ta_yt[self.actor];
        let token_pt_depositor = self.ta_pt[self.actor];
        let mint_yt = self.mint_yt;
        let mint_pt = self.mint_pt;
        let token_program = SPL_TOKEN_ID;
        let vault_address_lookup_table = self.alt;
        let market_address_lookup_table = self.alt;
        let sy_program = self.sy_program_id;
        let user_yield_position = self.yield_position[self.actor];
        let escrow_yt = self.escrow_yt;
        let token_lp_escrow = self.market_escrow_lp;
        let lp_position = self.lp_position[self.actor];
        let vault_robot_yield_position = self.vault_yield_position;
        let system_program = system_program::ID;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __outcome = self.ctx
            .program(self.program_id)
            .call(instruction::WrapperProvideLiquidity { amount_base, min_lp_out, mint_base_accounts_until })
            .accounts(accounts::WrapperProvideLiquidity {
                depositor: depositor,
                authority: authority,
                vault: vault,
                market: market,
                token_pt_escrow: token_pt_escrow,
                token_sy_escrow: token_sy_escrow,
                token_lp_dst: token_lp_dst,
                mint_lp: mint_lp,
                token_sy_depositor: token_sy_depositor,
                escrow_sy: escrow_sy,
                token_yt_depositor: token_yt_depositor,
                token_pt_depositor: token_pt_depositor,
                mint_yt: mint_yt,
                mint_pt: mint_pt,
                token_program: token_program,
                vault_address_lookup_table: vault_address_lookup_table,
                market_address_lookup_table: market_address_lookup_table,
                sy_program: sy_program,
                user_yield_position: user_yield_position,
                escrow_yt: escrow_yt,
                token_lp_escrow: token_lp_escrow,
                lp_position: lp_position,
                vault_robot_yield_position: vault_robot_yield_position,
                system_program: system_program,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)])
            .signers(&[&*self.payer, &__scout_signer_depositor])
            .send()
            .expect("send failed");
        __outcome
    }

    /// DIAGNOSTIC twin of `action_wrapper_provide_liquidity_base`, regenerated from the CURRENT generated body.
    #[allow(dead_code)]
    pub fn diag_wrapper_provide_liquidity_base(&mut self) -> crucible_test_context::TxOutcome {
        let amount_base: u64 = { let m = self.ctx.token_balance(&self.market_escrow_pt); (m / 64).max(1) };
        let min_lp_out: u64 = 0;
        let mint_sy_accounts_until: u8 = MINT_SY_ACCOUNTS;
        let external_pt_to_buy: u64 = { let m = self.ctx.token_balance(&self.market_escrow_pt); (m / 64).max(1) };
        let external_sy_constraint: u64 = { self.ctx.token_balance(&self.ta_sy[self.actor]).max(1) };
        let __scout_signer_depositor = self.users[self.actor].insecure_clone();
        let depositor = __scout_signer_depositor.pubkey();
        let market = self.market;
        let token_pt_escrow = self.market_escrow_pt;
        let token_sy_escrow = self.market_escrow_sy;
        let token_lp_dst = self.ta_lp[self.actor];
        let mint_lp = self.mint_lp;
        let token_sy_depositor = self.ta_sy[self.actor];
        let token_pt_depositor = self.ta_pt[self.actor];
        let token_program = SPL_TOKEN_ID;
        let market_address_lookup_table = self.alt;
        let sy_program = self.sy_program_id;
        let token_fee_treasury_sy = self.token_treasury_fee_sy;
        let token_lp_escrow = self.market_escrow_lp;
        let lp_position = self.lp_position[self.actor];
        let system_program = system_program::ID;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __outcome = self.ctx
            .program(self.program_id)
            .call(instruction::WrapperProvideLiquidityBase { amount_base, min_lp_out, mint_sy_accounts_until, external_pt_to_buy, external_sy_constraint })
            .accounts(accounts::WrapperProvideLiquidityBase {
                depositor: depositor,
                market: market,
                token_pt_escrow: token_pt_escrow,
                token_sy_escrow: token_sy_escrow,
                token_lp_dst: token_lp_dst,
                mint_lp: mint_lp,
                token_sy_depositor: token_sy_depositor,
                token_pt_depositor: token_pt_depositor,
                token_program: token_program,
                market_address_lookup_table: market_address_lookup_table,
                sy_program: sy_program,
                token_fee_treasury_sy: token_fee_treasury_sy,
                token_lp_escrow: token_lp_escrow,
                lp_position: lp_position,
                system_program: system_program,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)])
            .signers(&[&*self.payer, &__scout_signer_depositor])
            .send()
            .expect("send failed");
        __outcome
    }

    /// DIAGNOSTIC twin of `action_wrapper_collect_interest`, regenerated from the CURRENT generated body.
    #[allow(dead_code)]
    pub fn diag_wrapper_collect_interest(&mut self) -> crucible_test_context::TxOutcome {
        let redeem_sy_accounts_length: u8 = REDEEM_SY_ACCOUNTS;
        let __scout_signer_claimer = self.users[self.actor].insecure_clone();
        let claimer = __scout_signer_claimer.pubkey();
        let authority = self.vault_authority;
        let vault = self.vault;
        let address_lookup_table = self.alt;
        let escrow_sy = self.escrow_sy;
        let sy_program = self.sy_program_id;
        let token_program = SPL_TOKEN_ID;
        let yield_position = self.yield_position[self.actor];
        let token_sy_dst = self.ta_sy[self.actor];
        let treasury_sy_token_account = self.treasury_sy_ta;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __outcome = self.ctx
            .program(self.program_id)
            .call(instruction::WrapperCollectInterest { redeem_sy_accounts_length })
            .accounts(accounts::WrapperCollectInterest {
                claimer: claimer,
                authority: authority,
                vault: vault,
                address_lookup_table: address_lookup_table,
                escrow_sy: escrow_sy,
                sy_program: sy_program,
                token_program: token_program,
                yield_position: yield_position,
                token_sy_dst: token_sy_dst,
                treasury_sy_token_account: treasury_sy_token_account,
                event_authority: event_authority,
                program: program,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.ta_sy[self.actor], false), AccountMeta::new(self.sy_mint, false), AccountMeta::new(self.base_custody, false), AccountMeta::new(self.ta_base[self.actor], false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new_readonly(self.users[self.actor].pubkey(), true), AccountMeta::new_readonly(SPL_TOKEN_ID, false), AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new(self.market_sy_position, false)])
            .signers(&[&*self.payer, &__scout_signer_claimer])
            .send()
            .expect("send failed");
        __outcome
    }

    /// DIAGNOSTIC twin of `action_collect_treasury_interest`, regenerated from the CURRENT generated body.
    #[allow(dead_code)]
    pub fn diag_collect_treasury_interest(&mut self) -> crucible_test_context::TxOutcome {
        // SCOUT-TODO: arg kind: exponent_core::types::CollectTreasuryInterestKind
        let amount: exponent_core::types::Amount = exponent_core::types::Amount::All;
        let kind: exponent_core::types::CollectTreasuryInterestKind = Default::default(); // SCOUT-TODO: construct arg kind: exponent_core::types::CollectTreasuryInterestKind
        let __scout_signer_signer = self.payer.insecure_clone();
        let signer = __scout_signer_signer.pubkey();
        let yield_position = self.vault_yield_position;
        let vault = self.vault;
        let sy_dst = self.ta_sy[self.actor];
        let escrow_sy = self.escrow_sy;
        let authority = self.vault_authority;
        let token_program = SPL_TOKEN_ID;
        let sy_program = self.sy_program_id;
        let address_lookup_table = self.alt;
        let admin = self.admin_account;
        let __outcome = self.ctx
            .program(self.program_id)
            .call(instruction::CollectTreasuryInterest { amount, kind })
            .accounts(accounts::CollectTreasuryInterest {
                signer: signer,
                yield_position: yield_position,
                vault: vault,
                sy_dst: sy_dst,
                escrow_sy: escrow_sy,
                authority: authority,
                token_program: token_program,
                sy_program: sy_program,
                address_lookup_table: address_lookup_table,
                admin: admin,
            })
            .remaining_accounts_metas(vec![AccountMeta::new(self.sy_global, false), AccountMeta::new(self.vault_sy_position, false), AccountMeta::new(self.sy_custody, false), AccountMeta::new(self.escrow_sy, false), AccountMeta::new_readonly(self.sy_authority, false), AccountMeta::new_readonly(SPL_TOKEN_ID, false)])
            .signers(&[&*self.payer, &__scout_signer_signer])
            .send()
            .expect("send failed");
        __outcome
    }

    /// DIAGNOSTIC twin of `action_add_lp_tokens_metadata`, regenerated from the CURRENT generated body.
    #[allow(dead_code)]
    pub fn diag_add_lp_tokens_metadata(&mut self) -> crucible_test_context::TxOutcome {
        // SCOUT-TODO: arg name: String; arg symbol: String; arg uri: String
        let name: String = String::new(); // SCOUT-TODO: value for arg name: String
        let symbol: String = String::new(); // SCOUT-TODO: value for arg symbol: String
        let uri: String = String::new(); // SCOUT-TODO: value for arg uri: String
        let payer = self.payer.pubkey();
        let admin = self.admin_account;
        let market = self.market;
        let mint_lp = self.mint_lp;
        let metadata = self.lp_metadata;
        let token_metadata_program = MPL_TOKEN_METADATA_ID;
        let system_program = system_program::ID;
        let __outcome = self.ctx
            .program(self.program_id)
            .call(instruction::AddLpTokensMetadata { name, symbol, uri })
            .accounts(accounts::AddLpTokensMetadata {
                payer: payer,
                admin: admin,
                market: market,
                mint_lp: mint_lp,
                metadata: metadata,
                token_metadata_program: token_metadata_program,
                system_program: system_program,
            })
            .signers(&[&*self.payer])
            .send()
            .expect("send failed");
        __outcome
    }

    /// DIAGNOSTIC twin of `action_initialize_yield_position`, regenerated from the CURRENT generated body.
    #[allow(dead_code)]
    pub fn diag_initialize_yield_position(&mut self) -> crucible_test_context::TxOutcome {
        let __scout_signer_owner = self.users[self.actor].insecure_clone();
        let owner = __scout_signer_owner.pubkey();
        let vault = self.vault;
        let yield_position = self.yield_position[self.actor];
        let system_program = system_program::ID;
        let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let program = self.program_id;
        let __outcome = self.ctx
            .program(self.program_id)
            .call(instruction::InitializeYieldPosition {  })
            .accounts(accounts::InitializeYieldPosition {
                owner: owner,
                vault: vault,
                yield_position: yield_position,
                system_program: system_program,
                event_authority: event_authority,
                program: program,
            })
            .signers(&[&*self.payer, &__scout_signer_owner])
            .send()
            .expect("send failed");
        __outcome
    }

    /// Arm the mock SY program to call BACK into `target` from inside `get_sy_state`, or disarm it
    /// with `None`.
    ///
    /// Exponent reaches `get_sy_state` from the middle of `update_vault_yield`
    /// (`instructions/vault/common.rs:15-26`): the vault has been deserialized, the handler's
    /// mutations have not been written back. That is precisely the window a reentrancy attack
    /// needs, and until now the harness had no way to occupy it (BLIND-SPOTS.md #3) -- the mock's
    /// only CPIs were into SPL Token.
    pub fn mock_sy_arm_reentrancy(&mut self, target: Option<Pubkey>) -> bool {
        let mut data = vec![205u8]; // ix::ARM_REENTRANCY
        if let Some(t) = target { data.extend_from_slice(t.as_ref()); }
        let ix = Instruction {
            program_id: self.sy_program_id,
            accounts: vec![AccountMeta::new(self.sy_global, false)],
            data,
        };
        let payer = self.payer.clone();
        self.ctx.raw_call(ix).signers(&[&*payer]).send()
            .map(|o| o.is_success()).unwrap_or(false)
    }

    /// P-0012 -- ATOMIC strip -> merge round trip, asserted at whatever the CURRENT rate is.
    ///
    /// This is the rounding detector P-0004 cannot be. P-0004's gate pins the rate to its setup
    /// value of exactly 1.0, where `py_to_sy` and `sy_to_py` are the identity and every conversion
    /// is exact -- so the one-unit-per-operation leak it was written to find can never appear under
    /// its own gate (BLIND-SPOTS.md #10). Doing both legs inside ONE action removes the need for
    /// the gate entirely: no other actor moves, the market is not touched, and the rate cannot
    /// change in between, so any SY the actor ends up with beyond what they started with is
    /// created rather than transferred. That makes it sound at 0.337, 3.0, or any other rate the
    /// fuzzer has installed, which is exactly where the rounding lives:
    /// `sy_backing_for_pt` floors (`vault.rs:573-574`) and `py_to_sy_ceil`/`py_to_sy_floor` differ
    /// by leg (`sy_cpi.rs:280-300`).
    ///
    /// Skips rather than asserts unless PT and YT both return to their starting balances. A
    /// post-maturity `merge` burns PT without burning YT (`merge.rs:120-129`), so the actor keeps
    /// YT the round trip did not consume -- that is documented behaviour, not a leak, and asserting
    /// through it would be measuring the maturity rule instead of the arithmetic.
    pub fn action_probe_strip_merge_roundtrip(&mut self, #[range(1..1_000_000_000)] amount: u64) -> bool {
        let a = self.actor;
        let (before_sy, before_pt, before_yt) = (
            self.ctx.token_balance(&self.ta_sy[a]),
            self.ctx.token_balance(&self.ta_pt[a]),
            self.ctx.token_balance(&self.ta_yt[a]),
        );
        if before_sy < amount || amount == 0 { return false; }
        if !self.strip_exact(amount) { return false; }
        let gained = self.ctx.token_balance(&self.ta_pt[a]).saturating_sub(before_pt)
            .min(self.ctx.token_balance(&self.ta_yt[a]).saturating_sub(before_yt));
        if gained == 0 { return false; }
        if !self.merge_exact(gained) { return false; }

        let (after_sy, after_pt, after_yt) = (
            self.ctx.token_balance(&self.ta_sy[a]),
            self.ctx.token_balance(&self.ta_pt[a]),
            self.ctx.token_balance(&self.ta_yt[a]),
        );
        // Not a complete round trip -- say nothing rather than assert on a partial one.
        if after_pt != before_pt || after_yt != before_yt { return true; }

        // SCOUT:ACTION-HOOK:probe_strip_merge_roundtrip:BEGIN
        scout_run_property!("P-0012", {
            // LIVENESS PROBE, same contract as SCOUT_P0004_PROBE: inverts the check so it fails the
            // instant a COMPLETE round trip is reached. A campaign under the probe that reports
            // nothing proves the fuzzer never completes one -- both legs succeeding and PT and YT
            // both returning to baseline is a narrow target, and without this a silent P-0012 is
            // indistinguishable from a probe that never ran.
            let probe = std::env::var("SCOUT_P0012_PROBE").is_ok();
            scout_check!("P-0012", "strip_merge_roundtrip_never_gains", !probe && after_sy <= before_sy,
                "actor {} round-tripped {} SY through strip+merge at rate {} and came out with {} \
                 (started {}, gained {}) -- both legs in one action, market untouched, so this is \
                 value created rather than transferred",
                a, amount, self.sy_exchange_rate(), after_sy, before_sy,
                after_sy.saturating_sub(before_sy));
        });
        // SCOUT:ACTION-HOOK:probe_strip_merge_roundtrip:END
        true
    }

    /// Move a stream's cumulative index BACKWARDS on the SY program, then touch the vault so the
    /// move propagates.
    ///
    /// This models a third-party SY program that reports a lower index than it did before -- a
    /// migration, a redeploy from a fresh account, a precision change, or malice. Exponent's own
    /// comment states the assumption it relies on (`vault.rs:265`, *"Since emissions are
    /// non-decreasing, that is the only constraint"*) and **nothing validates it**:
    /// `update_from_sy_state` writes `self.emissions[index].last_seen_index = *x` unconditionally
    /// (`vault.rs:356-363`), so a rewind lands directly in vault state.
    ///
    /// Reachable only by an action, not by ordinary use -- which is the point. The SY program is
    /// not Exponent's code, and blind spot #2 is that the harness had no way to make it misbehave.
    pub fn action_rewind_emission_index(&mut self, #[range(0..2)] stream: u8, #[range(1..1_000)] to_milli: u32) -> bool {
        let s = stream as usize;
        let registered = self.ctx
            .read_anchor_account::<exponent_core::state::Vault>(&self.vault)
            .map(|v| v.emissions.len()).unwrap_or(0);
        if s >= registered.min(N_EMISSION_STREAMS) { return false; }
        let cur = self.ctx.account_data(&self.sy_global).ok()
            .and_then(|d| d.get(1..17).map(|b| u128::from_le_bytes(b.try_into().unwrap())))
            .unwrap_or(0);
        let _ = cur; // the global's rate, not the index -- read per-stream below
        let target = to_milli as u128 * NUMBER_ONE / 1_000;
        if !self.mock_sy_set_emission_index(s as u32, target) { return false; }
        // Propagate: the vault only re-reads SY state through a refreshing instruction.
        self.action_stage_yt_yield()
    }

    /// Land the clock EXACTLY on maturity, or one second either side of it.
    ///
    /// `is_active` is `now >= start && now <= maturity` and `is_expired` is its complement, so the
    /// boundary is a single second out of 31,536,000 and `action_advance_time`'s 1..500-day steps
    /// will never draw it (blind spot #9). That one second decides whether
    /// `final_sy_exchange_rate` freezes (`vault.rs:352-354`) and whether the post-maturity treasury
    /// sweep opens (`vault.rs:257-263`) -- both of which issue-05 turns on.
    ///
    /// Only ever moves the clock FORWARD, for the same reason `action_advance_time` does: a rewind
    /// re-labels legitimate post-maturity state as active and manufactures P-0002 violations.
    pub fn action_warp_to_maturity(&mut self, #[range(0..3)] offset: u8) -> bool {
        let vault = match self.ctx.read_anchor_account::<exponent_core::state::Vault>(&self.vault) {
            Ok(v) => v, Err(_) => return false,
        };
        let maturity = vault.start_ts.saturating_add(vault.duration) as i64;
        let target = maturity + (offset as i64) - 1; // -1, 0, +1 around the boundary
        let now = self.svm_unix_timestamp().map(|t| t as i64).unwrap_or(self.current_ts as i64);
        if target <= now { return false; }
        Self::warp_clock(&mut self.ctx, target);
        self.current_ts = target as u32;
        true
    }

    /// Advance the clock. Vault behaviour is gated on maturity (`is_active`/`is_expired`), and the
    /// post-maturity "lambo" treasury path -- where `update_from_sy_state` credits `treasury_sy`
    /// from SY appreciation and freezes `final_sy_exchange_rate` -- is unreachable without crossing
    /// that boundary, so `days` must be able to take the clock past the 365-day duration.
    ///
    /// TIME MUST ONLY EVER MOVE FORWARD. This used to recompute an ABSOLUTE timestamp
    /// (`vault_start_ts + days*86400`), so `advance_time(379)` followed by `advance_time(115)`
    /// moved the clock BACKWARDS -- something Solana's Clock never does. That single defect
    /// manufactured every P-0002 "PT != YT while active" crash: `merge` deliberately burns PT
    /// without burning YT once the vault has expired (vault/merge.rs:120-130), and the rewind then
    /// re-labelled that legitimate post-maturity state as "active". Adding to the CURRENT clock
    /// keeps every reachable state reachable and deletes the unreachable ones.
    pub fn action_advance_time(&mut self, #[range(1..500)] days: u16) -> bool {
        let now = self
            .svm_unix_timestamp()
            .map(|t| t as i64)
            .unwrap_or(self.current_ts as i64);
        let next = now + (days as i64) * 24 * 60 * 60;
        Self::warp_clock(&mut self.ctx, next);
        self.current_ts = next as u32;
        true
    }

    /// Acquire SY by spending base through the mock SY program, so actors can keep stripping.
    pub fn action_acquire_sy(&mut self, #[range(1..1_000_000_000)] base_amount: u64) -> bool {
        let actor = self.actor;
        self.mock_sy_mint(actor, base_amount)
    }


    /// Mint SY to `owner` by spending base through the mock SY program (static form, for setup()).
    #[allow(clippy::too_many_arguments)]
    fn run_mock_sy_mint(
        ctx: &mut crucible_test_context::TestContext,
        owner: &Rc<Keypair>, sy_program_id: Pubkey, sy_global: Pubkey,
        base_src: Pubkey, base_custody: Pubkey, sy_mint: Pubkey, sy_dst: Pubkey, base_amount: u64,
    ) {
        let mut data = vec![1u8]; // ix::MINT_SY
        data.extend_from_slice(&base_amount.to_le_bytes());
        let ix = Instruction {
            program_id: sy_program_id,
            accounts: vec![
                AccountMeta::new(sy_global, false),
                AccountMeta::new(base_src, false),
                AccountMeta::new(base_custody, false),
                AccountMeta::new(sy_mint, false),
                AccountMeta::new(sy_dst, false),
                AccountMeta::new_readonly(owner.pubkey(), true),
                AccountMeta::new_readonly(SPL_TOKEN_ID, false),
            ],
            data,
        };
        let o = ctx.raw_call(ix).signers(&[&**owner]).send().expect("mock mint_sy send failed");
        assert!(o.is_success(), "mock mint_sy failed: {:#?}", o.logs());
    }

    /// Real `strip`, used by setup() to give the admin PT to seed the market with.
    #[allow(clippy::too_many_arguments)]
    fn run_strip(
        ctx: &mut crucible_test_context::TestContext,
        program_id: Pubkey, depositor: &Rc<Keypair>, vault: Pubkey, vault_authority: Pubkey,
        sy_src: Pubkey, escrow_sy: Pubkey, yt_dst: Pubkey, pt_dst: Pubkey,
        mint_yt: Pubkey, mint_pt: Pubkey, alt: Pubkey, sy_program_id: Pubkey,
        vault_yield_position: Pubkey, sy_global: Pubkey, vault_sy_position: Pubkey,
        sy_custody: Pubkey, sy_authority: Pubkey, amount: u64,
    ) {
        let o = ctx
            .program(program_id)
            .call(instruction::Strip { amount })
            .accounts(accounts::Strip {
                depositor: depositor.pubkey(),
                authority: vault_authority,
                vault,
                sy_src,
                escrow_sy,
                yt_dst,
                pt_dst,
                mint_yt,
                mint_pt,
                token_program: SPL_TOKEN_ID,
                address_lookup_table: alt,
                sy_program: sy_program_id,
                yield_position: vault_yield_position,
                event_authority: Pubkey::find_program_address(
                    &[b"__event_authority"], &program_id).0,
                program: program_id,
            })
            .remaining_accounts_metas(vec![
                AccountMeta::new(sy_global, false),
                AccountMeta::new(vault_sy_position, false),
                AccountMeta::new(sy_custody, false),
                AccountMeta::new_readonly(sy_authority, false),
            ])
            .signers(&[&**depositor])
            .send()
            .expect("setup strip send failed");
        assert!(o.is_success(), "setup strip failed: {:#?}", o.logs());
    }

    /// The market's own `CpiAccounts`, indexing the SAME ALT as the vault's but pointing at the
    /// market's position/escrow and signing as the market. Slots: 7 market_sy_position,
    /// 8 market_escrow_sy, 9 market.
    fn market_cpi_accounts() -> exponent_core::types::CpiAccounts {
        let c = |alt_index: u8, is_signer: bool, is_writable: bool| {
            exponent_core::types::CpiInterfaceContext { alt_index, is_signer, is_writable }
        };
        exponent_core::types::CpiAccounts {
            get_sy_state: vec![c(0, false, false)],
            deposit_sy: vec![
                c(0, false, true), c(7, false, true), c(8, false, true),
                c(3, false, true), c(9, true, false), c(5, false, false),
            ],
            withdraw_sy: vec![
                c(0, false, true), c(7, false, true), c(3, false, true),
                c(8, false, true), c(6, false, false), c(5, false, false),
            ],
            // The market claims its reward stream from the SY program into `token_farm`. The
            // SOURCE is stream MARKET_EMISSION_STREAM's custody -- the mock resolves which stream
            // it is paying from the MINT of the custody it is handed, so this pair is what ties the
            // market's tracker 0 to the vault's stream 2.
            claim_emission: vec![vec![
                c(0, false, true), c(7, false, true),
                c(10 + 2 * MARKET_EMISSION_STREAM as u8, false, true),
                c(ALT_SLOT_TOKEN_FARM, false, true),
                c(6, false, false), c(5, false, false),
            ]],
            get_position_state: vec![c(7, false, false)],
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_init_market_two(
        ctx: &mut crucible_test_context::TestContext,
        program_id: Pubkey, payer: &Rc<Keypair>, market: Pubkey, vault: Pubkey,
        sy_mint: Pubkey, mint_pt: Pubkey, mint_lp: Pubkey, escrow_pt: Pubkey,
        escrow_sy: Pubkey, escrow_lp: Pubkey, pt_src: Pubkey, sy_src: Pubkey, lp_dst: Pubkey,
        sy_program_id: Pubkey, alt: Pubkey, admin_account: Pubkey, token_treasury_fee_sy: Pubkey,
        market_sy_position: Pubkey, sy_global: Pubkey, sy_custody: Pubkey,
    ) {
        // remaining_accounts are forwarded verbatim to the SY program's init_personal_account for
        // the MARKET's robot account, and then filtered for the market's own deposit_sy.
        let remaining = vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new(market_sy_position, false),
            AccountMeta::new_readonly(market, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new(sy_global, false),
            AccountMeta::new(sy_custody, false),
        ];
        let o = ctx
            .program(program_id)
            .call(instruction::InitMarketTwo {
                ln_fee_rate_root: MARKET_LN_FEE_RATE_ROOT,
                rate_scalar_root: MARKET_RATE_SCALAR_ROOT,
                init_rate_anchor: MARKET_INIT_RATE_ANCHOR,
                sy_exchange_rate: exponent_core::types::Number(number_words(NUMBER_ONE)),
                pt_init: MARKET_PT_INIT,
                sy_init: MARKET_SY_INIT,
                fee_treasury_sy_bps: 100,
                cpi_accounts: Self::market_cpi_accounts(),
                seed_id: MARKET_SEED_ID,
            })
            .accounts(accounts::InitMarketTwo {
                payer: payer.pubkey(),
                admin_signer: payer.pubkey(),
                market,
                vault,
                mint_sy: sy_mint,
                mint_pt,
                mint_lp,
                escrow_pt,
                escrow_sy,
                escrow_lp,
                pt_src,
                sy_src,
                lp_dst,
                token_program: SPL_TOKEN_ID,
                system_program: system_program::ID,
                sy_program: sy_program_id,
                associated_token_program: ASSOCIATED_TOKEN_ID,
                address_lookup_table: alt,
                admin: admin_account,
                token_treasury_fee_sy,
            })
            .remaining_accounts_metas(remaining)
            .signers(&[&**payer])
            .send()
            .expect("init_market_two send failed");
        assert!(o.is_success(), "init_market_two failed: {:#?}", o.logs());
    }


    fn run_init_lp_position(
        ctx: &mut crucible_test_context::TestContext,
        program_id: Pubkey, fee_payer: &Rc<Keypair>, owner: &Rc<Keypair>,
        market: Pubkey, lp_position: Pubkey,
    ) {
        let o = ctx
            .program(program_id)
            .call(instruction::InitLpPosition {})
            .accounts(accounts::InitLpPosition {
                fee_payer: fee_payer.pubkey(),
                owner: owner.pubkey(),
                market,
                lp_position,
                system_program: system_program::ID,
                event_authority: Pubkey::find_program_address(
                    &[b"__event_authority"], &program_id).0,
                program: program_id,
            })
            .signers(&[&**fee_payer])
            .send()
            .expect("init_lp_position send failed");
        assert!(o.is_success(), "init_lp_position failed: {:#?}", o.logs());
    }

    // ================= emission wiring =========================================================
    // The vault's emission list and the mock SY's global stream list are POSITIONALLY paired:
    // `Vault::update_from_sy_state` walks `sy_state.emission_indexes` and writes
    // `self.emissions[index]` (`state/vault.rs:356-364`), so the two lists must have the same
    // length whenever any Exponent instruction runs. Register the stream on the mock and then
    // immediately run `add_emission` on the vault -- an Exponent instruction in between panics with
    // an index-out-of-bounds.

    /// mock SY `[202] add_emission_index(initial, mint)` -- register global stream `n`.
    fn mock_sy_add_emission_index(&mut self, initial_1e12: u128, mint: Pubkey) -> bool {
        let mut data = vec![MOCK_SY_ADD_EMISSION_INDEX];
        data.extend_from_slice(&number_bytes(initial_1e12));
        data.extend_from_slice(mint.as_ref());
        let ix = Instruction {
            program_id: self.sy_program_id,
            accounts: vec![AccountMeta::new(self.sy_global, false)],
            data,
        };
        let payer = self.payer.clone();
        self.ctx.raw_call(ix).signers(&[&*payer]).send()
            .map(|o| o.is_success()).unwrap_or(false)
    }

    /// mock SY `[201] set_emission_index(index, value)` -- absolute assignment of the global
    /// cumulative index, in emission tokens per SY, 1e12 fixed point.
    fn mock_sy_set_emission_index(&mut self, index: u32, value_1e12: u128) -> bool {
        let mut data = vec![MOCK_SY_SET_EMISSION_INDEX];
        data.extend_from_slice(&index.to_le_bytes());
        data.extend_from_slice(&number_bytes(value_1e12));
        let ix = Instruction {
            program_id: self.sy_program_id,
            accounts: vec![AccountMeta::new(self.sy_global, false)],
            data,
        };
        let payer = self.payer.clone();
        self.ctx.raw_call(ix).signers(&[&*payer]).send()
            .map(|o| o.is_success()).unwrap_or(false)
    }

    /// mock SY `[203] fund_emission(index, amount)` against the VAULT's SY position. It accrues
    /// first and then credits `amount`, so `amount = 0` is a pure "touch the position" call.
    fn mock_sy_fund_vault_emission(&mut self, index: u32, amount: u64) -> bool {
        let mut data = vec![MOCK_SY_FUND_EMISSION];
        data.extend_from_slice(&index.to_le_bytes());
        data.extend_from_slice(&amount.to_le_bytes());
        let ix = Instruction {
            program_id: self.sy_program_id,
            accounts: vec![
                AccountMeta::new(self.sy_global, false),
                AccountMeta::new(self.vault_sy_position, false),
            ],
            data,
        };
        let payer = self.payer.clone();
        self.ctx.raw_call(ix).signers(&[&*payer]).send()
            .map(|o| o.is_success()).unwrap_or(false)
    }

    /// `Vault.cpi_accounts` extended with one `claim_emission` list per emission. ALT slots 10/11
    /// are the emission custody (SY side) and the vault's emission escrow (Exponent side); the
    /// mock's `claim_emission` reads `sy_global, sy_position, emission_custody, emission_dst,
    /// sy_authority, token_program` in that order.
    fn vault_cpi_accounts_with_emission(num_emissions: usize) -> exponent_core::types::CpiAccounts {
        let c = |alt_index: u8, is_signer: bool, is_writable: bool| {
            exponent_core::types::CpiInterfaceContext { alt_index, is_signer, is_writable }
        };
        let mut accts = Self::vault_cpi_accounts();
        // Stream `i` gets ITS OWN custody/escrow pair, slots (10 + 2i, 11 + 2i). These used to be
        // hard-coded to (10, 11) for every stream, which was invisible while only one stream ever
        // existed and would have made stream 1 collect stream 0's token: `collect_emission`
        // resolves which stream it is paying from the MINT of the custody it is handed.
        accts.claim_emission = (0..num_emissions)
            .map(|i| {
                let custody = 10 + 2 * i as u8;
                let escrow = 11 + 2 * i as u8;
                vec![
                    c(0, false, true), c(1, false, true), c(custody, false, true),
                    c(escrow, false, true), c(6, false, false), c(5, false, false),
                ]
            })
            .collect();
        accts
    }

    /// The accounts `cpi_claim_emission` needs. `collect_emission` passes ONLY
    /// `ctx.remaining_accounts` as the CPI's account_infos (`vault/collect_emission.rs:104`), so
    /// every account named by `cpi_accounts.claim_emission[i]` must appear here.
    /// The `claim_emission` account list for ONE stream, in the mock's `next_account_info` order.
    /// Must stay in lockstep with the ALT slots assigned in `vault_cpi_accounts_with_emission`.
    fn claim_emission_metas_for(&self, stream: usize) -> Vec<AccountMeta> {
        vec![
            AccountMeta::new(self.sy_global, false),
            AccountMeta::new(self.vault_sy_position, false),
            AccountMeta::new(self.emission_custodies[stream], false),
            AccountMeta::new(self.emission_escrows[stream], false),
            AccountMeta::new_readonly(self.sy_authority, false),
            AccountMeta::new_readonly(SPL_TOKEN_ID, false),
        ]
    }

    /// Every registered stream's `claim_emission` accounts, concatenated. Instructions that refresh
    /// all streams at once need the whole pool; `do_claim_emission` filters it by key.
    #[allow(dead_code)]
    fn claim_emission_metas_all(&self, streams: usize) -> Vec<AccountMeta> {
        let mut v = Vec::new();
        for s in 0..streams {
            v.extend(self.claim_emission_metas_for(s));
        }
        v
    }

    /// `stage_yt_yield` as a raw call, returning the outcome so a caller can read LOGS rather than
    /// only a bool. `stage_yt_yield` is one of the instructions that reaches the SY program through
    /// `do_get_sy_state` (`stage_yield.rs:47`); `strip` does NOT -- it goes through `do_deposit_sy`
    /// (`strip.rs:159`) and takes the SY state from that call's return data, which is why arming a
    /// `get_sy_state` reentry and then calling `strip` tests nothing at all.
    pub fn run_stage_yt_yield(&mut self) -> crucible_test_context::TxOutcome {
        let signer = self.users[self.actor].insecure_clone();
        let payer_kp = self.payer.clone();
        let (event_authority, _) =
            Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        let acc = accounts::StageYtYield {
            payer: signer.pubkey(),
            vault: self.vault,
            user_yield_position: self.yield_position[self.actor],
            yield_position: self.vault_yield_position,
            sy_program: self.sy_program_id,
            address_lookup_table: self.alt,
            system_program: system_program::ID,
            event_authority,
            program: self.program_id,
        };
        let metas = vec![
            AccountMeta::new(self.sy_global, false),
            AccountMeta::new(self.vault_sy_position, false),
            AccountMeta::new(self.sy_custody, false),
            AccountMeta::new_readonly(self.sy_authority, false),
        ];
        let pid = self.program_id;
        self.ctx.program(pid)
            .call(instruction::StageYtYield {})
            .accounts(acc)
            .remaining_accounts_metas(metas)
            .signers(&[&*payer_kp, &signer])
            .send()
            .expect("stage_yt_yield send failed")
    }

    /// Real `add_emission` instruction, signed by the hot admin (the payer).
    ///
    /// NOTE the generated `action_add_emission` binds `authority` to `self.vault_authority` from the
    /// global binding table; for THIS instruction `authority` is a `Signer` checked against the hot
    /// admin set (`vault/admin/add_emission.rs:55-62`), so it must be the payer.
    pub fn run_add_emission(
        &mut self, treasury_fee_bps: u16,
    ) -> crucible_test_context::TxOutcome {
        self.run_add_emission_stream(0, treasury_fee_bps)
    }

    /// Register stream `stream` on the vault. `stream` is the index it will occupy, so the vault
    /// must already hold exactly `stream` emissions and the SY program must report exactly
    /// `stream + 1` -- `Vault::add_emission` reads `sy_state.emission_indexes[self.emissions.len()]`
    /// (vault.rs:377) and panics otherwise, which is the mirror image of issue-02.
    pub fn run_add_emission_stream(
        &mut self, stream: usize, treasury_fee_bps: u16,
    ) -> crucible_test_context::TxOutcome {
        let payer = self.payer.insecure_clone();
        // The vault stores this list wholesale, so it must describe EVERY stream it will then
        // have -- not just the one being added.
        let cpi_accounts = Self::vault_cpi_accounts_with_emission(stream + 1);
        // `AddEmission`'s own account list carries NONE of the deposit_sy accounts, so all six named
        // by `cpi_accounts.deposit_sy` have to arrive as remaining_accounts: sy_global,
        // vault_sy_position, escrow_sy, sy_custody, vault_authority, token_program.
        // (`do_deposit_sy` filters the combined pool by key and silently drops anything absent.)
        let remaining = vec![
            AccountMeta::new(self.sy_global, false),
            AccountMeta::new(self.vault_sy_position, false),
            AccountMeta::new(self.escrow_sy, false),
            AccountMeta::new(self.sy_custody, false),
            AccountMeta::new_readonly(self.vault_authority, false),
            AccountMeta::new_readonly(SPL_TOKEN_ID, false),
            AccountMeta::new_readonly(self.sy_authority, false),
        ];
        self.ctx
            .program(self.program_id)
            .call(instruction::AddEmission { cpi_accounts, treasury_fee_bps })
            .accounts(accounts::AddEmission {
                authority: payer.pubkey(),
                fee_payer: payer.pubkey(),
                vault: self.vault,
                admin: self.admin_account,
                sy_program: self.sy_program_id,
                address_lookup_table: self.alt,
                robot_token_account: self.emission_escrow,
                treasury_token_account: self.treasury_emission_ta,
                yield_position: self.vault_yield_position,
                system_program: system_program::ID,
            })
            .remaining_accounts_metas(remaining)
            .signers(&[&payer])
            .send()
            .expect("add_emission send failed")
    }

    /// Real `collect_emission` for the currently selected actor. Returns the outcome so a caller can
    /// read logs / error codes rather than only a bool.
    pub fn run_collect_emission(
        &mut self, index: u16, amount: exponent_core::types::Amount,
    ) -> crucible_test_context::TxOutcome {
        let owner = self.users[self.actor].insecure_clone();
        let position = self.yield_position[self.actor];
        // Destination, escrow and treasury account must all be the ones for THIS stream, or the
        // instruction pays the wrong token out of the wrong escrow.
        let s = index as usize;
        let emission_dst = self.ta_emissions[s][self.actor];
        let remaining = self.claim_emission_metas_for(s);
        let (event_authority, _) =
            Pubkey::find_program_address(&[b"__event_authority"], &self.program_id);
        self.ctx
            .program(self.program_id)
            .call(instruction::CollectEmission { index, amount })
            .accounts(accounts::CollectEmission {
                owner: owner.pubkey(),
                vault: self.vault,
                position,
                sy_program: self.sy_program_id,
                authority: self.vault_authority,
                emission_escrow: self.emission_escrows[s],
                emission_dst,
                address_lookup_table: self.alt,
                treasury_emission_token_account: self.treasury_emission_tas[s],
                token_program: SPL_TOKEN_ID,
                event_authority,
                program: self.program_id,
            })
            .remaining_accounts_metas(remaining)
            .signers(&[&*self.payer, &owner])
            .send()
            .expect("collect_emission send failed")
    }

    /// Decode `(yt_balance, interest.staged, emissions[i].last_seen_index, emissions[i].staged)`
    /// straight out of a `YieldTokenPosition` account. Layout: 8 disc, 32 owner, 32 vault,
    /// 8 yt_balance, interest = (32 Number, 8 staged), then `Vec<YieldTokenTracker>` (4-byte LE
    /// length, 40 bytes each). The index comes back as a raw u128 in 1e12 fixed point.
    fn read_position_emission(&self, position: &Pubkey, i: usize) -> (u64, u64, u128, u64) {
        let data = self.ctx.account_data(position).expect("yield position not found");
        let yt_balance = u64::from_le_bytes(data[72..80].try_into().unwrap());
        let interest_staged = u64::from_le_bytes(data[112..120].try_into().unwrap());
        let n = u32::from_le_bytes(data[120..124].try_into().unwrap()) as usize;
        assert!(i < n, "position has {} emission trackers, wanted index {}", n, i);
        let off = 124 + i * 40;
        let index = u128::from_le_bytes(data[off..off + 16].try_into().unwrap());
        assert_eq!(&data[off + 16..off + 32], &[0u8; 16],
                   "emission index does not fit in the low 128 bits");
        let staged = u64::from_le_bytes(data[off + 32..off + 40].try_into().unwrap());
        (yt_balance, interest_staged, index, staged)
    }

    /// `(sy_balance, emissions[i].amount_claimable)` of the VAULT's position with the mock SY.
    /// Mock layout (`fuzz/mock_sy/src/state.rs`): 1 tag byte, 32 owner, 8 sy_balance, 4 vec len,
    /// then 72-byte `Emission { mint: 32, amount_claimable: 8, last_seen_index: 32 }`.
    fn read_vault_sy_position(&self, i: usize) -> (u64, u64) {
        let data = self.ctx.account_data(&self.vault_sy_position).expect("sy position not found");
        let sy_balance = u64::from_le_bytes(data[33..41].try_into().unwrap());
        let n = u32::from_le_bytes(data[41..45].try_into().unwrap()) as usize;
        if i >= n {
            return (sy_balance, 0);
        }
        let off = 45 + i * 72 + 32;
        let claimable = u64::from_le_bytes(data[off..off + 8].try_into().unwrap());
        (sy_balance, claimable)
    }


    /// The actor's own YT balance inside their `YieldTokenPosition` (offset 72, after the 8-byte
    /// discriminator, `owner` and `vault`). Needed to clamp `withdraw_yt`/`market_withdraw_lp` to
    /// something the position can actually satisfy.
    fn position_yt_balance(&self, actor: usize) -> u64 {
        self.ctx.account_data(&self.yield_position[actor])
            .ok()
            .and_then(|d| d.get(72..80).map(|b| u64::from_le_bytes(b.try_into().unwrap())))
            .unwrap_or(0)
    }

    // ================= P-0004 / P-0007 readers =================================================

    /// The mock SY program's current exchange rate, 1e12 fixed point. `SyGlobal` is plain borsh
    /// behind a 1-byte account-kind tag (`fuzz/mock_sy/src/state.rs`), so `exchange_rate: Number`
    /// -- `[u64; 4]`, little-endian -- starts at offset 1. Only the low 128 bits are read; the
    /// high half is asserted zero because a rate that overflowed it would make every py conversion
    /// below silently wrong.
    pub fn sy_exchange_rate(&self) -> u128 {
        let d = match self.ctx.account_data(&self.sy_global) { Ok(d) => d, Err(_) => return 0 };
        let (lo, hi) = match (d.get(1..17), d.get(17..33)) { (Some(a), Some(b)) => (a, b), _ => return 0 };
        if hi != [0u8; 16] { return 0; } // unreadable => gate stays shut, never a violation
        u128::from_le_bytes(lo.try_into().unwrap())
    }

    /// `Vault.all_time_high_sy_exchange_rate`, low 128 bits. Offset 369 is the same one the
    /// PoC module's `vault_rates` reads and is pinned in CLAUDE.md.
    pub fn vault_ath(&self) -> u128 {
        self.ctx.account_data(&self.vault).ok()
            .and_then(|d| d.get(369..385).map(|b| u128::from_le_bytes(b.try_into().unwrap())))
            .unwrap_or(0)
    }

    /// The adversary's (`users[0]`) total value, denominated in **py units**.
    ///
    /// py and base are the SAME unit: the mock mints `sy = floor(base * 1e12 / rate)`
    /// (`fuzz/mock_sy/src/lib.rs:21`) and Exponent's `py = sy * rate`, so `py == base` exactly.
    /// That is what lets base tokens, SY and PT be added together at all.
    ///
    /// WHAT IS COUNTED, and why each is right at a FROZEN rate -- which is the only condition
    /// P-0004 ever asserts under:
    ///  * base           -- 1 py each, per the identity above.
    ///  * SY             -- `sy * rate` py.
    ///  * PT             -- 1 py each. `merge` returns exactly `py_to_sy(n)` for n PT + n YT.
    ///  * staged interest -- SY the position can already withdraw; a CLAIM, not a transfer, and the
    ///                       playbook is explicit that claims must be counted or a corrupted claim
    ///                       reads as no change at all.
    ///  * YT             -- deliberately ZERO. At a frozen rate YT accrues nothing further
    ///                      (`calc_earned_sy` returns 0 when `last_seen >= cur`), so all of its
    ///                      realizable value is already in `staged`. Counting it again would
    ///                      double-count `strip`, which hands out PT and YT together.
    ///  * LP / emission / farm tokens -- NOT counted. LP is excluded by the market-restoration
    ///    gate; emission and farm tokens are a separate unit with their own property (P-0003), and
    ///    folding a KNOWN-broken channel into this one would make P-0004 fire on issue-01 forever
    ///    and mask anything new.
    pub fn adversary_value_py(&self) -> u128 {
        const ADVERSARY: usize = 0;
        let rate = self.sy_exchange_rate();
        if rate == 0 { return 0; }
        let base = self.ctx.token_balance(&self.ta_base[ADVERSARY]) as u128;
        let sy = self.ctx.token_balance(&self.ta_sy[ADVERSARY]) as u128;
        let pt = self.ctx.token_balance(&self.ta_pt[ADVERSARY]) as u128;
        let staged = self.position_interest_staged(ADVERSARY) as u128;
        base + pt + (sy * rate) / NUMBER_ONE + (staged * rate) / NUMBER_ONE
    }

    /// P-0004's gate. See the long rationale on the invariant itself; in short, an adversary gain
    /// only means value CREATION when no yield was earned and no counterparty took the other side.
    /// Every condition except `sy_rate_moved` is re-derived from on-chain state rather than
    /// tracked, so the gate cannot be wrongly opened by a fixture-template fallback.
    pub fn p0004_gate_open(&self) -> bool {
        let rate = self.sy_exchange_rate();
        !self.sy_rate_moved
            && rate != 0
            && rate == self.baseline_sy_rate
            && self.vault_ath() == self.baseline_ath
            && self.ctx.token_balance(&self.market_escrow_pt) == self.baseline_market_pt
            && self.ctx.token_balance(&self.market_escrow_sy) == self.baseline_market_sy
            && self.market_sy_position_balance() == self.baseline_market_sy_position
            && self.mint_supply(&self.mint_lp).unwrap_or(0) == self.baseline_lp_supply
    }

    /// How many emission trackers a `YieldTokenPosition` currently carries. The vec length sits at
    /// offset 120, after 8 disc + 32 owner + 32 vault + 8 yt_balance + 40 interest tracker.
    /// Zero for a position created before the vault had any emission.
    pub fn tracker_count_of(f: &ExponentCoreFixture, position: &Pubkey) -> u32 {
        f.ctx.account_data(position).ok()
            .and_then(|d| d.get(120..124).map(|b| u32::from_le_bytes(b.try_into().unwrap())))
            .unwrap_or(0)
    }

    /// `sy_balance` of the MARKET's position with the mock SY program. Same layout as
    /// `read_vault_sy_position`: 1 tag byte, 32 owner, then 8 bytes of balance.
    pub fn market_sy_position_balance(&self) -> u64 {
        self.ctx.account_data(&self.market_sy_position).ok()
            .and_then(|d| d.get(33..41).map(|b| u64::from_le_bytes(b.try_into().unwrap())))
            .unwrap_or(0)
    }

    /// `YieldTokenPosition.interest.staged` -- offset 112, the same one the issue-04 PoC reads.
    /// Zero when the position does not exist yet, which is correct: nothing is claimable.
    fn position_interest_staged(&self, actor: usize) -> u64 {
        self.ctx.account_data(&self.yield_position[actor]).ok()
            .and_then(|d| d.get(112..120).map(|b| u64::from_le_bytes(b.try_into().unwrap())))
            .unwrap_or(0)
    }

    /// The actor's LP balance inside their `LpPosition` (8-byte discriminator, owner, market, then
    /// `lp_balance`).
    fn position_lp_balance(&self, actor: usize) -> u64 {
        self.ctx.account_data(&self.lp_position[actor])
            .ok()
            .and_then(|d| d.get(72..80).map(|b| u64::from_le_bytes(b.try_into().unwrap())))
            .unwrap_or(0)
    }


    // ================= explicit-amount helpers for PoCs ========================================
    // The generated actions derive their amounts from live state (see the clamp rationale in
    // SCOUT:BINDINGS), so their signatures carry no amount and will change again if the clamps
    // change. PoCs need EXACT amounts and must not break when that happens, so they call these.

    pub fn strip_exact(&mut self, amount: u64) -> bool {
        let d = self.users[self.actor].insecure_clone();
        let ea = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id).0;
        let m = self.sy_cpi_metas_full();
        let (pid, vault, auth) = (self.program_id, self.vault, self.vault_authority);
        let (sy, yt, pt) = (self.ta_sy[self.actor], self.ta_yt[self.actor], self.ta_pt[self.actor]);
        let (esy, myt, mpt) = (self.escrow_sy, self.mint_yt, self.mint_pt);
        let (alt, syp, vyp) = (self.alt, self.sy_program_id, self.vault_yield_position);
        self.ctx.program(pid)
            .call(instruction::Strip { amount })
            .accounts(accounts::Strip {
                depositor: d.pubkey(), authority: auth, vault, sy_src: sy, escrow_sy: esy,
                yt_dst: yt, pt_dst: pt, mint_yt: myt, mint_pt: mpt,
                token_program: SPL_TOKEN_ID, address_lookup_table: alt, sy_program: syp,
                yield_position: vyp, event_authority: ea, program: pid,
            })
            .remaining_accounts_metas(m).signers(&[&d]).send()
            .map(|o| o.is_success()).unwrap_or(false)
    }

    pub fn merge_exact(&mut self, amount: u64) -> bool {
        let o = self.users[self.actor].insecure_clone();
        let ea = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id).0;
        let m = self.sy_cpi_metas_full();
        let (pid, vault, auth) = (self.program_id, self.vault, self.vault_authority);
        let (sy, yt, pt) = (self.ta_sy[self.actor], self.ta_yt[self.actor], self.ta_pt[self.actor]);
        let (esy, myt, mpt) = (self.escrow_sy, self.mint_yt, self.mint_pt);
        let (alt, syp, vyp) = (self.alt, self.sy_program_id, self.vault_yield_position);
        self.ctx.program(pid)
            .call(instruction::Merge { amount })
            .accounts(accounts::Merge {
                owner: o.pubkey(), authority: auth, vault, sy_dst: sy, escrow_sy: esy,
                yt_src: yt, pt_src: pt, mint_yt: myt, mint_pt: mpt,
                token_program: SPL_TOKEN_ID, sy_program: syp, address_lookup_table: alt,
                yield_position: vyp, event_authority: ea, program: pid,
            })
            .remaining_accounts_metas(m).signers(&[&o]).send()
            .map(|o| o.is_success()).unwrap_or(false)
    }

    pub fn deposit_yt_exact(&mut self, amount: u64) -> bool {
        let d = self.users[self.actor].insecure_clone();
        let ea = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id).0;
        let m = self.sy_cpi_metas_full();
        let (pid, vault, syp, alt) = (self.program_id, self.vault, self.sy_program_id, self.alt);
        let (uyp, yt, eyt, vyp) =
            (self.yield_position[self.actor], self.ta_yt[self.actor], self.escrow_yt,
             self.vault_yield_position);
        self.ctx.program(pid)
            .call(instruction::DepositYt { amount })
            .accounts(accounts::DepositYt {
                depositor: d.pubkey(), vault, user_yield_position: uyp, yt_src: yt,
                escrow_yt: eyt, token_program: SPL_TOKEN_ID, sy_program: syp,
                address_lookup_table: alt, yield_position: vyp,
                system_program: system_program::ID, event_authority: ea, program: pid,
            })
            .remaining_accounts_metas(m).signers(&[&d]).send()
            .map(|o| o.is_success()).unwrap_or(false)
    }

    pub fn withdraw_yt_exact(&mut self, amount: u64) -> bool {
        let o = self.users[self.actor].insecure_clone();
        let ea = Pubkey::find_program_address(&[b"__event_authority"], &self.program_id).0;
        let m = self.sy_cpi_metas_full();
        let (pid, vault, syp, alt, auth) =
            (self.program_id, self.vault, self.sy_program_id, self.alt, self.vault_authority);
        let (uyp, yt, eyt, vyp) =
            (self.yield_position[self.actor], self.ta_yt[self.actor], self.escrow_yt,
             self.vault_yield_position);
        self.ctx.program(pid)
            .call(instruction::WithdrawYt { amount })
            .accounts(accounts::WithdrawYt {
                owner: o.pubkey(), vault, user_yield_position: uyp, yt_dst: yt,
                escrow_yt: eyt, token_program: SPL_TOKEN_ID, authority: auth, sy_program: syp,
                address_lookup_table: alt, yield_position: vyp,
                system_program: system_program::ID, event_authority: ea, program: pid,
            })
            .remaining_accounts_metas(m).signers(&[&o]).send()
            .map(|o| o.is_success()).unwrap_or(false)
    }

    /// Every account the vault-side SY CPIs can name, in one list.
    fn sy_cpi_metas_full(&self) -> Vec<AccountMeta> {
        vec![
            AccountMeta::new(self.sy_global, false),
            AccountMeta::new(self.vault_sy_position, false),
            AccountMeta::new(self.sy_custody, false),
            AccountMeta::new_readonly(self.sy_authority, false),
        ]
    }


    /// The mock SY `mint_sy` account list, in the order that program reads it
    /// (`fuzz/mock_sy/src/lib.rs`): sy_global, base_src, base_custody, sy_mint, sy_dst,
    /// user_authority(signer), token_program. SEVEN accounts -- the wrappers slice
    /// `remaining_accounts[..mint_sy_accounts_until]` and hand exactly this to `cpi_mint_sy`, whose
    /// metas come from these AccountInfos' own flags, so the signer flag has to be right here.
    fn mint_sy_metas(&self) -> Vec<AccountMeta> {
        vec![
            AccountMeta::new(self.sy_global, false),
            AccountMeta::new(self.ta_base[self.actor], false),
            AccountMeta::new(self.base_custody, false),
            AccountMeta::new(self.sy_mint, false),
            AccountMeta::new(self.ta_sy[self.actor], false),
            AccountMeta::new_readonly(self.users[self.actor].pubkey(), true),
            AccountMeta::new_readonly(SPL_TOKEN_ID, false),
        ]
    }

    /// The mock SY `redeem_sy` account list: sy_global, sy_src, sy_mint, base_custody, base_dst,
    /// sy_authority, user_authority(signer), token_program. EIGHT accounts.
    fn redeem_sy_metas(&self) -> Vec<AccountMeta> {
        vec![
            AccountMeta::new(self.sy_global, false),
            AccountMeta::new(self.ta_sy[self.actor], false),
            AccountMeta::new(self.sy_mint, false),
            AccountMeta::new(self.base_custody, false),
            AccountMeta::new(self.ta_base[self.actor], false),
            AccountMeta::new_readonly(self.sy_authority, false),
            AccountMeta::new_readonly(self.users[self.actor].pubkey(), true),
            AccountMeta::new_readonly(SPL_TOKEN_ID, false),
        ]
    }

    /// `mint_sy` accounts followed by everything the vault- and market-side SY CPIs can name.
    /// The wrappers split this list at `mint_sy_accounts_until` (= MINT_SY_ACCOUNTS).
    fn wrapper_mint_metas(&self) -> Vec<AccountMeta> {
        let mut v = self.mint_sy_metas();
        v.extend(self.sy_cpi_metas_full());
        v.push(AccountMeta::new(self.market_sy_position, false));
        v
    }

    fn wrapper_redeem_metas(&self) -> Vec<AccountMeta> {
        let mut v = self.redeem_sy_metas();
        v.extend(self.sy_cpi_metas_full());
        v.push(AccountMeta::new(self.market_sy_position, false));
        v
    }



    /// Wall-clock read from the SVM's Clock sysvar -- GROUND TRUTH, not a mirrored Rust field.
    /// A field updated by `action_advance_time` desyncs from the VM whenever the fuzzer restores
    /// the fixture snapshot between iterations, and a property built on it then reports violations
    /// on healthy runs. That is exactly how the P-0007 high-water mark produced 100+ false
    /// positives, so this reads the account instead.
    /// `Clock`: slot u64, epoch_start_timestamp i64, epoch u64, leader_schedule_epoch u64,
    /// unix_timestamp i64 -- so unix_timestamp is at offset 32.
    pub fn svm_unix_timestamp(&self) -> Option<u32> {
        let clock_id = Pubkey::new_from_array([
            6, 167, 213, 23, 24, 199, 116, 201, 40, 86, 99, 152, 105, 29, 94, 182,
            139, 94, 184, 163, 155, 75, 109, 92, 115, 85, 91, 33, 0, 0, 0, 0,
        ]);
        let d = self.ctx.account_data(&clock_id).ok()?;
        let ts = i64::from_le_bytes(d.get(32..40)?.try_into().ok()?);
        u32::try_from(ts).ok()
    }

    // ================= invariant decoders ======================================================
    // These live in SCOUT:EXTRA-ACTIONS, NOT at file scope: anything outside a SCOUT region is
    // deleted by regeneration, and putting them in SCOUT:PRELUDE is impossible because they name
    // generated types (`exponent_core::types::Number`) that only exist after
    // `declare_fuzz_program!`.

    /// SPL mint supply. `Mint` layout: 4-byte COption tag + 32-byte authority = 36, then `supply`.
    pub fn mint_supply(&self, mint: &Pubkey) -> Option<u64> {
        let d = self.ctx.account_data(mint).ok()?;
        d.get(36..44).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }

    /// `(last_seen_index, staged)` of a position's emission tracker `i`.
    /// `YieldTokenPosition`: 8 disc, 32 owner, 32 vault, 8 yt_balance, interest = (32 Number,
    /// 8 staged), then `Vec<YieldTokenTracker>` (4-byte LE len, 40 bytes each = 32-byte Number
    /// index + 8-byte staged).
    pub fn position_tracker(&self, position: &Pubkey, i: usize) -> (u128, u64) {
        let Ok(d) = self.ctx.account_data(position) else { return (0, 0) };
        let Some(lb) = d.get(120..124) else { return (0, 0) };
        let n = u32::from_le_bytes(lb.try_into().unwrap()) as usize;
        if i >= n { return (0, 0); }
        let off = 124 + i * 40;
        let (Some(ib), Some(sb)) = (d.get(off..off + 16), d.get(off + 32..off + 40))
            else { return (0, 0) };
        (u128::from_le_bytes(ib.try_into().unwrap()),
         u64::from_le_bytes(sb.try_into().unwrap()))
    }

    /// `LpPosition`: 8 disc, 32 owner, 32 market, then `lp_balance: u64`.
    pub fn position_lp_balance_of(
        ctx: &crucible_test_context::TestContext, position: &Pubkey,
    ) -> u64 {
        ctx.account_data(position).ok()
            .and_then(|d| d.get(72..80).map(|b| u64::from_le_bytes(b.try_into().unwrap())))
            .unwrap_or(0)
    }

    pub fn position_staged_emission(&self, position: &Pubkey, i: usize) -> u64 {
        self.position_tracker(position, i).1
    }

    /// The low 128 bits of a `Number` ([u64; 4] LE words, 1e12 fixed point). Every rate this
    /// harness sets fits well inside 128 bits; a larger value would read as truncated, which can
    /// only make a monotonicity check MORE permissive, never manufacture a violation.
    pub fn number_u128(n: &exponent_core::types::Number) -> u128 {
        (n.0[0] as u128) | ((n.0[1] as u128) << 64)
    }

    /// High-water mark for P-0007. "Never decreases ACROSS actions" cannot be checked from a single
    /// post-state snapshot, so the previous value is kept in the harness process. A stale-high mark
    /// could only ever cause a FALSE POSITIVE, so the mark is re-seeded on every observation and
    /// the check is evaluated against the immediately preceding value.
    // `ts_high_water` and `ath_high_water` used to live here as `thread_local!` marks. Both are
    // gone. A thread-local outlives the fuzzer's snapshot restore while the state it describes does
    // not, which is what made P-0007 unusable; `ts_seen` and `ath_seen` are fixture fields now, so
    // the mark and the state it is compared against always come from the same point in history.

    // ================= setup helpers ==========================================================
    // These live here rather than at file scope because they name generated types
    // (`instruction::*`, `accounts::*`, `exponent_core::types::*`), which only exist after
    // `declare_fuzz_program!` -- and because anything outside a SCOUT region is deleted on regen.

    /// Associated token address for `owner`/`mint` under the classic SPL Token program.
    fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(
            &[owner.as_ref(), SPL_TOKEN_ID.as_ref(), mint.as_ref()],
            &ASSOCIATED_TOKEN_ID,
        ).0
    }

    /// Pin the clock to `unix_timestamp`. The vault gates on wall-clock time
    /// (`Vault::is_active`/`is_expired`, `util::now()`), so time must move only when an action
    /// says so -- otherwise maturity-dependent behaviour drifts between runs.
    fn warp_clock(ctx: &mut crucible_test_context::TestContext, unix_timestamp: i64) {
        let clock = anchor_lang::prelude::Clock {
            slot: 1,
            epoch_start_timestamp: unix_timestamp,
            epoch: 0,
            leader_schedule_epoch: 0,
            unix_timestamp,
        };
        ctx.set_sysvar(&clock);
    }

    fn make_token_account(
        ctx: &mut crucible_test_context::TestContext,
        mint: Pubkey, owner: Pubkey, amount: u64,
    ) -> Pubkey {
        let addr = Pubkey::new_unique();
        ctx.create_token_account().pubkey(addr).mint(mint).token_owner(owner)
            .amount(amount).create().unwrap();
        addr
    }

    /// Create the mock SY program's global state with a starting exchange rate of exactly 1.0.
    ///
    /// `exchange_rate` is underlying-per-SY in 1e12 fixed point; starting at 1.0 makes the first
    /// strip's PT/YT amounts equal to the SY in, which keeps early counterexamples readable.
    fn init_mock_sy(
        ctx: &mut crucible_test_context::TestContext,
        payer: &Rc<Keypair>, sy_program_id: Pubkey, sy_global: Pubkey,
    ) {
        let mut data = vec![MOCK_SY_INIT_GLOBAL];
        data.extend_from_slice(&number_whole(1));
        let ix = Instruction {
            program_id: sy_program_id,
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new(sy_global, false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
            data,
        };
        let outcome = ctx.raw_call(ix).signers(&[&**payer]).send()
            .expect("mock SY init_global send failed");
        assert!(outcome.is_success(), "mock SY init_global failed: {:?}", outcome.logs());
    }

    /// The `CpiAccounts` the vault stores: ALT INDICES, not pubkeys. Each inner list is the exact
    /// account order the mock SY program's corresponding handler reads with `next_account_info`,
    /// so these two must be changed together. Slots are assigned in setup():
    /// 0 sy_global · 1 vault_sy_position · 2 escrow_sy · 3 sy_custody · 4 vault_authority ·
    /// 5 token_program · 6 sy_authority.
    fn vault_cpi_accounts() -> exponent_core::types::CpiAccounts {
        let c = |alt_index: u8, is_signer: bool, is_writable: bool| {
            exponent_core::types::CpiInterfaceContext { alt_index, is_signer, is_writable }
        };
        exponent_core::types::CpiAccounts {
            // get_sy_state(sy_global, exponent_program) -- see the ALT comment on slot 10 + 2N
            get_sy_state: vec![c(0, false, false), c(ALT_SLOT_EXPONENT, false, false)],
            // deposit_sy(sy_global, sy_position, sy_src, sy_custody, src_authority, token_program)
            // src_authority is the vault authority PDA; Exponent signs the CPI for it with
            // `vault.signer_seeds()`, so the meta must be marked signer.
            deposit_sy: vec![
                c(0, false, true), c(1, false, true), c(2, false, true),
                c(3, false, true), c(4, true, false), c(5, false, false),
            ],
            // withdraw_sy(sy_global, sy_position, sy_custody, sy_dst, sy_authority, token_program)
            // sy_authority is the MOCK's own PDA -- it signs internally via invoke_signed, so at
            // this level it is an ordinary read-only account.
            withdraw_sy: vec![
                c(0, false, true), c(1, false, true), c(3, false, true),
                c(2, false, true), c(6, false, false), c(5, false, false),
            ],
            // No emissions are configured at vault creation; add_emission extends this later.
            claim_emission: vec![],
            // get_position(sy_position)
            get_position_state: vec![c(1, false, false)],
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_initialize_vault(
        ctx: &mut crucible_test_context::TestContext,
        program_id: Pubkey, payer: &Rc<Keypair>, vault_kp: &Keypair,
        admin_account: Pubkey, vault_authority: Pubkey, mint_pt: Pubkey, mint_yt: Pubkey,
        escrow_yt: Pubkey, escrow_sy: Pubkey, sy_mint: Pubkey, treasury_sy_ta: Pubkey,
        sy_program_id: Pubkey, alt: Pubkey, vault_yield_position: Pubkey, metadata: Pubkey,
        vault_sy_position: Pubkey, start_ts: u32, duration: u32,
    ) {
        // remaining_accounts are forwarded verbatim to the SY program's init_personal_account
        // (`cpi_init_sy_personal_account` builds its metas with `to_metas(rem_accounts)`), so this
        // list IS that instruction's account order and its flags must be right.
        let remaining = vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new(vault_sy_position, false),
            AccountMeta::new_readonly(vault_authority, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ];
        let outcome = ctx
            .program(program_id)
            .call(instruction::InitializeVault {
                start_timestamp: start_ts,
                duration,
                interest_bps_fee: 1_000, // 10% of YT interest to the treasury
                cpi_accounts: Self::vault_cpi_accounts(),
                min_op_size_strip: 0,
                min_op_size_merge: 0,
                pt_metadata_name: "PT".to_string(),
                pt_metadata_symbol: "PT".to_string(),
                pt_metadata_uri: "https://x.invalid/pt.json".to_string(),
            })
            .accounts(accounts::InitializeVault {
                payer: payer.pubkey(),
                admin: admin_account,
                authority: vault_authority,
                vault: vault_kp.pubkey(),
                mint_pt,
                mint_yt,
                escrow_yt,
                escrow_sy,
                mint_sy: sy_mint,
                system_program: system_program::ID,
                token_program: SPL_TOKEN_ID,
                treasury_token_account: treasury_sy_ta,
                associated_token_program: ASSOCIATED_TOKEN_ID,
                sy_program: sy_program_id,
                address_lookup_table: alt,
                yield_position: vault_yield_position,
                metadata,
                token_metadata_program: MPL_TOKEN_METADATA_ID,
            })
            .remaining_accounts_metas(remaining)
            .signers(&[&**payer, vault_kp])
            .send()
            .expect("initialize_vault send failed");
        assert!(outcome.is_success(), "initialize_vault failed: {:#?}", outcome.logs());
    }
    // SCOUT:EXTRA-ACTIONS:END
}

#[invariant_test]
fn invariant_test(_f: &mut ExponentCoreFixture) {
    scout_check_session!();
    // SCOUT:INVARIANTS:BEGIN
    // NOTE: `#[invariant_test]` rewrites this function's signature and renames the parameter to
    // `fixture` (crucible-invariant-macro/src/lib.rs:1354), so the body must use `fixture`, not the
    // `_f` in the declaration above.
    // Read the world once; every property below reads from persisted state only.
    // SCOUT:INVARIANT:P-0001:BEGIN
    // Vault solvency. The protocol states this itself as `Vault::sy_balance_invariant`
    // (state/vault.rs:110-113) and then never calls it -- it is `#[cfg(test)]` with ZERO call sites
    // anywhere in the repo, including tests. So this is not a mirror of a live check; it is the
    // check the code describes and never performs.
    //
    // False-positive reasoning: the sum is computed in u128 so it cannot wrap, and the comparison
    // is the exact inequality the source states. `active_sy()` (vault.rs:308-313) clamps underflow
    // with `unwrap_or(0)`, which can only HIDE a violation, never manufacture one -- so this check
    // is strictly more permissive than the truth, which is the right direction.
    scout_run_property!("P-0001", {
        // Vault read lives inside the property block: SCOUT:INVARIANTS permits only
        // comments between blocks, and a property block only its scout_run_property! stmt.
        let Ok(vault) = fixture.ctx.read_anchor_account::<exponent_core::state::Vault>(&fixture.vault)
        else { return }; // vault unreadable => nothing ran; not a violation
        let lhs = vault.total_sy_in_escrow as u128;
        let rhs = vault.sy_for_pt as u128
            + vault.treasury_sy as u128
            + vault.uncollected_sy as u128;
        scout_check!("P-0001", "vault_solvency", lhs >= rhs,
            "total_sy_in_escrow={} < sy_for_pt={} + treasury_sy={} + uncollected_sy={} (sum={})",
            vault.total_sy_in_escrow, vault.sy_for_pt, vault.treasury_sy,
            vault.uncollected_sy, rhs);
    });
    // SCOUT:INVARIANT:P-0001:END

    // SCOUT:INVARIANT:P-0002:BEGIN
    // PT and YT are minted 1:1 by `strip` and burned 1:1 by `merge`, and `Vault.pt_supply` is
    // supposed to track that. Nothing in the program ever compares the tracked counter against the
    // real SPL mint supply, so any path that mints or burns without updating it -- a wrapper, an
    // admin helper, a future instruction -- breaks this silently. `Sigma yt_balance == pt_supply` is
    // load-bearing for the emission accounting, so a divergence here is upstream of a payout bug.
    scout_run_property!("P-0002", {
        // Vault read lives inside the property block: SCOUT:INVARIANTS permits only
        // comments between blocks, and a property block only its scout_run_property! stmt.
        let Ok(vault) = fixture.ctx.read_anchor_account::<exponent_core::state::Vault>(&fixture.vault)
        else { return }; // vault unreadable => nothing ran; not a violation
        let pt_supply = fixture.mint_supply(&fixture.mint_pt);
        let yt_supply = fixture.mint_supply(&fixture.mint_yt);
        if let (Some(pt), Some(yt)) = (pt_supply, yt_supply) {
            // `Vault.pt_supply` tracks the PT mint unconditionally: `strip` mints and increments,
            // `merge` burns and decrements, with no maturity condition on either.
            scout_check!("P-0002", "pt_supply_tracked", pt == vault.pt_supply,
                "mint_pt.supply={} != Vault.pt_supply={}", pt, vault.pt_supply);

            // PT == YT is only an invariant while the vault is ACTIVE. After maturity `merge`
            // deliberately burns PT without burning YT -- `burn_py` (vault/merge.rs:120-129) is
            // explicit: "If the maturity has passed, do not burn the YT". Asserting equality
            // unconditionally is a FALSE PROPERTY, and it fired 136 times in the first campaign
            // once `action_advance_time` was widened past the 365-day duration. Scoping it is the
            // fix; deleting it would lose the check for the window where it does hold.
            // Ground truth from the SVM, not the mirrored field -- see svm_unix_timestamp.
            //
            // "Currently active" is NOT sufficient. `merge` burns PT without burning YT once the
            // vault has expired, and that gap is permanent -- nothing ever re-mints the YT. So the
            // check must also require that the vault has NEVER been expired, otherwise any clock
            // that moves backwards re-labels a legitimate post-maturity state as active and the
            // property fires on healthy behaviour. Solana's Clock is monotonic, so on-chain the
            // two conditions are the same thing; in the harness they are not unless
            // `action_advance_time` is monotonic too (it now is). Keeping both is belt and braces:
            // a stale high-water mark can only suppress the check, never manufacture a violation.
            let now = fixture.svm_unix_timestamp().unwrap_or(fixture.current_ts);
            let maturity = vault.start_ts.saturating_add(vault.duration);
            let is_active = now >= vault.start_ts && now <= maturity;
            fixture.ts_seen = fixture.ts_seen.max(now);
            let never_expired = fixture.ts_seen <= maturity;
            if is_active && never_expired {
                scout_check!("P-0002", "pt_yt_supply_equal_while_active", pt == yt,
                    "vault active (ts={} in [{}, {}]) but mint_pt.supply={} != mint_yt.supply={}",
                    now, vault.start_ts,
                    vault.start_ts.saturating_add(vault.duration), pt, yt);
            }
        }
    });
    // SCOUT:INVARIANT:P-0002:END

    // SCOUT:INVARIANT:P-0003:BEGIN
    // Emission claims must be bounded by what the vault actually received for that stream.
    // `collect_emission` pays out against `staged` with no cross-check against the escrow
    // (collect_emission.rs:97, :115, :119). This is the general, fuzzable form of confirmed bug
    // issue-01: a tracker seeded at Number::ZERO credits `final_index * sy_balance`, which is
    // bounded by nothing the vault holds.
    //
    // Denominator: escrow balance + everything already paid out of it. Paid-out is not directly
    // observable, so the check uses (escrow + all actors' emission holdings + treasury holdings),
    // which is an OVER-estimate of what was received -- again strictly more permissive.
    // Checked PER STREAM, not just stream 0. The vault could only ever hold one emission until the
    // harness grew a second (BLIND-SPOTS.md #1), so `[0]` was the whole world; it no longer is, and
    // a defect that only affects a later stream -- the positional-shift class, or a second stream
    // whose custody is confused with the first's -- would otherwise be invisible.
    scout_run_property!("P-0003", {
        // Vault read lives inside the property block: SCOUT:INVARIANTS permits only
        // comments between blocks, and a property block only its scout_run_property! stmt.
        let Ok(vault) = fixture.ctx.read_anchor_account::<exponent_core::state::Vault>(&fixture.vault)
        else { return }; // vault unreadable => nothing ran; not a violation
        for stream in 0..vault.emissions.len().min(fixture.emission_escrows.len()) {
            let mut claimed: u128 = 0;
            let mut crediting_positions: u128 = 0;
            for i in 0..N_USERS {
                let st = fixture.position_staged_emission(&fixture.yield_position[i], stream) as u128;
                if st > 0 { crediting_positions += 1; }
                claimed += st;
            }
            let vst = fixture.position_staged_emission(&fixture.vault_yield_position, stream) as u128;
            if vst > 0 { crediting_positions += 1; }
            claimed += vst;
            // The denominator must count CLAIMS, not just realized transfers -- the exact mistake
            // the invariant-design playbook warns about, which I made first: before any
            // `collect_emission` the vault holds ZERO emission tokens while legitimately having a
            // claim on the SY program, so "escrow + payouts" alone reports a violation on every
            // healthy run. Include what the vault can still pull (`amount_claimable` on its SY
            // position), which is an over-estimate of entitlement and therefore strictly more
            // permissive.
            let mut received: u128 =
                fixture.ctx.token_balance(&fixture.emission_escrows[stream]) as u128
                + fixture.ctx.token_balance(&fixture.treasury_emission_tas[stream]) as u128
                + fixture.read_vault_sy_position(stream).1 as u128;
            for i in 0..N_USERS {
                received += fixture.ctx.token_balance(&fixture.ta_emissions[stream][i]) as u128;
            }
            // Rounding slack: each position's credit is a separate floored
            // `calc_share_value` (utils/math.rs:4-13), and the mock's own accrual floors
            // independently, so the two sides can legitimately differ by up to 1 unit PER
            // CREDITING POSITION. Without this the property fires on ±1 noise
            // (observed: 5810046 vs 5810045) and buries the real signal, which is orders of
            // magnitude (observed: 2869299094 vs 5730022). Allowing slack can only make the
            // check MORE permissive, never manufacture a violation.
            let allowed = received + crediting_positions;
            scout_check!("P-0003", "emission_claims_bounded", claimed <= allowed,
                "stream {}: staged emission across positions={} exceeds everything the vault holds \
                 or can still claim for the stream={} (+{} rounding slack) \
                 (escrow={}, treasury={}, sy-side claimable={})",
                stream, claimed, received, crediting_positions,
                fixture.ctx.token_balance(&fixture.emission_escrows[stream]),
                fixture.ctx.token_balance(&fixture.treasury_emission_tas[stream]),
                fixture.read_vault_sy_position(stream).1);
        }
    });
    // SCOUT:INVARIANT:P-0003:END

    // SCOUT:INVARIANT:P-0004:BEGIN
    // Adversary value conservation -- pattern 1, the strongest property this protocol admits.
    // `users[0]` cannot end richer than they started, valued in py units at a stated mark.
    //
    // THE GATE IS THE WHOLE PROPERTY. A naive per-actor "did the attacker profit" check is a
    // documented false-positive factory in anything with an AMM, because a profit can be an honest
    // transfer from whoever took the other side of the trade, and re-deriving the counterparty's
    // loss and calling it a bug is not a finding. So this asserts only when value creation is the
    // ONLY thing an adversary gain can be:
    //
    //   1. the SY rate is exactly its baseline           -- no yield has been earned to explain a
    //                                                       gain, and every conversion below is
    //                                                       evaluated at the mark the baseline was
    //                                                       taken at.
    //   2. `Vault.all_time_high_sy_exchange_rate` is unchanged -- the rate never went ABOVE
    //                                                       baseline either. `all_time_high` is
    //                                                       one-way (vault.rs:348), so this is the
    //                                                       only way to see an excursion that has
    //                                                       since come back.
    //   3. the market's PT and SY escrows are at their baseline -- the counterparty guard the
    //                                                       playbook insists on. Any trade or
    //                                                       liquidity event that has not been
    //                                                       exactly undone shuts the gate. When it
    //                                                       HAS been exactly undone this is the
    //                                                       sound form of the round-trip property,
    //                                                       which is why P-0006 is retired into
    //                                                       this one rather than written separately.
    //   4. `sy_rate_moved` is clear                      -- see the field comment; covers the
    //                                                       dip-and-return that (1) and (2) cannot
    //                                                       see.
    //
    // Conditions 1-3 are re-derived from on-chain state on every evaluation rather than tracked, so
    // they survive the fixture-template fallback described on the fields. Condition 4 only ever
    // adds restriction.
    //
    // Under that gate no legitimate mechanism pays the adversary anything: interest is zero at a
    // frozen rate, the market is where it started, and `strip`/`merge`/`deposit_yt`/`withdraw_yt`/
    // `collect_interest` are all exact at rate 1.0 (the mock floors both directions, and 1e12/1e12
    // divides evenly). SLACK IS ZERO deliberately -- a tolerance here would hide precisely the
    // one-unit-per-operation rounding leak this is best placed to find. If it turns out to fire on
    // rounding noise, the magnitude in the message is what says so, and the slack gets raised with
    // that measurement attached rather than guessed at up front.
    scout_run_property!("P-0004", {
        if fixture.p0004_gate_open() {
            let rate = fixture.sy_exchange_rate();
            let market_pt = fixture.ctx.token_balance(&fixture.market_escrow_pt);
            let market_sy = fixture.ctx.token_balance(&fixture.market_escrow_sy);
            let now = fixture.adversary_value_py();
            let start = fixture.baseline_adversary_py;
            // LIVENESS PROBE. `SCOUT_P0004_PROBE=1` inverts the check so it fails the instant the
            // gate opens. A campaign under the probe that reports NOTHING proves the gate never
            // opened, i.e. that a silent P-0004 says nothing about the code -- which is the failure
            // mode of every gated property and is not otherwise observable from the outside.
            // The unit test asserts the gate opens in the STARTING state; this asserts the fuzzer
            // actually reaches states where it is open. Off unless the variable is set.
            //
            // The probe fires only when the adversary has ACTUALLY TRANSACTED under the open gate.
            // Firing on a state where they still hold nothing but their starting base would prove
            // only that the gate opens before anyone moves, which is worth nothing: the question is
            // whether the fuzzer reaches states where value COULD have been created and the gate is
            // still open.
            let probe = std::env::var("SCOUT_P0004_PROBE").is_ok()
                && (fixture.ctx.token_balance(&fixture.ta_sy[0]) != 0
                    || fixture.ctx.token_balance(&fixture.ta_pt[0]) != 0
                    || fixture.ctx.token_balance(&fixture.ta_yt[0]) != 0
                    || fixture.position_yt_balance(0) != 0
                    || fixture.position_interest_staged(0) != 0
                    || fixture.ctx.token_balance(&fixture.ta_base[0])
                        != fixture.initial_base_per_user);
            scout_check!("P-0004", "adversary_cannot_gain_value", !probe && now <= start,
                "adversary(users[0]) value={} py exceeds its starting value={} py by {} \
                 at an unchanged rate={} and an unchanged market (pt={}, sy={}) \
                 [base={}, sy={}, pt={}, staged_interest={}]",
                now, start, now.saturating_sub(start), rate, market_pt, market_sy,
                fixture.ctx.token_balance(&fixture.ta_base[0]),
                fixture.ctx.token_balance(&fixture.ta_sy[0]),
                fixture.ctx.token_balance(&fixture.ta_pt[0]),
                fixture.position_interest_staged(0));
        }
    });
    // SCOUT:INVARIANT:P-0004:END

    // SCOUT:INVARIANT:P-0010:BEGIN
    // The post-maturity side of P-0002, which stops at maturity and therefore watched nothing in
    // the window where issue-05 lives (BLIND-SPOTS.md #8).
    //
    //     mint_yt.supply >= mint_pt.supply,  and the gap never shrinks
    //
    // `merge` burns PT without burning YT once the vault has expired -- `burn_py`
    // (`vault/merge.rs:120-129`) says so explicitly -- and nothing anywhere re-mints YT. So the gap
    // is one-way by construction, which makes both halves nets rather than mirrors: YT dropping
    // below PT, or the gap closing, would each mean something minted or burned outside the two
    // paths that are supposed to be the only ones.
    //
    // False-positive reasoning: the gap is read from the real SPL mints, not from vault
    // bookkeeping, so it cannot desync from a mirrored field. The high-water mark is a fixture
    // field and therefore travels with the snapshot; a template-fallback reset lowers the mark,
    // which can only suppress a finding.
    scout_run_property!("P-0010", {
        if let (Some(pt), Some(yt)) =
            (fixture.mint_supply(&fixture.mint_pt), fixture.mint_supply(&fixture.mint_yt))
        {
            scout_check!("P-0010", "yt_never_below_pt", yt >= pt,
                "mint_yt.supply={} is BELOW mint_pt.supply={} -- YT was burned without PT, which \
                 no path is supposed to do", yt, pt);
            let gap = yt.saturating_sub(pt);
            scout_check!("P-0010", "yt_pt_gap_never_shrinks", gap >= fixture.yt_pt_gap_seen,
                "the YT-over-PT gap fell from {} to {} (mint_pt.supply={}, mint_yt.supply={}); \
                 nothing re-mints YT, so this means PT was minted after maturity or YT was burned \
                 outside merge", fixture.yt_pt_gap_seen, gap, pt, yt);
            fixture.yt_pt_gap_seen = gap.max(fixture.yt_pt_gap_seen);
        }
    });
    // SCOUT:INVARIANT:P-0010:END

    // SCOUT:INVARIANT:P-0011:BEGIN
    // Emission indexes in VAULT state never move backwards.
    //
    // This is the "invariant the code states in one place and enforces nowhere" pattern in its
    // purest form. `can_collect_emission_lambo` justifies having no constraint beyond expiry with
    // the comment *"Since emissions are non-decreasing, that is the only constraint"*
    // (`vault.rs:265`). Nothing validates that. `update_from_sy_state` copies whatever the SY
    // program returns straight into vault state, unconditionally (`vault.rs:356-363`):
    //
    //     self.emissions[index].last_seen_index = *x;
    //     if self.is_active(now) { self.emissions[index].final_index = *x; }
    //
    // and the SY program is a THIRD PARTY. `calc_emission_surpluses` then credits the treasury
    // `calc_share_value(emission.last_seen_index, *index, total_sy_in_escrow)` (`vault.rs:286`), so
    // a rewound `last_seen_index` means the same accrual is swept into the treasury a second time
    // when the index climbs back -- structurally the same defect as issue-05, in the emission
    // channel instead of the rate channel.
    //
    // Why this is a NET and not a restatement of the comment: the comment is an assumption about
    // an external program, and the property is over Exponent's OWN state after it has ingested
    // that program's output. It fires on the ingestion, which is the thing Exponent controls.
    //
    // False-positive reasoning: both marks start at 0 and only ever rise. A snapshot restore
    // lowers the mark, which suppresses rather than manufactures. Streams the vault has not
    // registered are skipped.
    scout_run_property!("P-0011", {
        // Vault read lives inside the property block: SCOUT:INVARIANTS permits only
        // comments between blocks, and a property block only its scout_run_property! stmt.
        let Ok(vault) = fixture.ctx.read_anchor_account::<exponent_core::state::Vault>(&fixture.vault)
        else { return }; // vault unreadable => nothing ran; not a violation
        for (i, emission) in vault.emissions.iter().enumerate() {
            if i >= fixture.emission_index_seen.len() { break; }
            let last_seen = ExponentCoreFixture::number_u128(&emission.last_seen_index);
            let final_idx = ExponentCoreFixture::number_u128(&emission.final_index);
            scout_check!("P-0011", "last_seen_index_never_decreases",
                last_seen >= fixture.emission_index_seen[i],
                "stream {}: Vault.emissions[{}].last_seen_index fell from {} to {} -- the vault \
                 ingested a decreasing index from the SY program, and calc_emission_surpluses \
                 (vault.rs:286) will re-credit the treasury for the recovery",
                i, i, fixture.emission_index_seen[i], last_seen);
            scout_check!("P-0011", "final_index_never_decreases",
                final_idx >= fixture.emission_final_seen[i],
                "stream {}: Vault.emissions[{}].final_index fell from {} to {} -- positions are \
                 credited against this, so a rewind changes what every holder is owed",
                i, i, fixture.emission_final_seen[i], final_idx);
            fixture.emission_index_seen[i] = last_seen.max(fixture.emission_index_seen[i]);
            fixture.emission_final_seen[i] = final_idx.max(fixture.emission_final_seen[i]);
        }
    });
    // SCOUT:INVARIANT:P-0011:END

    // SCOUT:INVARIANT:P-0009:BEGIN
    // PT is fully backed: the SY set aside for PT is worth at least the PT supply.
    //
    //     sy_for_pt * last_seen_sy_exchange_rate  >=  pt_supply        (both in py)
    //
    // WHY THIS IS A NET AND P-0001 IS NOT. P-0001 asserts the vault's own stated solvency
    // condition, `escrow >= sy_for_pt + treasury + uncollected` (vault.rs:110-113). That condition
    // is structurally INCAPABLE of seeing a PT shortfall, because `sy_for_pt` is not "what PT is
    // owed" -- it is recomputed as `min(pt_supply / rate, active_sy)` (vault.rs:569-581) every time
    // the vault refreshes. When the pool can no longer back PT, the `min` silently takes the
    // `active_sy` branch and solvency continues to hold with ZERO slack. Confirmed bug issue-05
    // does exactly that: PT ends up half-backed while P-0001 reports the vault healthy in BOTH the
    // healthy and the damaged world -- measured, not assumed.
    //
    // This property states the thing `sy_for_pt` was supposed to mean, which is what the `min`
    // quietly gives up on. It fires precisely when the fallback branch is taken.
    //
    // False-positive reasoning: `sy_backing_for_pt` FLOORS the division (vault.rs:573-574), so a
    // healthy vault can sit just under face. The comparison is in py with one whole SY unit of
    // slack, which is strictly more permissive than the truth and cannot manufacture a violation.
    // Skipped when `pt_supply == 0` (nothing to back) or the rate is unreadable.
    //
    // GATED ON NOT BEING IN EMERGENCY MODE, and the first version was WRONG without it. PT's face
    // value is denominated in py and fixed at mint; the SY backing it is worth `sy * rate`. If the
    // SY genuinely DEPRECIATES, the same SY covers less py and PT is under-backed for an honest
    // reason -- the code says so itself (`vault.rs:255-257`, "since SY can depreciate"). Ungated,
    // this fired 30+ times in a 7-minute campaign on exactly that: `sy_for_pt=10000074 at
    // rate=2000000000` is rate 0.002, i.e. the fuzzer had driven the SY price down 500x, and the
    // shortfall was arithmetic rather than a defect.
    //
    // `last_seen == all_time_high` is precisely "not in emergency mode" (`vault.rs:120-122`), i.e.
    // the SY has never been worth more than it is now. Under that condition there is no honest
    // source of a shortfall, and it is the condition issue-05's damaged world satisfies: both of
    // its worlds end at last_seen == ath == 3.0, the healthy one fully backed and the damaged one
    // at half. So the gate keeps the finding and drops the noise.
    scout_run_property!("P-0009", {
        // Vault read lives inside the property block: SCOUT:INVARIANTS permits only
        // comments between blocks, and a property block only its scout_run_property! stmt.
        let Ok(vault) = fixture.ctx.read_anchor_account::<exponent_core::state::Vault>(&fixture.vault)
        else { return }; // vault unreadable => nothing ran; not a violation
        let rate = ExponentCoreFixture::number_u128(&vault.last_seen_sy_exchange_rate);
        let ath = ExponentCoreFixture::number_u128(&vault.all_time_high_sy_exchange_rate);
        if rate != 0 && rate == ath && vault.pt_supply != 0 {
            let backing_py = vault.sy_for_pt as u128 * rate / NUMBER_ONE;
            let owed_py = vault.pt_supply as u128;
            let slack = rate / NUMBER_ONE + 1; // one SY unit, expressed in py
            scout_check!("P-0009", "pt_is_fully_backed", backing_py + slack >= owed_py,
                "PT is under-backed: sy_for_pt={} at rate={} is worth {} py but pt_supply={} py \
                 (short by {}, slack {}) [escrow={}, treasury={}, uncollected={}, ath={}]",
                vault.sy_for_pt, rate, backing_py, owed_py,
                owed_py.saturating_sub(backing_py), slack,
                vault.total_sy_in_escrow, vault.treasury_sy, vault.uncollected_sy,
                ExponentCoreFixture::number_u128(&vault.all_time_high_sy_exchange_rate));
        }
    });
    // SCOUT:INVARIANT:P-0009:END

    // SCOUT:INVARIANT:P-0005:BEGIN
    // LP accounting: `lp_escrow_amount` and the per-position `lp_balance` are incremented and
    // decremented by the same amount in the same two instructions (deposit_lp.rs:75/:111,
    // withdraw_lp.rs:77/:114) and are NEVER compared. `lp_escrow_amount` is the divisor for every
    // emission index update and `lp_balance` is the multiplier for every payout, so a divergence is
    // directly monetary.
    scout_run_property!("P-0005", {
        if let Ok(market) = fixture.ctx.read_anchor_account::<exponent_core::state::MarketTwo>(&fixture.market) {
            let mut sum: u128 = 0;
            for i in 0..N_USERS {
                sum += ExponentCoreFixture::position_lp_balance_of(&fixture.ctx, &fixture.lp_position[i]) as u128;
            }
            scout_check!("P-0005", "lp_escrow_equals_sum_of_positions",
                sum == market.lp_escrow_amount as u128,
                "sum(LpPosition.lp_balance)={} != MarketTwo.lp_escrow_amount={}",
                sum, market.lp_escrow_amount);
        }
    });
    // SCOUT:INVARIANT:P-0005:END

    // SCOUT:INVARIANT:P-0007:BEGIN
    // `Vault.all_time_high_sy_exchange_rate` never decreases. It is written in exactly one place,
    // `cur_rate.max(self.all_time_high_sy_exchange_rate)` (vault.rs:348), so this is not a mirror
    // of that line -- it is a net over every OTHER writer of the vault account: the admin paths,
    // `modify_vault_setting`, and any future whole-struct assignment. It is also load-bearing:
    // `calc_earned_sy` prices all interest against it, so a value that moves backwards pays yield
    // twice.
    //
    // RE-ENABLED. This was disabled after firing 100+ times on healthy runs, because the
    // high-water mark lived in a `thread_local!` that SURVIVED the fuzzer's snapshot restore while
    // the vault's ATH reset to 1.0 with the rest of the state. The mark now lives on the fixture,
    // which crucible stores and restores together with the state delta -- see the long note on the
    // fields. The check is therefore comparing two values from the same point in history, which is
    // the thing it was never doing before.
    scout_run_property!("P-0007", {
        let ath = fixture.vault_ath();
        if ath != 0 {
            scout_check!("P-0007", "ath_rate_never_decreases", ath >= fixture.ath_seen,
                "Vault.all_time_high_sy_exchange_rate fell from {} to {} \
                 (baseline at setup was {})",
                fixture.ath_seen, ath, fixture.baseline_ath);
            fixture.ath_seen = ath.max(fixture.ath_seen);
        }
    });
    // SCOUT:INVARIANT:P-0007:END

    // SCOUT:INVARIANT:P-0008:BEGIN
    // Emission tracker floor. `EmissionInfo.initial_index` exists precisely to be the first
    // claimable index for a position created after the emission was added (vault.rs:503-504), is
    // written at :528, and is read NOWHERE. A tracker sitting below it is exactly the corrupt state
    // behind confirmed bug issue-01, stated as an invariant over persisted state rather than over
    // one code path.
    scout_run_property!("P-0008", {
        // Vault read lives inside the property block: SCOUT:INVARIANTS permits only
        // comments between blocks, and a property block only its scout_run_property! stmt.
        let Ok(vault) = fixture.ctx.read_anchor_account::<exponent_core::state::Vault>(&fixture.vault)
        else { return }; // vault unreadable => nothing ran; not a violation
        for (idx, emission) in vault.emissions.iter().enumerate() {
            let floor = ExponentCoreFixture::number_u128(&emission.initial_index);
            if floor == 0 { continue; } // nothing to violate
            for i in 0..N_USERS {
                let (seen, staged) =
                    fixture.position_tracker(&fixture.yield_position[i], idx);
                // Only meaningful once the position actually has a tracker AND has been credited.
                if staged == 0 { continue; }
                scout_check!("P-0008", "tracker_at_or_above_initial_index", seen >= floor,
                    "position {} tracker[{}].last_seen_index={} is below \
                     EmissionInfo.initial_index={} while staged={}",
                    i, idx, seen, floor, staged);
            }
        }
    });
    // SCOUT:INVARIANT:P-0008:END
    // SCOUT:INVARIANTS:END
}
