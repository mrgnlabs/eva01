use anchor_lang::{InstructionData, Key, ToAccountMetas};

use anchor_spl::token_2022;
use log::{debug, info, trace};
use solana_client::{rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
};

use crate::{utils::find_bank_liquidity_vault_authority, wrappers::bank::BankWrapper};

#[allow(clippy::too_many_arguments)]
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
    .to_account_metas(Some(true));

    maybe_add_bank_mint(&mut accounts, bank.bank.mint, &token_program);

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
    signer_token_account: Pubkey,
    token_program: Pubkey,
    amount: u64,
    repay_all: Option<bool>,
) -> Instruction {
    let mut accounts = marginfi::accounts::LendingAccountRepay {
        marginfi_account,
        authority: signer,
        signer_token_account,
        liquidity_vault: bank.bank.liquidity_vault,
        token_program,
        bank: bank.address,
        group: marginfi_group,
    }
    .to_account_metas(Some(true));

    maybe_add_bank_mint(&mut accounts, bank.bank.mint, &token_program);

    Instruction {
        program_id: marginfi_program_id,
        accounts,
        data: marginfi::instruction::LendingAccountRepay { amount, repay_all }.data(),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn make_withdraw_ix(
    marginfi_program_id: Pubkey,
    marginfi_group: Pubkey,
    marginfi_account: Pubkey,
    signer: Pubkey,
    bank: &BankWrapper,
    destination_token_account: Pubkey,
    token_program: Pubkey,
    observation_accounts: Vec<Pubkey>,
    amount: u64,
    withdraw_all: Option<bool>,
) -> Instruction {
    let mut accounts = marginfi::accounts::LendingAccountWithdraw {
        marginfi_account,
        destination_token_account,
        liquidity_vault: bank.bank.liquidity_vault,
        token_program,
        authority: signer,
        bank_liquidity_vault_authority: find_bank_liquidity_vault_authority(
            &bank.address,
            &marginfi_program_id,
        ),
        bank: bank.address,
        group: marginfi_group,
    }
    .to_account_metas(Some(true));

    maybe_add_bank_mint(&mut accounts, bank.bank.mint, &token_program);

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
            withdraw_all,
        }
        .data(),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn make_liquidate_ix(
    marginfi_program_id: Pubkey,
    marginfi_group: Pubkey,
    marginfi_account: Pubkey,
    asset_bank: &BankWrapper,
    liab_bank: &BankWrapper,
    signer: Pubkey,
    liquidatee_marginfi_account: Pubkey,
    token_program: Pubkey,
    observation_accounts: Vec<Pubkey>,
    asset_amount: u64,
) -> Instruction {
    let accounts_raw = marginfi::accounts::LendingAccountLiquidate {
        group: marginfi_group,
        asset_bank: asset_bank.address,
        liab_bank: liab_bank.address,
        liquidator_marginfi_account: marginfi_account,
        authority: signer,
        liquidatee_marginfi_account,
        bank_liquidity_vault_authority: find_bank_liquidity_vault_authority(
            &liab_bank.address,
            &marginfi_program_id,
        ),
        bank_liquidity_vault: liab_bank.bank.liquidity_vault,
        bank_insurance_vault: liab_bank.bank.insurance_vault,
        token_program,
    };
    let mut accounts = accounts_raw.to_account_metas(Some(true));

    info!(
        "LendingAccountLiquidate {{ group: {:?}, liquidator_marginfi_account: {:?}, signer: {:?}, liquidatee_marginfi_account: {:?}, bank_liquidity_vault_authority: {:?}, bank_liquidity_vault: {:?}, bank_insurance_vault: {:?}, token_program: {:?}, asset_bank: {:?}, liab_bank: {:?} }}",
        accounts_raw.group,
        accounts_raw.liquidator_marginfi_account,
        accounts_raw.authority,
        accounts_raw.liquidatee_marginfi_account,
        accounts_raw.bank_liquidity_vault_authority,
        accounts_raw.bank_liquidity_vault,
        accounts_raw.bank_insurance_vault,
        accounts_raw.token_program,
        accounts_raw.asset_bank,
        accounts_raw.liab_bank
    );
    maybe_add_bank_mint(&mut accounts, liab_bank.bank.mint, &token_program);

    accounts.extend([
        AccountMeta::new_readonly(asset_bank.oracle_adapter.address, false),
        AccountMeta::new_readonly(liab_bank.oracle_adapter.address, false),
    ]);

    accounts.extend(
        observation_accounts
            .iter()
            .map(|a| AccountMeta::new_readonly(a.key(), false)),
    );

    Instruction {
        program_id: marginfi_program_id,
        accounts,
        data: marginfi::instruction::LendingAccountLiquidate { asset_amount }.data(),
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
        CommitmentConfig::confirmed(),
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
