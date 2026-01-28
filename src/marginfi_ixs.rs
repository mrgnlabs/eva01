use std::collections::HashSet;

use anchor_lang::{Id, InstructionData, Key, ToAccountMetas};

use anchor_spl::token_2022;
use log::{debug, info, trace};
use marginfi_type_crate::constants::LIQUIDATION_RECORD_SEED;
use solana_client::{rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    system_program, sysvar,
};

use crate::{
    cache::{DriftSpotMarket, KaminoReserve},
    drift::program::Drift,
    kamino_farms::program::Farms as KaminoFarms,
    kamino_lending::program::KaminoLending,
    utils::{
        drift::{derive_drift_signer, derive_drift_state, derive_spot_market_vault},
        find_bank_liquidity_vault_authority,
        kamino::derive_user_state,
    },
    wrappers::{bank::BankWrapper, mint::MintWrapper},
};

pub fn make_init_liquidation_record_ix(
    marginfi_program_id: Pubkey,
    liquidatee_account: Pubkey,
    fee_payer: Pubkey,
) -> (Instruction, Pubkey) {
    let (liquidation_record, _bump) = Pubkey::find_program_address(
        &[
            LIQUIDATION_RECORD_SEED.as_bytes(),
            liquidatee_account.as_ref(),
        ],
        &marginfi_program_id,
    );
    let mut accounts = marginfi::accounts::InitLiquidationRecord {
        marginfi_account: liquidatee_account,
        fee_payer,
        liquidation_record,
        system_program: system_program::id(),
    }
    .to_account_metas(None);
    mark_signer(&mut accounts, fee_payer);

    (
        Instruction {
            program_id: marginfi_program_id,
            accounts,
            data: marginfi::instruction::MarginfiAccountInitLiqRecord.data(),
        },
        liquidation_record,
    )
}

pub fn make_start_liquidate_ix(
    marginfi_program_id: Pubkey,
    liquidatee_account: Pubkey,
    liquidator_account: Pubkey,
    liquidation_record: Pubkey,
    observation_accounts: &[Pubkey],
    participating_accounts: &mut HashSet<Pubkey>,
) -> Instruction {
    let mut accounts = marginfi::accounts::StartLiquidation {
        marginfi_account: liquidatee_account,
        liquidation_receiver: liquidator_account,
        liquidation_record,
        instruction_sysvar: sysvar::instructions::id(),
    }
    .to_account_metas(None);
    mark_signer(&mut accounts, liquidator_account);

    accounts.extend(
        observation_accounts
            .iter()
            .map(|a| AccountMeta::new_readonly(a.key(), false)),
    );

    participating_accounts.extend(accounts.iter().map(|a| a.pubkey));

    Instruction {
        program_id: marginfi_program_id,
        accounts,
        data: marginfi::instruction::StartLiquidation.data(),
    }
}

pub fn make_deposit_ix(
    marginfi_program_id: Pubkey,
    marginfi_group: Pubkey,
    marginfi_account: Pubkey,
    signer: Pubkey,
    bank: &BankWrapper,
    signer_token_account: Pubkey,
    token_program: Pubkey,
    amount: u64,
) -> Instruction {
    let mut accounts = marginfi::accounts::LendingAccountDeposit {
        marginfi_account,
        authority: signer,
        signer_token_account,
        liquidity_vault: bank.bank.liquidity_vault,
        token_program,
        bank: bank.address,
        group: marginfi_group,
    }
    .to_account_metas(None);
    maybe_add_bank_mint(&mut accounts, bank.bank.mint, &token_program);
    mark_signer(&mut accounts, signer);

    Instruction {
        program_id: marginfi_program_id,
        accounts,
        data: marginfi::instruction::LendingAccountDeposit {
            amount,
            deposit_up_to_limit: None,
        }
        .data(),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn make_repay_ix(
    marginfi_program_id: Pubkey,
    marginfi_group: Pubkey,
    marginfi_account: Pubkey,
    signer: Pubkey,
    bank: &BankWrapper,
    mint_wrapper: &MintWrapper,
    amount: u64,
    repay_all: bool,
    participating_accounts: &mut HashSet<Pubkey>,
) -> Instruction {
    let mut accounts = marginfi::accounts::LendingAccountRepay {
        marginfi_account,
        authority: signer,
        signer_token_account: mint_wrapper.token,
        liquidity_vault: bank.bank.liquidity_vault,
        token_program: mint_wrapper.account.owner,
        bank: bank.address,
        group: marginfi_group,
    }
    .to_account_metas(None);
    maybe_add_bank_mint(&mut accounts, bank.bank.mint, &mint_wrapper.account.owner);
    mark_signer(&mut accounts, signer);

    participating_accounts.extend(accounts.iter().map(|a| a.pubkey));

    Instruction {
        program_id: marginfi_program_id,
        accounts,
        data: marginfi::instruction::LendingAccountRepay {
            amount,
            repay_all: Some(repay_all),
        }
        .data(),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn make_withdraw_ix(
    marginfi_program_id: Pubkey,
    marginfi_group: Pubkey,
    marginfi_account: Pubkey,
    signer: Pubkey,
    bank: &BankWrapper,
    mint_wrapper: &MintWrapper,
    observation_accounts: &[Pubkey],
    amount: u64,
    withdraw_all: bool,
    participating_accounts: Option<&mut HashSet<Pubkey>>,
) -> Instruction {
    let mut accounts = marginfi::accounts::LendingAccountWithdraw {
        marginfi_account,
        destination_token_account: mint_wrapper.token,
        liquidity_vault: bank.bank.liquidity_vault,
        token_program: mint_wrapper.account.owner,
        authority: signer,
        bank_liquidity_vault_authority: find_bank_liquidity_vault_authority(
            &bank.address,
            &marginfi_program_id,
        ),
        bank: bank.address,
        group: marginfi_group,
    }
    .to_account_metas(Some(true));
    maybe_add_bank_mint(&mut accounts, bank.bank.mint, &mint_wrapper.account.owner);
    mark_signer(&mut accounts, signer);

    trace!(
        "make_withdraw_ix: observation_accounts: {:?}",
        observation_accounts
    );

    accounts.extend(
        observation_accounts
            .iter()
            .map(|a| AccountMeta::new_readonly(a.key(), false)),
    );

    if let Some(participating_accounts) = participating_accounts {
        participating_accounts.extend(accounts.iter().map(|a| a.pubkey));
    }

    Instruction {
        program_id: marginfi_program_id,
        accounts,
        data: marginfi::instruction::LendingAccountWithdraw {
            amount,
            withdraw_all: Some(withdraw_all),
        }
        .data(),
    }
}

pub fn make_end_liquidate_ix(
    marginfi_program_id: Pubkey,
    liquidatee_account: Pubkey,
    liquidator_account: Pubkey,
    liquidation_record: Pubkey,
    fee_state: Pubkey,
    global_fee_wallet: Pubkey,
    observation_accounts: &[Pubkey],
    participating_accounts: &mut HashSet<Pubkey>,
) -> Instruction {
    let mut accounts = marginfi::accounts::EndLiquidation {
        marginfi_account: liquidatee_account,
        liquidation_receiver: liquidator_account,
        liquidation_record,
        fee_state,
        global_fee_wallet,
        system_program: system_program::id(),
    }
    .to_account_metas(None);
    mark_signer(&mut accounts, liquidator_account);

    accounts.extend(
        observation_accounts
            .iter()
            .map(|a| AccountMeta::new_readonly(a.key(), false)),
    );

    participating_accounts.extend(accounts.iter().map(|a| a.pubkey));

    Instruction {
        program_id: marginfi_program_id,
        accounts,
        data: marginfi::instruction::EndLiquidation.data(),
    }
}

fn maybe_add_bank_mint(accounts: &mut Vec<AccountMeta>, mint: Pubkey, token_program: &Pubkey) {
    if token_program == &token_2022::ID {
        debug!("!!!Adding mint account to accounts!!!");
        accounts.push(AccountMeta::new_readonly(mint, false));
    }
}

pub fn make_create_ix(
    marginfi_program_id: Pubkey,
    marginfi_group: Pubkey,
    marginfi_account: Pubkey,
    signer: Pubkey,
) -> Instruction {
    Instruction {
        program_id: marginfi_program_id,
        accounts: marginfi::accounts::MarginfiAccountInitialize {
            marginfi_group,
            marginfi_account,
            system_program: solana_sdk::system_program::ID,
            authority: signer,
            fee_payer: signer,
        }
        .to_account_metas(Some(true)),
        data: marginfi::instruction::MarginfiAccountInitialize.data(),
    }
}

fn mark_signer(
    accounts: &mut [solana_sdk::instruction::AccountMeta],
    signer: solana_sdk::pubkey::Pubkey,
) {
    for m in accounts.iter_mut() {
        m.is_signer = m.pubkey == signer;
    }
}

pub fn initialize_marginfi_account(
    rpc_client: &RpcClient,
    marginfi_program_id: Pubkey,
    marginfi_group: Pubkey,
    signer_keypair: &Keypair,
) -> anyhow::Result<Pubkey> {
    let marginfi_account_key = Keypair::new();

    let ix = make_create_ix(
        marginfi_program_id,
        marginfi_group,
        marginfi_account_key.pubkey(),
        signer_keypair.pubkey(),
    );

    let recent_blockhash = rpc_client.get_latest_blockhash()?;
    let tx = solana_sdk::transaction::Transaction::new_signed_with_payer(
        &[ix],
        Some(&signer_keypair.pubkey()),
        &[signer_keypair, &marginfi_account_key],
        recent_blockhash,
    );

    let res = rpc_client.send_and_confirm_transaction_with_spinner_and_config(
        &tx,
        CommitmentConfig::finalized(),
        RpcSendTransactionConfig {
            skip_preflight: true,
            ..Default::default()
        },
    );
    info!(
        "Initialized new Marginfi account {:?} (without preflight check): {:?} ",
        marginfi_account_key.pubkey(),
        res
    );

    Ok(marginfi_account_key.pubkey())
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
    participating_accounts: &mut HashSet<Pubkey>,
) -> Instruction {
    let (reserve_farm_state, obligation_farm_user_state) =
        if kamino_reserve.reserve.farm_collateral == Pubkey::default() {
            (Some(marginfi_program_id), Some(marginfi_program_id))
        } else {
            (
                Some(kamino_reserve.reserve.farm_collateral),
                Some(derive_user_state(
                    &kamino_reserve.reserve.farm_collateral,
                    &kamino_obligation,
                )),
            )
        };

    let mut accounts = marginfi::accounts::KaminoWithdraw {
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
        integration_acc_2: kamino_obligation,
        lending_market_authority: kamino_reserve.lending_market_authority,
        integration_acc_1: kamino_reserve.address,
        reserve_liquidity_mint: kamino_reserve.reserve.liquidity.mint_pubkey,
        reserve_liquidity_supply: kamino_reserve.reserve.liquidity.supply_vault,
        reserve_collateral_mint: kamino_reserve.reserve.collateral.mint_pubkey,
        reserve_source_collateral: kamino_reserve.reserve.collateral.supply_vault,
        obligation_farm_user_state,
        reserve_farm_state,
        kamino_program: KaminoLending::id(),
        farms_program: KaminoFarms::id(),
        collateral_token_program: mint_wrapper.account.owner, // assuming Kamino liquidity and collateral are the same as our bank's mint
        liquidity_token_program: mint_wrapper.account.owner, // assuming Kamino liquidity and collateral are the same as our bank's mint
        instruction_sysvar_account: sysvar::instructions::id(),
    }
    .to_account_metas(None);
    mark_signer(&mut accounts, authority);

    accounts.extend(
        remaining
            .iter()
            .map(|a| AccountMeta::new_readonly(*a, false)),
    );

    participating_accounts.extend(accounts.iter().map(|a| a.pubkey));

    Instruction {
        program_id: marginfi_program_id,
        accounts,
        data: marginfi::instruction::KaminoWithdraw {
            amount,
            withdraw_all: Some(withdraw_all),
        }
        .data(),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn make_drift_withdraw_ix(
    marginfi_program_id: Pubkey,
    group: Pubkey,
    marginfi_account: Pubkey,
    authority: Pubkey,
    bank: &BankWrapper,
    mint_wrapper: &MintWrapper,
    drift_spot_market: &DriftSpotMarket,
    reward_spot_market: Option<&DriftSpotMarket>,
    reward_spot_market_2: Option<&DriftSpotMarket>,
    remaining: &[Pubkey],
    amount: u64,
    withdraw_all: bool,
    participating_accounts: &mut HashSet<Pubkey>,
) -> Instruction {
    let drift_oracle = if bank.bank.mint
        == Pubkey::from_str_const("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v")
    {
        Some(marginfi_program_id)
    } else {
        Some(drift_spot_market.market.oracle)
    };

    assert_eq!(
        derive_spot_market_vault(drift_spot_market.market.market_index),
        drift_spot_market.market.vault
    );

    let mut accounts = marginfi::accounts::DriftWithdraw {
        group,
        marginfi_account,
        authority,
        bank: bank.address,
        drift_oracle,
        liquidity_vault_authority: find_bank_liquidity_vault_authority(
            &bank.address,
            &marginfi_program_id,
        ),
        liquidity_vault: bank.bank.liquidity_vault,
        destination_token_account: mint_wrapper.token,
        drift_state: derive_drift_state(),
        integration_acc_1: bank.bank.integration_acc_1, // spot market
        integration_acc_2: bank.bank.integration_acc_2, // user
        integration_acc_3: bank.bank.integration_acc_3, // user stats
        drift_spot_market_vault: drift_spot_market.market.vault,
        drift_reward_oracle: reward_spot_market
            .map(|m| m.market.oracle)
            .or(Some(marginfi_program_id)),
        drift_reward_spot_market: reward_spot_market
            .map(|m| m.address)
            .or(Some(marginfi_program_id)),
        drift_reward_mint: reward_spot_market
            .map(|m| m.market.mint)
            .or(Some(marginfi_program_id)),
        drift_reward_oracle_2: reward_spot_market_2
            .map(|m| m.market.oracle)
            .or(Some(marginfi_program_id)),
        drift_reward_spot_market_2: reward_spot_market_2
            .map(|m| m.address)
            .or(Some(marginfi_program_id)),
        drift_reward_mint_2: reward_spot_market_2
            .map(|m| m.market.mint)
            .or(Some(marginfi_program_id)),
        drift_signer: derive_drift_signer(),
        mint: bank.bank.mint,
        drift_program: Drift::id(),
        token_program: mint_wrapper.account.owner,
        system_program: system_program::id(),
    }
    .to_account_metas(None);

    mark_signer(&mut accounts, authority);

    accounts.extend(
        remaining
            .iter()
            .map(|a| AccountMeta::new_readonly(*a, false)),
    );

    participating_accounts.extend(accounts.iter().map(|a| a.pubkey));

    Instruction {
        program_id: marginfi_program_id,
        accounts,
        data: marginfi::instruction::DriftWithdraw {
            amount,
            withdraw_all: Some(withdraw_all),
        }
        .data(),
    }
}
