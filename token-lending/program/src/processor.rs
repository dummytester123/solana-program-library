//! Program state processor

use crate::{
    dex_market::{DexMarket, TradeAction, TradeSimulator, BASE_MINT_OFFSET, QUOTE_MINT_OFFSET},
    error::LendingError,
    instruction::{BorrowAmountType, LendingInstruction},
    math::{Decimal, Rate, WAD},
    state::{LendingMarket, Obligation, Reserve, ReserveConfig, ReserveState, PROGRAM_VERSION},
};
use num_traits::FromPrimitive;
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    decode_error::DecodeError,
    entrypoint::ProgramResult,
    msg,
    program::{invoke, invoke_signed},
    program_error::{PrintProgramError, ProgramError},
    program_option::COption,
    program_pack::{IsInitialized, Pack},
    pubkey::Pubkey,
    sysvar::{clock::Clock, rent::Rent, Sysvar},
};
use spl_token::state::Account as Token;

/// Processes an instruction
pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    input: &[u8],
) -> ProgramResult {
    let instruction = LendingInstruction::unpack(input)?;
    match instruction {
        LendingInstruction::InitLendingMarket => {
            msg!("Instruction: Init Lending Market");
            process_init_lending_market(program_id, accounts)
        }
        LendingInstruction::InitReserve {
            liquidity_amount,
            config,
        } => {
            msg!("Instruction: Init Reserve");
            process_init_reserve(program_id, liquidity_amount, config, accounts)
        }
        LendingInstruction::DepositReserveLiquidity { liquidity_amount } => {
            msg!("Instruction: Deposit");
            process_deposit(program_id, liquidity_amount, accounts)
        }
        LendingInstruction::WithdrawReserveLiquidity { collateral_amount } => {
            msg!("Instruction: Withdraw");
            process_withdraw(program_id, collateral_amount, accounts)
        }
        LendingInstruction::BorrowReserveLiquidity {
            amount,
            amount_type,
        } => {
            msg!("Instruction: Borrow");
            process_borrow(program_id, amount, amount_type, accounts)
        }
        LendingInstruction::RepayReserveLiquidity { liquidity_amount } => {
            msg!("Instruction: Repay");
            process_repay(program_id, liquidity_amount, accounts)
        }
        LendingInstruction::LiquidateObligation { liquidity_amount } => {
            msg!("Instruction: Liquidate");
            process_liquidate(program_id, liquidity_amount, accounts)
        }
    }
}

fn process_init_lending_market(_program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let account_info_iter = &mut accounts.iter();
    let lending_market_info = next_account_info(account_info_iter)?;
    let quote_token_mint_info = next_account_info(account_info_iter)?;
    let rent = &Rent::from_account_info(next_account_info(account_info_iter)?)?;
    let token_program_id = next_account_info(account_info_iter)?;

    unpack_mint(&quote_token_mint_info.data.borrow())?;
    if quote_token_mint_info.owner != token_program_id.key {
        return Err(LendingError::InvalidTokenOwner.into());
    }

    assert_rent_exempt(rent, lending_market_info)?;
    let mut new_lending_market: LendingMarket = assert_uninitialized(lending_market_info)?;
    new_lending_market.version = PROGRAM_VERSION;
    new_lending_market.quote_token_mint = *quote_token_mint_info.key;
    LendingMarket::pack(
        new_lending_market,
        &mut lending_market_info.data.borrow_mut(),
    )?;

    Ok(())
}

fn process_init_reserve(
    program_id: &Pubkey,
    liquidity_amount: u64,
    config: ReserveConfig,
    accounts: &[AccountInfo],
) -> ProgramResult {
    if liquidity_amount == 0 {
        return Err(LendingError::InvalidAmount.into());
    }
    if config.optimal_utilization_rate > 100 {
        msg!("Optimal utilization rate must be in range [0, 100])");
        return Err(LendingError::InvalidConfig.into());
    }
    if config.loan_to_value_ratio > 90 {
        msg!("Loan to value ratio must be in range [0, 90]");
        return Err(LendingError::InvalidConfig.into());
    }
    if config.liquidation_bonus > 100 {
        msg!("Liquidation bonus must be in range [0, 100]");
        return Err(LendingError::InvalidConfig.into());
    }
    if config.liquidation_threshold > 100 {
        msg!("Liquidation threshold must be in range [0, 100]");
        return Err(LendingError::InvalidConfig.into());
    }
    if config.min_borrow_rate >= config.optimal_borrow_rate {
        msg!("Min borrow rate must be less than the optimal borrow rate");
        return Err(LendingError::InvalidConfig.into());
    }
    if config.optimal_borrow_rate >= config.max_borrow_rate {
        msg!("Optimal borrow rate must be less than the max borrow rate");
        return Err(LendingError::InvalidConfig.into());
    }
    if config.fees.borrow_fee_wad >= WAD {
        msg!("Borrow fee must be in range [0, 1_000_000_000_000_000_000)");
        return Err(LendingError::InvalidConfig.into());
    }
    if config.fees.host_fee_percentage > 100 {
        msg!("Host fee percentage must be in range [0, 100]");
        return Err(LendingError::InvalidConfig.into());
    }

    let account_info_iter = &mut accounts.iter();
    let source_liquidity_info = next_account_info(account_info_iter)?;
    let destination_collateral_info = next_account_info(account_info_iter)?;
    let reserve_info = next_account_info(account_info_iter)?;
    let reserve_liquidity_mint_info = next_account_info(account_info_iter)?;
    let reserve_liquidity_supply_info = next_account_info(account_info_iter)?;
    let reserve_collateral_mint_info = next_account_info(account_info_iter)?;
    let reserve_collateral_supply_info = next_account_info(account_info_iter)?;
    let reserve_collateral_fees_receiver_info = next_account_info(account_info_iter)?;
    let lending_market_info = next_account_info(account_info_iter)?;
    let lending_market_authority_info = next_account_info(account_info_iter)?;
    let user_transfer_authority_info = next_account_info(account_info_iter)?;
    let clock = &Clock::from_account_info(next_account_info(account_info_iter)?)?;
    let rent_info = next_account_info(account_info_iter)?;
    let rent = &Rent::from_account_info(rent_info)?;
    let token_program_id = next_account_info(account_info_iter)?;

    assert_rent_exempt(rent, reserve_info)?;
    assert_uninitialized::<Reserve>(reserve_info)?;

    let lending_market = LendingMarket::unpack(&lending_market_info.data.borrow())?;
    if lending_market_info.owner != program_id {
        return Err(LendingError::InvalidAccountOwner.into());
    }
    if !lending_market_info.is_signer {
        return Err(LendingError::InvalidSigner.into());
    }
    if &lending_market.token_program_id != token_program_id.key {
        return Err(LendingError::InvalidTokenProgram.into());
    }

    let dex_market = if reserve_liquidity_mint_info.key != &lending_market.quote_token_mint {
        let dex_market_info = next_account_info(account_info_iter)?;
        // TODO: check that market state is owned by real serum dex program
        if !rent.is_exempt(dex_market_info.lamports(), dex_market_info.data_len()) {
            return Err(LendingError::NotRentExempt.into());
        }

        let dex_market_data = &dex_market_info.data.borrow();
        let market_quote_mint = DexMarket::pubkey_at_offset(&dex_market_data, QUOTE_MINT_OFFSET);
        if lending_market.quote_token_mint != market_quote_mint {
            return Err(LendingError::DexMarketMintMismatch.into());
        }
        let market_base_mint = DexMarket::pubkey_at_offset(&dex_market_data, BASE_MINT_OFFSET);
        if reserve_liquidity_mint_info.key != &market_base_mint {
            return Err(LendingError::DexMarketMintMismatch.into());
        }

        COption::Some(*dex_market_info.key)
    } else {
        COption::None
    };

    let (lending_market_authority_pubkey, bump_seed) =
        Pubkey::find_program_address(&[lending_market_info.key.as_ref()], program_id);
    if lending_market_authority_info.key != &lending_market_authority_pubkey {
        return Err(LendingError::InvalidMarketAuthority.into());
    }

    let liquidity_reserve_mint = unpack_mint(&reserve_liquidity_mint_info.data.borrow())?;
    spl_token_init_account(TokenInitializeAccountParams {
        account: reserve_liquidity_supply_info.clone(),
        mint: reserve_liquidity_mint_info.clone(),
        owner: lending_market_authority_info.clone(),
        rent: rent_info.clone(),
        token_program: token_program_id.clone(),
    })?;

    spl_token_init_mint(TokenInitializeMintParams {
        mint: reserve_collateral_mint_info.clone(),
        authority: lending_market_authority_info.key,
        rent: rent_info.clone(),
        decimals: liquidity_reserve_mint.decimals,
        token_program: token_program_id.clone(),
    })?;

    spl_token_init_account(TokenInitializeAccountParams {
        account: reserve_collateral_supply_info.clone(),
        mint: reserve_collateral_mint_info.clone(),
        owner: lending_market_authority_info.clone(),
        rent: rent_info.clone(),
        token_program: token_program_id.clone(),
    })?;

    spl_token_init_account(TokenInitializeAccountParams {
        account: reserve_collateral_fees_receiver_info.clone(),
        mint: reserve_collateral_mint_info.clone(),
        owner: lending_market_info.clone(),
        rent: rent_info.clone(),
        token_program: token_program_id.clone(),
    })?;

    spl_token_init_account(TokenInitializeAccountParams {
        account: destination_collateral_info.clone(),
        mint: reserve_collateral_mint_info.clone(),
        owner: lending_market_authority_info.clone(),
        rent: rent_info.clone(),
        token_program: token_program_id.clone(),
    })?;

    let authority_signer_seeds = &[lending_market_info.key.as_ref(), &[bump_seed]];
    spl_token_transfer(TokenTransferParams {
        source: source_liquidity_info.clone(),
        destination: reserve_liquidity_supply_info.clone(),
        amount: liquidity_amount,
        authority: user_transfer_authority_info.clone(),
        authority_signer_seeds: &[],
        token_program: token_program_id.clone(),
    })?;

    let reserve_state = ReserveState::new(clock.slot, liquidity_amount);
    spl_token_mint_to(TokenMintToParams {
        mint: reserve_collateral_mint_info.clone(),
        destination: destination_collateral_info.clone(),
        amount: reserve_state.collateral_mint_supply,
        authority: lending_market_authority_info.clone(),
        authority_signer_seeds,
        token_program: token_program_id.clone(),
    })?;

    Reserve::pack(
        Reserve {
            version: PROGRAM_VERSION,
            lending_market: *lending_market_info.key,
            liquidity_mint: *reserve_liquidity_mint_info.key,
            liquidity_mint_decimals: liquidity_reserve_mint.decimals,
            liquidity_supply: *reserve_liquidity_supply_info.key,
            collateral_mint: *reserve_collateral_mint_info.key,
            collateral_supply: *reserve_collateral_supply_info.key,
            collateral_fees_receiver: *reserve_collateral_fees_receiver_info.key,
            dex_market,
            state: reserve_state,
            config,
        },
        &mut reserve_info.data.borrow_mut(),
    )?;

    Ok(())
}

fn process_deposit(
    program_id: &Pubkey,
    liquidity_amount: u64,
    accounts: &[AccountInfo],
) -> ProgramResult {
    if liquidity_amount == 0 {
        return Err(LendingError::InvalidAmount.into());
    }

    let account_info_iter = &mut accounts.iter();
    let source_liquidity_info = next_account_info(account_info_iter)?;
    let destination_collateral_info = next_account_info(account_info_iter)?;
    let reserve_info = next_account_info(account_info_iter)?;
    let reserve_liquidity_supply_info = next_account_info(account_info_iter)?;
    let reserve_collateral_mint_info = next_account_info(account_info_iter)?;
    let lending_market_info = next_account_info(account_info_iter)?;
    let lending_market_authority_info = next_account_info(account_info_iter)?;
    let user_transfer_authority_info = next_account_info(account_info_iter)?;
    let clock = &Clock::from_account_info(next_account_info(account_info_iter)?)?;
    let token_program_id = next_account_info(account_info_iter)?;

    let lending_market = LendingMarket::unpack(&lending_market_info.data.borrow())?;
    if lending_market_info.owner != program_id {
        return Err(LendingError::InvalidAccountOwner.into());
    }
    if &lending_market.token_program_id != token_program_id.key {
        return Err(LendingError::InvalidTokenProgram.into());
    }

    let mut reserve = Reserve::unpack(&reserve_info.data.borrow())?;
    if reserve_info.owner != program_id {
        return Err(LendingError::InvalidAccountOwner.into());
    }
    if &reserve.lending_market != lending_market_info.key {
        msg!("Invalid reserve lending market account");
        return Err(LendingError::InvalidAccountInput.into());
    }
    if &reserve.liquidity_supply != reserve_liquidity_supply_info.key {
        msg!("Invalid reserve liquidity supply account");
        return Err(LendingError::InvalidAccountInput.into());
    }
    if &reserve.collateral_mint != reserve_collateral_mint_info.key {
        msg!("Invalid reserve collateral mint account");
        return Err(LendingError::InvalidAccountInput.into());
    }
    if &reserve.liquidity_supply == source_liquidity_info.key {
        msg!("Cannot use reserve liquidity supply as source account input");
        return Err(LendingError::InvalidAccountInput.into());
    }
    if &reserve.collateral_supply == destination_collateral_info.key {
        msg!("Cannot use reserve collateral supply as destination account input");
        return Err(LendingError::InvalidAccountInput.into());
    }

    reserve.accrue_interest(clock.slot);
    let collateral_amount = reserve.deposit_liquidity(liquidity_amount);
    Reserve::pack(reserve, &mut reserve_info.data.borrow_mut())?;

    let (lending_market_authority_pubkey, bump_seed) =
        Pubkey::find_program_address(&[lending_market_info.key.as_ref()], program_id);
    if lending_market_authority_info.key != &lending_market_authority_pubkey {
        return Err(LendingError::InvalidMarketAuthority.into());
    }

    let authority_signer_seeds = &[lending_market_info.key.as_ref(), &[bump_seed]];
    spl_token_transfer(TokenTransferParams {
        source: source_liquidity_info.clone(),
        destination: reserve_liquidity_supply_info.clone(),
        amount: liquidity_amount,
        authority: user_transfer_authority_info.clone(),
        authority_signer_seeds: &[],
        token_program: token_program_id.clone(),
    })?;

    spl_token_mint_to(TokenMintToParams {
        mint: reserve_collateral_mint_info.clone(),
        destination: destination_collateral_info.clone(),
        amount: collateral_amount,
        authority: lending_market_authority_info.clone(),
        authority_signer_seeds,
        token_program: token_program_id.clone(),
    })?;

    Ok(())
}

fn process_withdraw(
    program_id: &Pubkey,
    collateral_amount: u64,
    accounts: &[AccountInfo],
) -> ProgramResult {
    if collateral_amount == 0 {
        return Err(LendingError::InvalidAmount.into());
    }

    let account_info_iter = &mut accounts.iter();
    let source_collateral_info = next_account_info(account_info_iter)?;
    let destination_liquidity_info = next_account_info(account_info_iter)?;
    let reserve_info = next_account_info(account_info_iter)?;
    let reserve_collateral_mint_info = next_account_info(account_info_iter)?;
    let reserve_liquidity_supply_info = next_account_info(account_info_iter)?;
    let lending_market_info = next_account_info(account_info_iter)?;
    let lending_market_authority_info = next_account_info(account_info_iter)?;
    let user_transfer_authority_info = next_account_info(account_info_iter)?;
    let clock = &Clock::from_account_info(next_account_info(account_info_iter)?)?;
    let token_program_id = next_account_info(account_info_iter)?;

    let lending_market = LendingMarket::unpack(&lending_market_info.data.borrow())?;
    if lending_market_info.owner != program_id {
        return Err(LendingError::InvalidAccountOwner.into());
    }
    if &lending_market.token_program_id != token_program_id.key {
        return Err(LendingError::InvalidTokenProgram.into());
    }

    let mut reserve = Reserve::unpack(&reserve_info.data.borrow())?;
    if reserve_info.owner != program_id {
        return Err(LendingError::InvalidAccountOwner.into());
    }
    if &reserve.lending_market != lending_market_info.key {
        msg!("Invalid reserve lending market account");
        return Err(LendingError::InvalidAccountInput.into());
    }
    if &reserve.liquidity_supply != reserve_liquidity_supply_info.key {
        msg!("Invalid reserve liquidity supply account");
        return Err(LendingError::InvalidAccountInput.into());
    }
    if &reserve.collateral_mint != reserve_collateral_mint_info.key {
        msg!("Invalid reserve collateral mint account");
        return Err(LendingError::InvalidAccountInput.into());
    }
    if &reserve.liquidity_supply == destination_liquidity_info.key {
        msg!("Cannot use reserve liquidity supply as destination account input");
        return Err(LendingError::InvalidAccountInput.into());
    }
    if &reserve.collateral_supply == source_collateral_info.key {
        msg!("Cannot use reserve collateral supply as source account input");
        return Err(LendingError::InvalidAccountInput.into());
    }

    reserve.accrue_interest(clock.slot);
    let liquidity_withdraw_amount = reserve.redeem_collateral(collateral_amount)?;
    Reserve::pack(reserve, &mut reserve_info.data.borrow_mut())?;

    let (lending_market_authority_pubkey, bump_seed) =
        Pubkey::find_program_address(&[lending_market_info.key.as_ref()], program_id);
    if lending_market_authority_info.key != &lending_market_authority_pubkey {
        return Err(LendingError::InvalidMarketAuthority.into());
    }
    let authority_signer_seeds = &[lending_market_info.key.as_ref(), &[bump_seed]];

    spl_token_transfer(TokenTransferParams {
        source: reserve_liquidity_supply_info.clone(),
        destination: destination_liquidity_info.clone(),
        amount: liquidity_withdraw_amount,
        authority: lending_market_authority_info.clone(),
        authority_signer_seeds,
        token_program: token_program_id.clone(),
    })?;

    spl_token_burn(TokenBurnParams {
        mint: reserve_collateral_mint_info.clone(),
        source: source_collateral_info.clone(),
        amount: collateral_amount,
        authority: user_transfer_authority_info.clone(),
        authority_signer_seeds: &[],
        token_program: token_program_id.clone(),
    })?;

    Ok(())
}

#[inline(never)] // avoid stack frame limit
fn process_borrow(
    program_id: &Pubkey,
    amount: u64,
    amount_type: BorrowAmountType,
    accounts: &[AccountInfo],
) -> ProgramResult {
    if amount == 0 {
        return Err(LendingError::InvalidAmount.into());
    }

    let account_info_iter = &mut accounts.iter();
    let source_collateral_info = next_account_info(account_info_iter)?;
    let destination_liquidity_info = next_account_info(account_info_iter)?;
    let deposit_reserve_info = next_account_info(account_info_iter)?;
    let deposit_reserve_collateral_supply_info = next_account_info(account_info_iter)?;
    let deposit_reserve_collateral_fees_receiver_info = next_account_info(account_info_iter)?;
    let borrow_reserve_info = next_account_info(account_info_iter)?;
    let borrow_reserve_liquidity_supply_info = next_account_info(account_info_iter)?;
    let obligation_info = next_account_info(account_info_iter)?;
    let obligation_token_mint_info = next_account_info(account_info_iter)?;
    let obligation_token_output_info = next_account_info(account_info_iter)?;
    let obligation_token_owner_info = next_account_info(account_info_iter)?;
    let lending_market_info = next_account_info(account_info_iter)?;
    let lending_market_authority_info = next_account_info(account_info_iter)?;
    let user_transfer_authority_info = next_account_info(account_info_iter)?;
    let dex_market_info = next_account_info(account_info_iter)?;
    let dex_market_orders_info = next_account_info(account_info_iter)?;
    let memory = next_account_info(account_info_iter)?;
    let clock = &Clock::from_account_info(next_account_info(account_info_iter)?)?;
    let rent_info = next_account_info(account_info_iter)?;
    let rent = &Rent::from_account_info(rent_info)?;
    let token_program_id = next_account_info(account_info_iter)?;

    let lending_market = LendingMarket::unpack(&lending_market_info.data.borrow())?;
    if lending_market_info.owner != program_id {
        return Err(LendingError::InvalidAccountOwner.into());
    }
    if &lending_market.token_program_id != token_program_id.key {
        return Err(LendingError::InvalidTokenProgram.into());
    }

    let mut deposit_reserve = Reserve::unpack(&deposit_reserve_info.data.borrow())?;
    if deposit_reserve_info.owner != program_id {
        return Err(LendingError::InvalidAccountOwner.into());
    }
    if &deposit_reserve.lending_market != lending_market_info.key {
        msg!("Invalid reserve lending market account");
        return Err(LendingError::InvalidAccountInput.into());
    }

    let mut borrow_reserve = Reserve::unpack(&borrow_reserve_info.data.borrow())?;
    if borrow_reserve_info.owner != program_id {
        return Err(LendingError::InvalidAccountOwner.into());
    }
    if borrow_reserve.lending_market != deposit_reserve.lending_market {
        return Err(LendingError::LendingMarketMismatch.into());
    }

    if deposit_reserve.config.loan_to_value_ratio == 0 {
        return Err(LendingError::ReserveCollateralDisabled.into());
    }
    if deposit_reserve_info.key == borrow_reserve_info.key {
        return Err(LendingError::DuplicateReserve.into());
    }
    if deposit_reserve.liquidity_mint == borrow_reserve.liquidity_mint {
        return Err(LendingError::DuplicateReserveMint.into());
    }
    if &borrow_reserve.liquidity_supply != borrow_reserve_liquidity_supply_info.key {
        msg!("Invalid borrow reserve liquidity supply account input");
        return Err(LendingError::InvalidAccountInput.into());
    }
    if &deposit_reserve.collateral_supply != deposit_reserve_collateral_supply_info.key {
        msg!("Invalid deposit reserve collateral supply account input");
        return Err(LendingError::InvalidAccountInput.into());
    }
    if &deposit_reserve.collateral_supply == source_collateral_info.key {
        msg!("Cannot use deposit reserve collateral supply as source account input");
        return Err(LendingError::InvalidAccountInput.into());
    }
    if &deposit_reserve.collateral_fees_receiver
        != deposit_reserve_collateral_fees_receiver_info.key
    {
        msg!("Invalid deposit reserve collateral fees receiver account");
        return Err(LendingError::InvalidAccountInput.into());
    }
    if &borrow_reserve.liquidity_supply == destination_liquidity_info.key {
        msg!("Cannot use borrow reserve liquidity supply as destination account input");
        return Err(LendingError::InvalidAccountInput.into());
    }

    // TODO: handle case when neither reserve is the quote currency
    if let COption::Some(dex_market_pubkey) = borrow_reserve.dex_market {
        if &dex_market_pubkey != dex_market_info.key {
            msg!("Invalid dex market account input");
            return Err(LendingError::InvalidAccountInput.into());
        }
    }
    if let COption::Some(dex_market_pubkey) = deposit_reserve.dex_market {
        if &dex_market_pubkey != dex_market_info.key {
            msg!("Invalid dex market account input");
            return Err(LendingError::InvalidAccountInput.into());
        }
    }

    // accrue interest and update rates
    borrow_reserve.accrue_interest(clock.slot);
    deposit_reserve.accrue_interest(clock.slot);
    let cumulative_borrow_rate = borrow_reserve.state.cumulative_borrow_rate_wads;

    let mut trade_simulator = TradeSimulator::new(
        dex_market_info,
        dex_market_orders_info,
        memory,
        &lending_market.quote_token_mint,
    )?;

    let (borrow_amount, mut collateral_deposit_amount) = trade_simulator.calculate_borrow_amounts(
        &deposit_reserve,
        &borrow_reserve,
        amount_type,
        amount,
    )?;

    let (mut borrow_fee, host_fee) = deposit_reserve
        .config
        .fees
        .calculate_borrow_fees(collateral_deposit_amount)?;
    // update amount actually deposited
    collateral_deposit_amount -= borrow_fee;

    borrow_reserve.state.add_borrow(borrow_amount)?;

    let lending_market_key = deposit_reserve.lending_market;
    let obligation_mint_decimals = deposit_reserve.liquidity_mint_decimals;

    Reserve::pack(deposit_reserve, &mut deposit_reserve_info.data.borrow_mut())?;
    Reserve::pack(borrow_reserve, &mut borrow_reserve_info.data.borrow_mut())?;

    let mut obligation = Obligation::unpack_unchecked(&obligation_info.data.borrow())?;
    let reusing_obligation = obligation.is_initialized();
    if reusing_obligation {
        if &obligation.token_mint != obligation_token_mint_info.key {
            msg!("Obligation token mint input doesn't match existing obligation token mint");
            return Err(LendingError::InvalidAccountInput.into());
        }
        if &obligation.borrow_reserve != borrow_reserve_info.key {
            msg!("Borrow reserve input doesn't match existing obligation borrow reserve");
            return Err(LendingError::InvalidAccountInput.into());
        }
        if &obligation.collateral_reserve != deposit_reserve_info.key {
            msg!("Collateral reserve input doesn't match existing obligation collateral reserve");
            return Err(LendingError::InvalidAccountInput.into());
        }

        obligation.accrue_interest(clock, cumulative_borrow_rate);
        obligation.borrowed_liquidity_wads += Decimal::from(borrow_amount);
        obligation.deposited_collateral_tokens += collateral_deposit_amount;
        Obligation::pack(obligation, &mut obligation_info.data.borrow_mut())?;
    } else {
        assert_rent_exempt(rent, obligation_info)?;
        let mut new_obligation = obligation;
        new_obligation.version = PROGRAM_VERSION;
        new_obligation.last_update_slot = clock.slot;
        new_obligation.deposited_collateral_tokens = collateral_deposit_amount;
        new_obligation.collateral_reserve = *deposit_reserve_info.key;
        new_obligation.cumulative_borrow_rate_wads = cumulative_borrow_rate;
        new_obligation.borrowed_liquidity_wads = Decimal::from(borrow_amount);
        new_obligation.borrow_reserve = *borrow_reserve_info.key;
        new_obligation.token_mint = *obligation_token_mint_info.key;
        Obligation::pack(new_obligation, &mut obligation_info.data.borrow_mut())?;
    }

    let (lending_market_authority_pubkey, bump_seed) =
        Pubkey::find_program_address(&[lending_market_key.as_ref()], program_id);
    if lending_market_authority_info.key != &lending_market_authority_pubkey {
        return Err(LendingError::InvalidMarketAuthority.into());
    }
    let authority_signer_seeds = &[lending_market_key.as_ref(), &[bump_seed]];

    // deposit collateral
    spl_token_transfer(TokenTransferParams {
        source: source_collateral_info.clone(),
        destination: deposit_reserve_collateral_supply_info.clone(),
        amount: collateral_deposit_amount,
        authority: user_transfer_authority_info.clone(),
        authority_signer_seeds: &[],
        token_program: token_program_id.clone(),
    })?;

    // transfer host fees if host is specified
    if let Ok(host_fee_recipient) = next_account_info(account_info_iter) {
        if host_fee > 0 {
            borrow_fee -= host_fee;
            spl_token_transfer(TokenTransferParams {
                source: source_collateral_info.clone(),
                destination: host_fee_recipient.clone(),
                amount: host_fee,
                authority: user_transfer_authority_info.clone(),
                authority_signer_seeds: &[],
                token_program: token_program_id.clone(),
            })?;
        }
    }

    // transfer remaining fees to owner
    if borrow_fee > 0 {
        spl_token_transfer(TokenTransferParams {
            source: source_collateral_info.clone(),
            destination: deposit_reserve_collateral_fees_receiver_info.clone(),
            amount: borrow_fee,
            authority: user_transfer_authority_info.clone(),
            authority_signer_seeds: &[],
            token_program: token_program_id.clone(),
        })?;
    }

    // borrow liquidity
    spl_token_transfer(TokenTransferParams {
        source: borrow_reserve_liquidity_supply_info.clone(),
        destination: destination_liquidity_info.clone(),
        amount: borrow_amount,
        authority: lending_market_authority_info.clone(),
        authority_signer_seeds,
        token_program: token_program_id.clone(),
    })?;

    if !reusing_obligation {
        // init obligation token mint
        spl_token_init_mint(TokenInitializeMintParams {
            mint: obligation_token_mint_info.clone(),
            authority: lending_market_authority_info.key,
            rent: rent_info.clone(),
            decimals: obligation_mint_decimals,
            token_program: token_program_id.clone(),
        })?;
    }

    let obligation_token_output = if reusing_obligation {
        let obligation_token_output =
            Token::unpack_unchecked(&obligation_token_output_info.data.borrow())?;
        if obligation_token_output.is_initialized() {
            Some(obligation_token_output)
        } else {
            None
        }
    } else {
        None
    };

    if let Some(token_output) = obligation_token_output {
        if &token_output.owner != obligation_token_owner_info.key {
            return Err(LendingError::ObligationTokenOwnerMismatch.into());
        }
    } else {
        // init obligation token output account
        spl_token_init_account(TokenInitializeAccountParams {
            account: obligation_token_output_info.clone(),
            mint: obligation_token_mint_info.clone(),
            owner: obligation_token_owner_info.clone(),
            rent: rent_info.clone(),
            token_program: token_program_id.clone(),
        })?;
    }

    // mint obligation tokens to output account
    spl_token_mint_to(TokenMintToParams {
        mint: obligation_token_mint_info.clone(),
        destination: obligation_token_output_info.clone(),
        amount: collateral_deposit_amount,
        authority: lending_market_authority_info.clone(),
        authority_signer_seeds,
        token_program: token_program_id.clone(),
    })?;

    Ok(())
}

#[inline(never)] // avoid stack frame limit
fn process_repay(
    program_id: &Pubkey,
    liquidity_amount: u64,
    accounts: &[AccountInfo],
) -> ProgramResult {
    if liquidity_amount == 0 {
        return Err(LendingError::InvalidAmount.into());
    }

    let account_info_iter = &mut accounts.iter();
    let source_liquidity_info = next_account_info(account_info_iter)?;
    let destination_collateral_info = next_account_info(account_info_iter)?;
    let repay_reserve_info = next_account_info(account_info_iter)?;
    let repay_reserve_liquidity_supply_info = next_account_info(account_info_iter)?;
    let withdraw_reserve_info = next_account_info(account_info_iter)?;
    let withdraw_reserve_collateral_supply_info = next_account_info(account_info_iter)?;
    let obligation_info = next_account_info(account_info_iter)?;
    let obligation_token_mint_info = next_account_info(account_info_iter)?;
    let obligation_token_input_info = next_account_info(account_info_iter)?;
    let lending_market_info = next_account_info(account_info_iter)?;
    let lending_market_authority_info = next_account_info(account_info_iter)?;
    let user_transfer_authority_info = next_account_info(account_info_iter)?;
    let clock = &Clock::from_account_info(next_account_info(account_info_iter)?)?;
    let token_program_id = next_account_info(account_info_iter)?;

    let lending_market = LendingMarket::unpack(&lending_market_info.data.borrow())?;
    if lending_market_info.owner != program_id {
        return Err(LendingError::InvalidAccountOwner.into());
    }
    if &lending_market.token_program_id != token_program_id.key {
        return Err(LendingError::InvalidTokenProgram.into());
    }

    let mut obligation = Obligation::unpack(&obligation_info.data.borrow())?;
    if obligation_info.owner != program_id {
        return Err(LendingError::InvalidAccountOwner.into());
    }
    if &obligation.token_mint != obligation_token_mint_info.key {
        msg!("Invalid obligation token mint account");
        return Err(LendingError::InvalidAccountInput.into());
    }
    if &obligation.borrow_reserve != repay_reserve_info.key {
        msg!("Invalid repay reserve account");
        return Err(LendingError::InvalidAccountInput.into());
    }
    if &obligation.collateral_reserve != withdraw_reserve_info.key {
        msg!("Invalid withdraw reserve account");
        return Err(LendingError::InvalidAccountInput.into());
    }

    let mut repay_reserve = Reserve::unpack(&repay_reserve_info.data.borrow())?;
    if repay_reserve_info.owner != program_id {
        return Err(LendingError::InvalidAccountOwner.into());
    }
    if &repay_reserve.lending_market != lending_market_info.key {
        msg!("Invalid reserve lending market account");
        return Err(LendingError::InvalidAccountInput.into());
    }

    let withdraw_reserve = Reserve::unpack(&withdraw_reserve_info.data.borrow())?;
    if withdraw_reserve_info.owner != program_id {
        return Err(LendingError::InvalidAccountOwner.into());
    }
    if withdraw_reserve.lending_market != repay_reserve.lending_market {
        return Err(LendingError::LendingMarketMismatch.into());
    }

    if repay_reserve_info.key == withdraw_reserve_info.key {
        return Err(LendingError::DuplicateReserve.into());
    }
    if repay_reserve.liquidity_mint == withdraw_reserve.liquidity_mint {
        return Err(LendingError::DuplicateReserveMint.into());
    }
    if &repay_reserve.liquidity_supply != repay_reserve_liquidity_supply_info.key {
        msg!("Invalid repay reserve liquidity supply account");
        return Err(LendingError::InvalidAccountInput.into());
    }
    if &withdraw_reserve.collateral_supply != withdraw_reserve_collateral_supply_info.key {
        msg!("Invalid withdraw reserve collateral supply account");
        return Err(LendingError::InvalidAccountInput.into());
    }
    if &repay_reserve.liquidity_supply == source_liquidity_info.key {
        msg!("Cannot use repay reserve liquidity supply as source account input");
        return Err(LendingError::InvalidAccountInput.into());
    }
    if &withdraw_reserve.collateral_supply == destination_collateral_info.key {
        msg!("Cannot use withdraw reserve collateral supply as destination account input");
        return Err(LendingError::InvalidAccountInput.into());
    }

    // accrue interest and update rates
    repay_reserve.accrue_interest(clock.slot);
    obligation.accrue_interest(clock, repay_reserve.state.cumulative_borrow_rate_wads);

    let repay_amount = Decimal::from(liquidity_amount).min(obligation.borrowed_liquidity_wads);
    let rounded_repay_amount = repay_amount.round_u64();
    if rounded_repay_amount == 0 {
        return Err(LendingError::ObligationTooSmall.into());
    }

    repay_reserve.state.subtract_repay(repay_amount);
    Reserve::pack(repay_reserve, &mut repay_reserve_info.data.borrow_mut())?;

    let repay_pct: Decimal = repay_amount / obligation.borrowed_liquidity_wads;
    let collateral_withdraw_amount = {
        let withdraw_amount: Decimal = repay_pct * obligation.deposited_collateral_tokens;
        withdraw_amount.round_u64()
    };

    let obligation_token_amount = {
        let obligation_mint = &unpack_mint(&obligation_token_mint_info.data.borrow())?;
        let token_amount: Decimal = repay_pct * obligation_mint.supply;
        token_amount.round_u64()
    };

    obligation.borrowed_liquidity_wads -= repay_amount;
    obligation.deposited_collateral_tokens -= collateral_withdraw_amount;
    Obligation::pack(obligation, &mut obligation_info.data.borrow_mut())?;

    let (lending_market_authority_pubkey, bump_seed) =
        Pubkey::find_program_address(&[lending_market_info.key.as_ref()], program_id);
    if lending_market_authority_info.key != &lending_market_authority_pubkey {
        return Err(LendingError::InvalidMarketAuthority.into());
    }
    let authority_signer_seeds = &[lending_market_info.key.as_ref(), &[bump_seed]];

    // deposit repaid liquidity
    spl_token_transfer(TokenTransferParams {
        source: source_liquidity_info.clone(),
        destination: repay_reserve_liquidity_supply_info.clone(),
        amount: rounded_repay_amount,
        authority: user_transfer_authority_info.clone(),
        authority_signer_seeds: &[],
        token_program: token_program_id.clone(),
    })?;

    // withdraw collateral
    spl_token_transfer(TokenTransferParams {
        source: withdraw_reserve_collateral_supply_info.clone(),
        destination: destination_collateral_info.clone(),
        amount: collateral_withdraw_amount,
        authority: lending_market_authority_info.clone(),
        authority_signer_seeds,
        token_program: token_program_id.clone(),
    })?;

    // burn obligation tokens
    spl_token_burn(TokenBurnParams {
        mint: obligation_token_mint_info.clone(),
        source: obligation_token_input_info.clone(),
        amount: obligation_token_amount,
        authority: user_transfer_authority_info.clone(),
        authority_signer_seeds: &[],
        token_program: token_program_id.clone(),
    })?;

    Ok(())
}

#[inline(never)] // avoid stack frame limit
fn process_liquidate(
    program_id: &Pubkey,
    liquidity_amount: u64,
    accounts: &[AccountInfo],
) -> ProgramResult {
    if liquidity_amount == 0 {
        return Err(LendingError::InvalidAmount.into());
    }

    let account_info_iter = &mut accounts.iter();
    let source_liquidity_info = next_account_info(account_info_iter)?;
    let destination_collateral_info = next_account_info(account_info_iter)?;
    let repay_reserve_info = next_account_info(account_info_iter)?;
    let repay_reserve_liquidity_supply_info = next_account_info(account_info_iter)?;
    let withdraw_reserve_info = next_account_info(account_info_iter)?;
    let withdraw_reserve_collateral_supply_info = next_account_info(account_info_iter)?;
    let obligation_info = next_account_info(account_info_iter)?;
    let lending_market_info = next_account_info(account_info_iter)?;
    let lending_market_authority_info = next_account_info(account_info_iter)?;
    let user_transfer_authority_info = next_account_info(account_info_iter)?;
    let dex_market_info = next_account_info(account_info_iter)?;
    let dex_market_orders_info = next_account_info(account_info_iter)?;
    let memory = next_account_info(account_info_iter)?;
    let clock = &Clock::from_account_info(next_account_info(account_info_iter)?)?;
    let token_program_id = next_account_info(account_info_iter)?;

    let lending_market = LendingMarket::unpack(&lending_market_info.data.borrow())?;
    if lending_market_info.owner != program_id {
        return Err(LendingError::InvalidAccountOwner.into());
    }
    if &lending_market.token_program_id != token_program_id.key {
        return Err(LendingError::InvalidTokenProgram.into());
    }

    let mut obligation = Obligation::unpack(&obligation_info.data.borrow())?;
    if obligation_info.owner != program_id {
        return Err(LendingError::InvalidAccountOwner.into());
    }
    if &obligation.borrow_reserve != repay_reserve_info.key {
        msg!("Invalid repay reserve account");
        return Err(LendingError::InvalidAccountInput.into());
    }
    if &obligation.collateral_reserve != withdraw_reserve_info.key {
        msg!("Invalid withdraw reserve account");
        return Err(LendingError::InvalidAccountInput.into());
    }

    let mut repay_reserve = Reserve::unpack(&repay_reserve_info.data.borrow())?;
    if repay_reserve_info.owner != program_id {
        return Err(LendingError::InvalidAccountOwner.into());
    }
    if &repay_reserve.lending_market != lending_market_info.key {
        msg!("Invalid reserve lending market account");
        return Err(LendingError::InvalidAccountInput.into());
    }

    let mut withdraw_reserve = Reserve::unpack(&withdraw_reserve_info.data.borrow())?;
    if withdraw_reserve_info.owner != program_id {
        return Err(LendingError::InvalidAccountOwner.into());
    }
    if withdraw_reserve.lending_market != repay_reserve.lending_market {
        return Err(LendingError::LendingMarketMismatch.into());
    }

    if repay_reserve_info.key == withdraw_reserve_info.key {
        return Err(LendingError::DuplicateReserve.into());
    }
    if repay_reserve.liquidity_mint == withdraw_reserve.liquidity_mint {
        return Err(LendingError::DuplicateReserveMint.into());
    }
    if &repay_reserve.liquidity_supply != repay_reserve_liquidity_supply_info.key {
        msg!("Invalid repay reserve liquidity supply account");
        return Err(LendingError::InvalidAccountInput.into());
    }
    if &withdraw_reserve.collateral_supply != withdraw_reserve_collateral_supply_info.key {
        msg!("Invalid withdraw reserve collateral supply account");
        return Err(LendingError::InvalidAccountInput.into());
    }
    if &repay_reserve.liquidity_supply == source_liquidity_info.key {
        msg!("Cannot use repay reserve liquidity supply as source account input");
        return Err(LendingError::InvalidAccountInput.into());
    }
    if &withdraw_reserve.collateral_supply == destination_collateral_info.key {
        msg!("Cannot use withdraw reserve collateral supply as destination account input");
        return Err(LendingError::InvalidAccountInput.into());
    }

    // TODO: handle case when neither reserve is the quote currency
    if let COption::Some(dex_market_pubkey) = repay_reserve.dex_market {
        if &dex_market_pubkey != dex_market_info.key {
            msg!("Invalid dex market account");
            return Err(LendingError::InvalidAccountInput.into());
        }
    }
    if let COption::Some(dex_market_pubkey) = withdraw_reserve.dex_market {
        if &dex_market_pubkey != dex_market_info.key {
            msg!("Invalid dex market account");
            return Err(LendingError::InvalidAccountInput.into());
        }
    }

    // accrue interest and update rates
    repay_reserve.accrue_interest(clock.slot);
    withdraw_reserve.accrue_interest(clock.slot);
    obligation.accrue_interest(clock, repay_reserve.state.cumulative_borrow_rate_wads);

    let mut trade_simulator = TradeSimulator::new(
        dex_market_info,
        dex_market_orders_info,
        memory,
        &lending_market.quote_token_mint,
    )?;

    // calculate obligation health
    let withdraw_reserve_collateral_exchange_rate =
        withdraw_reserve.state.collateral_exchange_rate();
    let borrow_amount_as_collateral = withdraw_reserve_collateral_exchange_rate
        .liquidity_to_collateral(
            trade_simulator
                .simulate_trade(
                    TradeAction::Sell,
                    obligation.borrowed_liquidity_wads,
                    &repay_reserve.liquidity_mint,
                    true,
                )?
                .round_u64(),
        );

    if 100 * borrow_amount_as_collateral / obligation.deposited_collateral_tokens
        < withdraw_reserve.config.liquidation_threshold as u64
    {
        return Err(LendingError::HealthyObligation.into());
    }

    // calculate the amount of liquidity that will be repaid
    let close_factor = Rate::from_percent(50);
    let repay_amount =
        Decimal::from(liquidity_amount).min(obligation.borrowed_liquidity_wads * close_factor);
    let rounded_repay_amount = repay_amount.round_u64();
    if rounded_repay_amount == 0 {
        return Err(LendingError::ObligationTooSmall.into());
    }
    repay_reserve.state.subtract_repay(repay_amount);

    // TODO: check math precision
    // calculate the amount of collateral that will be withdrawn

    let withdraw_liquidity_amount = trade_simulator.simulate_trade(
        TradeAction::Sell,
        repay_amount,
        &repay_reserve.liquidity_mint,
        false,
    )?;
    let withdraw_amount_as_collateral = withdraw_reserve_collateral_exchange_rate
        .decimal_liquidity_to_collateral(withdraw_liquidity_amount)
        .round_u64();
    let liquidation_bonus_amount =
        withdraw_amount_as_collateral * (withdraw_reserve.config.liquidation_bonus as u64) / 100;
    let collateral_withdraw_amount = obligation
        .deposited_collateral_tokens
        .min(withdraw_amount_as_collateral + liquidation_bonus_amount);

    Reserve::pack(repay_reserve, &mut repay_reserve_info.data.borrow_mut())?;
    Reserve::pack(
        withdraw_reserve,
        &mut withdraw_reserve_info.data.borrow_mut(),
    )?;

    obligation.last_update_slot = clock.slot;
    obligation.borrowed_liquidity_wads -= repay_amount;
    obligation.deposited_collateral_tokens -= collateral_withdraw_amount;
    Obligation::pack(obligation, &mut obligation_info.data.borrow_mut())?;

    let (lending_market_authority_pubkey, bump_seed) =
        Pubkey::find_program_address(&[lending_market_info.key.as_ref()], program_id);
    if lending_market_authority_info.key != &lending_market_authority_pubkey {
        return Err(LendingError::InvalidMarketAuthority.into());
    }
    let authority_signer_seeds = &[lending_market_info.key.as_ref(), &[bump_seed]];

    // deposit repaid liquidity
    spl_token_transfer(TokenTransferParams {
        source: source_liquidity_info.clone(),
        destination: repay_reserve_liquidity_supply_info.clone(),
        amount: rounded_repay_amount,
        authority: user_transfer_authority_info.clone(),
        authority_signer_seeds: &[],
        token_program: token_program_id.clone(),
    })?;

    // withdraw collateral
    spl_token_transfer(TokenTransferParams {
        source: withdraw_reserve_collateral_supply_info.clone(),
        destination: destination_collateral_info.clone(),
        amount: collateral_withdraw_amount,
        authority: lending_market_authority_info.clone(),
        authority_signer_seeds,
        token_program: token_program_id.clone(),
    })?;

    Ok(())
}

fn assert_rent_exempt(rent: &Rent, account_info: &AccountInfo) -> ProgramResult {
    if !rent.is_exempt(account_info.lamports(), account_info.data_len()) {
        msg!(&rent.minimum_balance(account_info.data_len()).to_string());
        Err(LendingError::NotRentExempt.into())
    } else {
        Ok(())
    }
}

fn assert_uninitialized<T: Pack + IsInitialized>(
    account_info: &AccountInfo,
) -> Result<T, ProgramError> {
    let account: T = T::unpack_unchecked(&account_info.data.borrow())?;
    if account.is_initialized() {
        Err(LendingError::AlreadyInitialized.into())
    } else {
        Ok(account)
    }
}

/// Unpacks a spl_token `Mint`.
fn unpack_mint(data: &[u8]) -> Result<spl_token::state::Mint, LendingError> {
    spl_token::state::Mint::unpack(data).map_err(|_| LendingError::InvalidTokenMint)
}

/// Issue a spl_token `InitializeMint` instruction.
#[inline(always)]
fn spl_token_init_mint(params: TokenInitializeMintParams<'_, '_>) -> ProgramResult {
    let TokenInitializeMintParams {
        mint,
        rent,
        authority,
        token_program,
        decimals,
    } = params;
    let ix = spl_token::instruction::initialize_mint(
        token_program.key,
        mint.key,
        authority,
        None,
        decimals,
    )?;
    let result = invoke(&ix, &[mint, rent, token_program]);
    result.map_err(|_| LendingError::TokenInitializeMintFailed.into())
}

/// Issue a spl_token `InitializeAccount` instruction.
#[inline(always)]
fn spl_token_init_account(params: TokenInitializeAccountParams<'_>) -> ProgramResult {
    let TokenInitializeAccountParams {
        account,
        mint,
        owner,
        rent,
        token_program,
    } = params;
    let ix = spl_token::instruction::initialize_account(
        token_program.key,
        account.key,
        mint.key,
        owner.key,
    )?;
    let result = invoke(&ix, &[account, mint, owner, rent, token_program]);
    result.map_err(|_| LendingError::TokenInitializeAccountFailed.into())
}

/// Issue a spl_token `Transfer` instruction.
#[inline(always)]
fn spl_token_transfer(params: TokenTransferParams<'_, '_>) -> ProgramResult {
    let TokenTransferParams {
        source,
        destination,
        authority,
        token_program,
        amount,
        authority_signer_seeds,
    } = params;
    let result = invoke_signed(
        &spl_token::instruction::transfer(
            token_program.key,
            source.key,
            destination.key,
            authority.key,
            &[],
            amount,
        )?,
        &[source, destination, authority, token_program],
        &[authority_signer_seeds],
    );
    result.map_err(|_| LendingError::TokenTransferFailed.into())
}

/// Issue a spl_token `MintTo` instruction.
fn spl_token_mint_to(params: TokenMintToParams<'_, '_>) -> ProgramResult {
    let TokenMintToParams {
        mint,
        destination,
        authority,
        token_program,
        amount,
        authority_signer_seeds,
    } = params;
    let result = invoke_signed(
        &spl_token::instruction::mint_to(
            token_program.key,
            mint.key,
            destination.key,
            authority.key,
            &[],
            amount,
        )?,
        &[mint, destination, authority, token_program],
        &[authority_signer_seeds],
    );
    result.map_err(|_| LendingError::TokenMintToFailed.into())
}

/// Issue a spl_token `Burn` instruction.
#[inline(always)]
fn spl_token_burn(params: TokenBurnParams<'_, '_>) -> ProgramResult {
    let TokenBurnParams {
        mint,
        source,
        authority,
        token_program,
        amount,
        authority_signer_seeds,
    } = params;
    let result = invoke_signed(
        &spl_token::instruction::burn(
            token_program.key,
            source.key,
            mint.key,
            authority.key,
            &[],
            amount,
        )?,
        &[source, mint, authority, token_program],
        &[authority_signer_seeds],
    );
    result.map_err(|_| LendingError::TokenBurnFailed.into())
}

struct TokenInitializeMintParams<'a: 'b, 'b> {
    mint: AccountInfo<'a>,
    rent: AccountInfo<'a>,
    authority: &'b Pubkey,
    decimals: u8,
    token_program: AccountInfo<'a>,
}

struct TokenInitializeAccountParams<'a> {
    account: AccountInfo<'a>,
    mint: AccountInfo<'a>,
    owner: AccountInfo<'a>,
    rent: AccountInfo<'a>,
    token_program: AccountInfo<'a>,
}

struct TokenTransferParams<'a: 'b, 'b> {
    source: AccountInfo<'a>,
    destination: AccountInfo<'a>,
    amount: u64,
    authority: AccountInfo<'a>,
    authority_signer_seeds: &'b [&'b [u8]],
    token_program: AccountInfo<'a>,
}

struct TokenMintToParams<'a: 'b, 'b> {
    mint: AccountInfo<'a>,
    destination: AccountInfo<'a>,
    amount: u64,
    authority: AccountInfo<'a>,
    authority_signer_seeds: &'b [&'b [u8]],
    token_program: AccountInfo<'a>,
}

struct TokenBurnParams<'a: 'b, 'b> {
    mint: AccountInfo<'a>,
    source: AccountInfo<'a>,
    amount: u64,
    authority: AccountInfo<'a>,
    authority_signer_seeds: &'b [&'b [u8]],
    token_program: AccountInfo<'a>,
}

impl PrintProgramError for LendingError {
    fn print<E>(&self)
    where
        E: 'static + std::error::Error + DecodeError<E> + PrintProgramError + FromPrimitive,
    {
        msg!(&self.to_string());
    }
}
