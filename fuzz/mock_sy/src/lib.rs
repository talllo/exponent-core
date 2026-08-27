//! # mock_sy
//!
//! A mock Standardized Yield (SY) Solana program that implements the CPI
//! interface Exponent Core calls into (see
//! `exponent-core/programs/exponent_core/src/utils/sy_cpi.rs`), plus a handful
//! of test-control instructions that let a fuzz harness move the exchange rate
//! and the emission indexes between two user actions.
//!
//! ## Instruction encoding
//!
//! A **bare 1-byte discriminator** followed by borsh-encoded args. There is no
//! 8-byte Anchor discriminator anywhere in this interface, which is why this is
//! a plain native program and not an Anchor one.
//!
//! ## Economics
//!
//! `exchange_rate` is **underlying (base) tokens per SY**, scaled by 1e12.
//! Exponent computes `py = sy * rate` (`sy_to_py`) and `sy = py / rate`
//! (`py_to_sy`), so a rate that *increases* means SY appreciated.
//!
//! * `mint_sy(base)`  -> `sy_out   = floor(base * 1e12 / rate)`
//! * `redeem_sy(sy)`  -> `base_out = floor(sy * rate / 1e12)`
//!
//! Both directions floor, so the round trip `mint_sy(b)` then
//! `redeem_sy(sy_out)` at an unchanged rate can never return more than `b`.

pub mod error;
pub mod number;
pub mod state;
pub mod token;
pub mod wire;

use error::MockSyError;
use number::Number;
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    msg,
    instruction::Instruction,
    program::{invoke, invoke_signed, set_return_data},
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    sysvar::Sysvar,
};
use state::{
    load, store, SyGlobal, SyPosition, AUTHORITY_SEED, GLOBAL_ACCOUNT_SIZE, GLOBAL_SEED,
    GLOBAL_TAG, MAX_EMISSIONS, POSITION_ACCOUNT_SIZE, POSITION_SEED, POSITION_TAG,
};
use wire::{Amount, MintSyReturnData, RedeemSyReturnData};

solana_program::declare_id!("5oPn327MeFdp9GVik2VwYNVyMN6ZL88rx8yKo54ViQWk");

// ---------------------------------------------------------------------------
// Discriminators
// ---------------------------------------------------------------------------

pub mod ix {
    /// `mint_sy(amount_base: u64)` -> `MintSyReturnData`
    pub const MINT_SY: u8 = 1;
    /// `redeem_sy(amount_sy: u64)` -> `RedeemSyReturnData`
    pub const REDEEM_SY: u8 = 2;
    /// `init_sy_personal_account()`
    pub const INIT_SY_PERSONAL_ACCOUNT: u8 = 3;
    /// `deposit_sy(amount: u64)` -> `SyState`
    pub const DEPOSIT_SY: u8 = 5;
    /// `withdraw_sy(amount: u64)` -> `SyState`
    pub const WITHDRAW_SY: u8 = 6;
    /// `get_sy_state()` -> `SyState`
    pub const GET_SY_STATE: u8 = 7;
    /// `claim_emission(amount: Amount)`
    pub const CLAIM_EMISSION: u8 = 8;
    /// `get_position()` -> `PositionState`
    pub const GET_POSITION: u8 = 10;

    // -- test control ------------------------------------------------------
    /// `init_global(initial_exchange_rate: Number)`
    pub const INIT_GLOBAL: u8 = 199;
    /// `set_exchange_rate(rate: Number)`
    pub const SET_EXCHANGE_RATE: u8 = 200;
    /// `set_emission_index(index: u32, value: Number)`
    pub const SET_EMISSION_INDEX: u8 = 201;
    /// `add_emission_index(initial: Number, mint: Option<Pubkey> as raw tail)`
    pub const ADD_EMISSION_INDEX: u8 = 202;
    /// `fund_emission(index: u32, amount: u64)`
    pub const FUND_EMISSION: u8 = 203;
    /// `arm_reentrancy([Pubkey])` -- with a 32-byte tail, `get_sy_state` invokes
    /// that program before returning; with an empty tail, disarmed.
    pub const ARM_REENTRANCY: u8 = 205;
}

#[cfg(all(not(feature = "no-entrypoint"), target_os = "solana"))]
solana_program::entrypoint!(process_instruction);

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    let (discriminator, args) = instruction_data
        .split_first()
        .ok_or(MockSyError::InvalidInstructionData)?;

    match *discriminator {
        ix::MINT_SY => mint_sy(program_id, accounts, args),
        ix::REDEEM_SY => redeem_sy(program_id, accounts, args),
        ix::INIT_SY_PERSONAL_ACCOUNT => init_sy_personal_account(program_id, accounts),
        ix::DEPOSIT_SY => deposit_sy(program_id, accounts, args),
        ix::WITHDRAW_SY => withdraw_sy(program_id, accounts, args),
        ix::GET_SY_STATE => get_sy_state(program_id, accounts),
        ix::CLAIM_EMISSION => claim_emission(program_id, accounts, args),
        ix::GET_POSITION => get_position(program_id, accounts),

        ix::INIT_GLOBAL => init_global(program_id, accounts, args),
        ix::SET_EXCHANGE_RATE => set_exchange_rate(program_id, accounts, args),
        ix::SET_EMISSION_INDEX => set_emission_index(program_id, accounts, args),
        ix::ADD_EMISSION_INDEX => add_emission_index(program_id, accounts, args),
        ix::FUND_EMISSION => fund_emission(program_id, accounts, args),
        ix::ARM_REENTRANCY => arm_reentrancy(program_id, accounts, args),

        other => {
            msg!("mock_sy: unknown discriminator {}", other);
            Err(MockSyError::UnknownDiscriminator.into())
        }
    }
}

// ---------------------------------------------------------------------------
// Arg decoding helpers
// ---------------------------------------------------------------------------

fn read_u64(args: &[u8]) -> Result<u64, ProgramError> {
    let bytes: [u8; 8] = args
        .get(..8)
        .ok_or(MockSyError::InvalidInstructionData)?
        .try_into()
        .map_err(|_| MockSyError::InvalidInstructionData)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_u32(args: &[u8]) -> Result<u32, ProgramError> {
    let bytes: [u8; 4] = args
        .get(..4)
        .ok_or(MockSyError::InvalidInstructionData)?
        .try_into()
        .map_err(|_| MockSyError::InvalidInstructionData)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_number(args: &[u8]) -> Result<Number, ProgramError> {
    let raw = args
        .get(..Number::SIZEOF)
        .ok_or(MockSyError::InvalidInstructionData)?;
    let mut words = [0u64; 4];
    for (i, w) in words.iter_mut().enumerate() {
        let chunk: [u8; 8] = raw[i * 8..i * 8 + 8]
            .try_into()
            .map_err(|_| MockSyError::InvalidInstructionData)?;
        *w = u64::from_le_bytes(chunk);
    }
    Ok(Number(words))
}

fn read_pubkey(args: &[u8]) -> Result<Pubkey, ProgramError> {
    let raw: [u8; 32] = args
        .get(..32)
        .ok_or(MockSyError::InvalidInstructionData)?
        .try_into()
        .map_err(|_| MockSyError::InvalidInstructionData)?;
    Ok(Pubkey::new_from_array(raw))
}

fn read_amount(args: &[u8]) -> Result<Amount, ProgramError> {
    match args.first() {
        Some(0) => Ok(Amount::All),
        Some(1) => Ok(Amount::Some(read_u64(&args[1..])?)),
        _ => Err(MockSyError::InvalidInstructionData.into()),
    }
}

fn set_return<T: borsh::BorshSerialize>(value: &T) -> Result<(), ProgramError> {
    let bytes = borsh::to_vec(value).map_err(|_| MockSyError::SerializationFailed)?;
    set_return_data(&bytes);
    Ok(())
}

// ---------------------------------------------------------------------------
// PDA helpers
// ---------------------------------------------------------------------------

fn check_pda(key: &Pubkey, seeds: &[&[u8]], program_id: &Pubkey) -> Result<u8, ProgramError> {
    let (expected, bump) = Pubkey::find_program_address(seeds, program_id);
    if expected != *key {
        msg!("mock_sy: PDA mismatch, expected {}", expected);
        return Err(MockSyError::InvalidPda.into());
    }
    Ok(bump)
}

fn load_global(
    account: &AccountInfo,
    program_id: &Pubkey,
) -> Result<(SyGlobal, u8), ProgramError> {
    let bump = check_pda(account.key, &[GLOBAL_SEED], program_id)?;
    if account.owner != program_id {
        return Err(MockSyError::WrongAccountOwner.into());
    }
    Ok((load::<SyGlobal>(account, GLOBAL_TAG)?, bump))
}

fn load_position(
    account: &AccountInfo,
    program_id: &Pubkey,
) -> Result<SyPosition, ProgramError> {
    if account.owner != program_id {
        return Err(MockSyError::WrongAccountOwner.into());
    }
    let position: SyPosition = load(account, POSITION_TAG)?;
    check_pda(
        account.key,
        &[POSITION_SEED, position.owner.as_ref()],
        program_id,
    )?;
    Ok(position)
}

fn authority_seeds<'a>(bump: &'a [u8; 1]) -> [&'a [u8]; 2] {
    [AUTHORITY_SEED, bump.as_slice()]
}

fn global_seeds<'a>(bump: &'a [u8; 1]) -> [&'a [u8]; 2] {
    [GLOBAL_SEED, bump.as_slice()]
}

// ---------------------------------------------------------------------------
// [1] mint_sy
// accounts: sy_global(mut), base_src(mut), base_custody(mut), sy_mint(mut),
//           sy_dst(mut), user_authority(signer), token_program
// ---------------------------------------------------------------------------

fn mint_sy(program_id: &Pubkey, accounts: &[AccountInfo], args: &[u8]) -> ProgramResult {
    let amount_base = read_u64(args)?;

    let iter = &mut accounts.iter();
    let sy_global = next_account_info(iter)?;
    let base_src = next_account_info(iter)?;
    let base_custody = next_account_info(iter)?;
    let sy_mint = next_account_info(iter)?;
    let sy_dst = next_account_info(iter)?;
    let user_authority = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    if !user_authority.is_signer {
        return Err(MockSyError::MissingSigner.into());
    }

    let (global, global_bump) = load_global(sy_global, program_id)?;
    let rate = global
        .exchange_rate
        .checked_raw_u128()
        .ok_or(MockSyError::NumberTooLarge)?;
    if rate == 0 {
        return Err(MockSyError::ZeroExchangeRate.into());
    }

    // sy_out = floor(base * 1e12 / rate_raw)
    let sy_out = (amount_base as u128)
        .checked_mul(Number::DENOM)
        .ok_or(MockSyError::MathOverflow)?
        / rate;
    let sy_out = u64::try_from(sy_out).map_err(|_| MockSyError::MathOverflow)?;

    // Pull the base tokens in ...
    token::transfer(
        token_program,
        base_src,
        base_custody,
        user_authority,
        amount_base,
        None,
    )?;
    // ... and mint the SY out. The SY mint authority is the sy_global PDA
    // because `sy_authority_pda` is not part of this instruction's accounts.
    let bump = [global_bump];
    token::mint_to(
        token_program,
        sy_mint,
        sy_dst,
        sy_global,
        sy_out,
        Some(&global_seeds(&bump)),
    )?;

    set_return(&MintSyReturnData {
        sy_out_amount: sy_out,
        exchange_rate: global.exchange_rate,
    })
}

// ---------------------------------------------------------------------------
// [2] redeem_sy
// accounts: sy_global(mut), sy_src(mut), sy_mint(mut), base_custody(mut),
//           base_dst(mut), sy_authority_pda, user_authority(signer),
//           token_program
// ---------------------------------------------------------------------------

fn redeem_sy(program_id: &Pubkey, accounts: &[AccountInfo], args: &[u8]) -> ProgramResult {
    let amount_sy = read_u64(args)?;

    let iter = &mut accounts.iter();
    let sy_global = next_account_info(iter)?;
    let sy_src = next_account_info(iter)?;
    let sy_mint = next_account_info(iter)?;
    let base_custody = next_account_info(iter)?;
    let base_dst = next_account_info(iter)?;
    let sy_authority = next_account_info(iter)?;
    let user_authority = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    if !user_authority.is_signer {
        return Err(MockSyError::MissingSigner.into());
    }

    let (global, _) = load_global(sy_global, program_id)?;
    let authority_bump = check_pda(sy_authority.key, &[AUTHORITY_SEED], program_id)?;

    let rate = global
        .exchange_rate
        .checked_raw_u128()
        .ok_or(MockSyError::NumberTooLarge)?;

    // base_out = floor(sy * rate_raw / 1e12)
    let base_out = (amount_sy as u128)
        .checked_mul(rate)
        .ok_or(MockSyError::MathOverflow)?
        / Number::DENOM;
    let base_out = u64::try_from(base_out).map_err(|_| MockSyError::MathOverflow)?;

    token::burn(
        token_program,
        sy_src,
        sy_mint,
        user_authority,
        amount_sy,
        None,
    )?;

    let bump = [authority_bump];
    token::transfer(
        token_program,
        base_custody,
        base_dst,
        sy_authority,
        base_out,
        Some(&authority_seeds(&bump)),
    )?;

    set_return(&RedeemSyReturnData {
        base_out_amount: base_out,
        exchange_rate: global.exchange_rate,
    })
}

// ---------------------------------------------------------------------------
// [3] init_sy_personal_account
// accounts: payer(signer,mut), sy_position(mut), owner, system_program
// ---------------------------------------------------------------------------

fn init_sy_personal_account(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let sy_position = next_account_info(iter)?;
    let owner = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    if !payer.is_signer {
        return Err(MockSyError::MissingSigner.into());
    }

    let bump = check_pda(
        sy_position.key,
        &[POSITION_SEED, owner.key.as_ref()],
        program_id,
    )?;

    // Idempotent: Exponent may call this on an account that already exists.
    if sy_position.owner == program_id && sy_position.data_len() > 0 {
        let already = sy_position.try_borrow_data()?[0] == POSITION_TAG;
        if already {
            msg!("mock_sy: sy_position already initialized, no-op");
            return Ok(());
        }
    }

    let rent = Rent::get()?;
    let lamports = rent.minimum_balance(POSITION_ACCOUNT_SIZE);
    let seed_bump = [bump];
    let seeds: [&[u8]; 3] = [POSITION_SEED, owner.key.as_ref(), seed_bump.as_slice()];

    invoke_signed(
        &solana_system_interface::instruction::create_account(
            payer.key,
            sy_position.key,
            lamports,
            POSITION_ACCOUNT_SIZE as u64,
            program_id,
        ),
        &[payer.clone(), sy_position.clone(), system_program.clone()],
        &[&seeds],
    )?;

    store(
        sy_position,
        POSITION_TAG,
        &SyPosition {
            owner: *owner.key,
            sy_balance: 0,
            emissions: Vec::new(),
        },
    )
}

// ---------------------------------------------------------------------------
// [5] deposit_sy
// accounts: sy_global(mut), sy_position(mut), sy_src(mut), sy_custody(mut),
//           src_authority(signer), token_program
// ---------------------------------------------------------------------------

fn deposit_sy(program_id: &Pubkey, accounts: &[AccountInfo], args: &[u8]) -> ProgramResult {
    let amount = read_u64(args)?;

    let iter = &mut accounts.iter();
    let sy_global = next_account_info(iter)?;
    let sy_position = next_account_info(iter)?;
    let sy_src = next_account_info(iter)?;
    let sy_custody = next_account_info(iter)?;
    let src_authority = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    if !src_authority.is_signer {
        return Err(MockSyError::MissingSigner.into());
    }

    let (global, _) = load_global(sy_global, program_id)?;
    let mut position = load_position(sy_position, program_id)?;

    // Accrue on the *pre-deposit* balance first.
    position.sync_and_accrue(&global)?;

    token::transfer(
        token_program,
        sy_src,
        sy_custody,
        src_authority,
        amount,
        None,
    )?;

    position.sy_balance = position
        .sy_balance
        .checked_add(amount)
        .ok_or(MockSyError::MathOverflow)?;
    store(sy_position, POSITION_TAG, &position)?;

    set_return(&global.to_sy_state())
}

// ---------------------------------------------------------------------------
// [6] withdraw_sy
// accounts: sy_global(mut), sy_position(mut), sy_custody(mut), sy_dst(mut),
//           sy_authority_pda, token_program
// ---------------------------------------------------------------------------

fn withdraw_sy(program_id: &Pubkey, accounts: &[AccountInfo], args: &[u8]) -> ProgramResult {
    let amount = read_u64(args)?;

    let iter = &mut accounts.iter();
    let sy_global = next_account_info(iter)?;
    let sy_position = next_account_info(iter)?;
    let sy_custody = next_account_info(iter)?;
    let sy_dst = next_account_info(iter)?;
    let sy_authority = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    let (global, _) = load_global(sy_global, program_id)?;
    let mut position = load_position(sy_position, program_id)?;
    let authority_bump = check_pda(sy_authority.key, &[AUTHORITY_SEED], program_id)?;

    position.sync_and_accrue(&global)?;

    position.sy_balance = position
        .sy_balance
        .checked_sub(amount)
        .ok_or(MockSyError::InsufficientSyBalance)?;

    let bump = [authority_bump];
    token::transfer(
        token_program,
        sy_custody,
        sy_dst,
        sy_authority,
        amount,
        Some(&authority_seeds(&bump)),
    )?;

    store(sy_position, POSITION_TAG, &position)?;

    set_return(&global.to_sy_state())
}

// ---------------------------------------------------------------------------
// [7] get_sy_state
// accounts: sy_global
// ---------------------------------------------------------------------------

fn get_sy_state(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let iter = &mut accounts.iter();
    let sy_global = next_account_info(iter)?;
    let (global, _) = load_global(sy_global, program_id)?;

    // TEST CONTROL: attempt to call back into the caller, from inside the CPI it
    // is currently making. Exponent reaches here from the middle of
    // `update_vault_yield` — the vault is deserialized, the handler's mutations
    // are not yet written back — which is the window a reentrancy attack would
    // need. The invoke carries no accounts on purpose: the Solana runtime
    // decides whether reentrancy is permitted *before* the callee runs, so the
    // instruction never has to be a valid one to get the answer.
    if let Some(target) = global.reenter_target {
        // The callee's own program account must be present in `account_infos` for a CPI, so the
        // caller has to hand it to us: Exponent forwards it via an extra ALT slot on
        // `cpi_accounts.get_sy_state`. Without it the invoke fails with "Unknown program", which
        // looks like a refusal but is only a missing account -- and would answer the wrong
        // question.
        let target_info = accounts
            .iter()
            .find(|a| a.key == &target)
            .ok_or(MockSyError::InvalidInstructionData)?;
        msg!("mock_sy: attempting reentrancy into {}", target);
        invoke(
            &Instruction { program_id: target, accounts: vec![], data: vec![0u8] },
            &[target_info.clone()],
        )?;
    }

    set_return(&global.to_sy_state())
}

// ---------------------------------------------------------------------------
// [205] arm_reentrancy  (test control)
// accounts: sy_global(mut)
// args: [Pubkey] — 32 bytes arms, empty disarms
// ---------------------------------------------------------------------------

fn arm_reentrancy(program_id: &Pubkey, accounts: &[AccountInfo], args: &[u8]) -> ProgramResult {
    let iter = &mut accounts.iter();
    let sy_global = next_account_info(iter)?;
    let (mut global, _) = load_global(sy_global, program_id)?;
    global.reenter_target = if args.len() >= 32 {
        Some(Pubkey::new_from_array(args[..32].try_into().unwrap()))
    } else {
        None
    };
    store(sy_global, GLOBAL_TAG, &global)
}

// ---------------------------------------------------------------------------
// [8] claim_emission
// accounts: sy_global(mut), sy_position(mut), emission_custody(mut),
//           emission_dst(mut), sy_authority_pda, token_program
//
// The emission stream is identified by the *mint of `emission_custody`*, which
// must have been registered with `add_emission_index` [202].
// ---------------------------------------------------------------------------

fn claim_emission(program_id: &Pubkey, accounts: &[AccountInfo], args: &[u8]) -> ProgramResult {
    let requested = read_amount(args)?;

    let iter = &mut accounts.iter();
    let sy_global = next_account_info(iter)?;
    let sy_position = next_account_info(iter)?;
    let emission_custody = next_account_info(iter)?;
    let emission_dst = next_account_info(iter)?;
    let sy_authority = next_account_info(iter)?;
    let token_program = next_account_info(iter)?;

    let (global, _) = load_global(sy_global, program_id)?;
    let mut position = load_position(sy_position, program_id)?;
    let authority_bump = check_pda(sy_authority.key, &[AUTHORITY_SEED], program_id)?;

    position.sync_and_accrue(&global)?;

    let mint = token::token_account_mint(emission_custody)?;
    let index = global
        .emission_mints
        .iter()
        .position(|m| *m == mint)
        .ok_or(MockSyError::UnknownEmissionMint)?;

    let claimable = position
        .emissions
        .get(index)
        .ok_or(MockSyError::EmissionIndexOutOfRange)?
        .amount_claimable;

    let amount = match requested {
        Amount::All => claimable,
        Amount::Some(x) => {
            if x > claimable {
                return Err(MockSyError::InsufficientClaimable.into());
            }
            x
        }
    };

    let bump = [authority_bump];
    token::transfer(
        token_program,
        emission_custody,
        emission_dst,
        sy_authority,
        amount,
        Some(&authority_seeds(&bump)),
    )?;

    position.emissions[index].amount_claimable = claimable - amount;
    store(sy_position, POSITION_TAG, &position)
}

// ---------------------------------------------------------------------------
// [10] get_position
// accounts: sy_position
// ---------------------------------------------------------------------------

fn get_position(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let iter = &mut accounts.iter();
    let sy_position = next_account_info(iter)?;
    let position = load_position(sy_position, program_id)?;
    set_return(&position.to_position_state())
}

// ---------------------------------------------------------------------------
// [199] init_global  (test control)
// accounts: payer(signer,mut), sy_global(mut), system_program
// args: Number initial_exchange_rate
// ---------------------------------------------------------------------------

fn init_global(program_id: &Pubkey, accounts: &[AccountInfo], args: &[u8]) -> ProgramResult {
    let initial_rate = read_number(args)?;

    let iter = &mut accounts.iter();
    let payer = next_account_info(iter)?;
    let sy_global = next_account_info(iter)?;
    let system_program = next_account_info(iter)?;

    if !payer.is_signer {
        return Err(MockSyError::MissingSigner.into());
    }
    if !initial_rate.fits_u128() {
        return Err(MockSyError::NumberTooLarge.into());
    }

    let bump = check_pda(sy_global.key, &[GLOBAL_SEED], program_id)?;
    if sy_global.owner == program_id
        && sy_global.data_len() > 0
        && sy_global.try_borrow_data()?[0] == GLOBAL_TAG
    {
        return Err(MockSyError::AccountAlreadyInitialized.into());
    }

    let rent = Rent::get()?;
    let seed_bump = [bump];
    let seeds: [&[u8]; 2] = [GLOBAL_SEED, seed_bump.as_slice()];

    invoke_signed(
        &solana_system_interface::instruction::create_account(
            payer.key,
            sy_global.key,
            rent.minimum_balance(GLOBAL_ACCOUNT_SIZE),
            GLOBAL_ACCOUNT_SIZE as u64,
            program_id,
        ),
        &[payer.clone(), sy_global.clone(), system_program.clone()],
        &[&seeds],
    )?;

    store(
        sy_global,
        GLOBAL_TAG,
        &SyGlobal {
            exchange_rate: initial_rate,
            emission_mints: Vec::new(),
            emission_indexes: Vec::new(),
            reenter_target: None,
        },
    )
}

// ---------------------------------------------------------------------------
// [200] set_exchange_rate  (test control)
// accounts: sy_global(mut)   args: Number
//
// Absolute assignment: the rate is allowed to DECREASE. Exponent has an
// explicit emergency mode keyed on
// `all_time_high_sy_exchange_rate > last_seen_sy_exchange_rate`, so a falling
// rate is a real, in-scope state that the fuzzer must be able to reach.
// ---------------------------------------------------------------------------

fn set_exchange_rate(program_id: &Pubkey, accounts: &[AccountInfo], args: &[u8]) -> ProgramResult {
    let rate = read_number(args)?;
    if !rate.fits_u128() {
        return Err(MockSyError::NumberTooLarge.into());
    }

    let iter = &mut accounts.iter();
    let sy_global = next_account_info(iter)?;

    let (mut global, _) = load_global(sy_global, program_id)?;
    global.exchange_rate = rate;
    store(sy_global, GLOBAL_TAG, &global)
}

// ---------------------------------------------------------------------------
// [201] set_emission_index  (test control)
// accounts: sy_global(mut)   args: u32 index, Number value
// ---------------------------------------------------------------------------

fn set_emission_index(program_id: &Pubkey, accounts: &[AccountInfo], args: &[u8]) -> ProgramResult {
    let index = read_u32(args)? as usize;
    let value = read_number(&args[4..])?;
    if !value.fits_u128() {
        return Err(MockSyError::NumberTooLarge.into());
    }

    let iter = &mut accounts.iter();
    let sy_global = next_account_info(iter)?;

    let (mut global, _) = load_global(sy_global, program_id)?;
    let slot = global
        .emission_indexes
        .get_mut(index)
        .ok_or(MockSyError::EmissionIndexOutOfRange)?;
    *slot = value;
    store(sy_global, GLOBAL_TAG, &global)
}

// ---------------------------------------------------------------------------
// [202] add_emission_index  (test control)
// accounts: sy_global(mut)
// args: Number initial [, Pubkey mint]
//
// The optional 32-byte tail registers the emission token's mint, which is what
// `claim_emission` uses to resolve which stream is being claimed. Omitting it
// leaves the mint at the default pubkey (the stream still accrues, it just
// cannot be claimed).
// ---------------------------------------------------------------------------

fn add_emission_index(program_id: &Pubkey, accounts: &[AccountInfo], args: &[u8]) -> ProgramResult {
    let initial = read_number(args)?;
    if !initial.fits_u128() {
        return Err(MockSyError::NumberTooLarge.into());
    }
    let mint = if args.len() >= Number::SIZEOF + 32 {
        read_pubkey(&args[Number::SIZEOF..])?
    } else {
        Pubkey::default()
    };

    let iter = &mut accounts.iter();
    let sy_global = next_account_info(iter)?;

    let (mut global, _) = load_global(sy_global, program_id)?;
    if global.emission_indexes.len() >= MAX_EMISSIONS {
        return Err(MockSyError::TooManyEmissions.into());
    }
    global.emission_mints.push(mint);
    global.emission_indexes.push(initial);
    store(sy_global, GLOBAL_TAG, &global)
}

// ---------------------------------------------------------------------------
// [203] fund_emission  (test control)
// accounts: sy_global(mut), sy_position(mut)
// args: u32 index, u64 amount
//
// Credits `amount` directly to the position's claimable balance. The harness is
// responsible for making sure `emission_custody` actually holds the tokens.
// ---------------------------------------------------------------------------

fn fund_emission(program_id: &Pubkey, accounts: &[AccountInfo], args: &[u8]) -> ProgramResult {
    let index = read_u32(args)? as usize;
    let amount = read_u64(&args[4..])?;

    let iter = &mut accounts.iter();
    let sy_global = next_account_info(iter)?;
    let sy_position = next_account_info(iter)?;

    let (global, _) = load_global(sy_global, program_id)?;
    let mut position = load_position(sy_position, program_id)?;
    position.sync_and_accrue(&global)?;

    let slot = position
        .emissions
        .get_mut(index)
        .ok_or(MockSyError::EmissionIndexOutOfRange)?;
    slot.amount_claimable = slot
        .amount_claimable
        .checked_add(amount)
        .ok_or(MockSyError::MathOverflow)?;

    store(sy_position, POSITION_TAG, &position)
}

// ---------------------------------------------------------------------------
// Address helpers for harnesses / tests
// ---------------------------------------------------------------------------

pub fn sy_global_address(program_id: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[GLOBAL_SEED], program_id)
}

pub fn sy_position_address(program_id: &Pubkey, owner: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[POSITION_SEED, owner.as_ref()], program_id)
}

pub fn sy_authority_address(program_id: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[AUTHORITY_SEED], program_id)
}
