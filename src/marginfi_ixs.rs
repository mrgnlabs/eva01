use anchor_lang::{InstructionData, Key, ToAccountMetas};

use anchor_spl::token_2022;
use log::{debug, info, trace};
use marginfi_type_crate::constants::LIQUIDATION_RECORD_SEED;
use solana_client::{rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig};
use solana_sdk::{
    address_lookup_table,
    commitment_config::CommitmentConfig,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    system_program, sysvar,
};

use crate::{
    utils::find_bank_liquidity_vault_authority,
    wrappers::{bank::BankWrapper, mint::MintWrapper},
};

pub fn make_init_liquidation_record_ix(
    marginfi_program_id: Pubkey,
    liquidatee_account: Pubkey,
    fee_payer: Pubkey,
) -> Instruction {
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

    Instruction {
        program_id: marginfi_program_id,
        accounts,
        data: marginfi::instruction::MarginfiAccountInitLiqRecord.data(),
    }
}

pub fn make_start_liquidate_ix(
    marginfi_program_id: Pubkey,
    liquidatee_account: Pubkey,
    liquidator_account: Pubkey,
    liquidation_record: Pubkey,
    observation_accounts: &[Pubkey],
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
