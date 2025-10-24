use anchor_lang::{prelude::AccountMeta, Id, InstructionData, ToAccountMetas};

use marginfi::accounts::KaminoWithdraw;
use solana_sdk::{instruction::Instruction, pubkey::Pubkey, sysvar};

use crate::{
    cache::KaminoReserve,
    kamino_farms::program::Farms as KaminoFarms,
    kamino_lending::{client as kamino, program::KaminoLending},
    utils::find_bank_liquidity_vault_authority,
    wrappers::{bank::BankWrapper, mint::MintWrapper},
};

pub fn make_refresh_reserve_ix(
    kamino_reserve_address: Pubkey,
    kamino_reserve: &KaminoReserve,
) -> Instruction {
    let (pyth_oracle, switchboard_price_oracle, switchboard_twap_oracle, scope_prices) =
        get_oracle_setup(kamino_reserve);
    let accounts = kamino::accounts::RefreshReserve {
        reserve: kamino_reserve_address,
        lending_market: kamino_reserve.reserve.lending_market,
        pyth_oracle,
        switchboard_price_oracle,
        switchboard_twap_oracle,
        scope_prices,
    }
    .to_account_metas(None);

    Instruction {
        program_id: KaminoLending::id(),
        accounts,
        data: kamino::args::RefreshReserve.data(),
    }
}

pub fn make_refresh_obligation_ix(
    obligation: Pubkey,
    lending_market: Pubkey,
    remaining: &[Pubkey],
) -> Instruction {
    let mut accounts = kamino::accounts::RefreshObligation {
        lending_market,
        obligation,
    }
    .to_account_metas(None);

    accounts.extend(
        remaining
            .iter()
            .map(|a| AccountMeta::new_readonly(*a, false)),
    );

    Instruction {
        program_id: KaminoLending::id(),
        accounts,
        data: kamino::args::RefreshObligation.data(),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn make_kamino_withdraw_ix(
    marginfi_program_id: Pubkey,
    group: Pubkey,
    marginfi_account: Pubkey,
    authority: Pubkey,
    bank: &BankWrapper,
    mint_wrapper: &MintWrapper,
    kamino_obligation: Pubkey,
    kamino_reserve: &KaminoReserve,
    remaining: &[Pubkey],
    amount: u64,
    withdraw_all: bool,
) -> Instruction {
    let mut accounts = KaminoWithdraw {
        group,
        lending_market: kamino_reserve.reserve.lending_market,
        marginfi_account,
        authority,
        bank: bank.address,
        destination_token_account: mint_wrapper.token,
        liquidity_vault_authority: find_bank_liquidity_vault_authority(
            &bank.address,
            &marginfi_program_id,
        ),
        liquidity_vault: bank.bank.liquidity_vault,
        kamino_obligation,
        lending_market_authority: kamino_reserve.lending_market_authority,
        kamino_reserve: kamino_reserve.address,
        reserve_liquidity_mint: kamino_reserve.reserve.liquidity.mint_pubkey,
        reserve_liquidity_supply: kamino_reserve.reserve.liquidity.supply_vault,
        reserve_collateral_mint: kamino_reserve.reserve.collateral.mint_pubkey,
        reserve_source_collateral: kamino_reserve.reserve.collateral.supply_vault,
        obligation_farm_user_state: None,
        reserve_farm_state: Some(kamino_reserve.reserve.farm_collateral),
        kamino_program: KaminoLending::id(),
        farms_program: KaminoFarms::id(),
        collateral_token_program: mint_wrapper.account.owner, // assuming Kamino liquidity and collateral are the same as our bank's mint
        liquidity_token_program: mint_wrapper.account.owner, // assuming Kamino liquidity and collateral are the same as our bank's mint
        instruction_sysvar_account: sysvar::instructions::id(),
    }
    .to_account_metas(None);

    accounts.extend(
        remaining
            .iter()
            .map(|a| AccountMeta::new_readonly(*a, false)),
    );

    Instruction {
        program_id: KaminoLending::id(),
        accounts,
        data: marginfi::instruction::KaminoWithdraw {
            amount,
            withdraw_all: Some(withdraw_all),
        }
        .data(),
    }
}

fn get_oracle_setup(
    reserve: &KaminoReserve,
) -> (
    Option<Pubkey>,
    Option<Pubkey>,
    Option<Pubkey>,
    Option<Pubkey>,
) {
    (
        none_if_default(reserve.reserve.config.token_info.pyth_configuration.price),
        none_if_default(
            reserve
                .reserve
                .config
                .token_info
                .switchboard_configuration
                .price_aggregator,
        ),
        none_if_default(
            reserve
                .reserve
                .config
                .token_info
                .switchboard_configuration
                .twap_aggregator,
        ),
        none_if_default(
            reserve
                .reserve
                .config
                .token_info
                .scope_configuration
                .price_feed,
        ),
    )
}

fn none_if_default(pk: Pubkey) -> Option<Pubkey> {
    (pk != Pubkey::default()).then_some(pk)
}
