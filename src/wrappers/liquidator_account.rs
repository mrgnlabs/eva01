use super::{bank::BankWrapper, marginfi_account::MarginfiAccountWrapper};
use crate::{
    cache::Cache,
    cli::setup::marginfi_account_by_authority,
    config::GeneralConfig,
    marginfi_ixs::{
        initialize_marginfi_account, make_deposit_ix, make_liquidate_ix, make_repay_ix,
        make_withdraw_ix,
    },
    metrics::LIQUIDATION_ATTEMPTS,
    thread_debug, thread_info,
    utils::{check_asset_tags_matching, swb_cranker::is_stale_swb_price_error},
    wrappers::oracle::OracleWrapper,
};
use anyhow::{anyhow, Result};
use log::info;
use solana_client::{rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig};

use solana_program::pubkey::Pubkey;
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    message::{v0::Message, CompileError, VersionedMessage},
    pubkey,
    signature::{read_keypair_file, Keypair},
    signer::{Signer, SignerError},
    system_instruction::transfer,
    transaction::VersionedTransaction,
};
use std::{sync::Arc, thread, time::Duration};

#[derive(Debug)]
pub struct LiquidationError {
    pub error: anyhow::Error,
    pub keys: Vec<Pubkey>,
}

impl LiquidationError {
    pub fn from_anyhow_error(error: anyhow::Error) -> Self {
        Self {
            error,
            keys: vec![],
        }
    }

    pub fn from_anyhow_error_with_keys(error: anyhow::Error, keys: Vec<Pubkey>) -> Self {
        Self { error, keys }
    }
    pub fn from_compile_error(error: CompileError) -> Self {
        Self {
            error: anyhow!("{:?}", error),
            keys: vec![],
        }
    }

    pub fn from_signer_error(error: SignerError) -> Self {
        Self {
            error: anyhow!("{:?}", error),
            keys: vec![],
        }
    }
}

pub struct LiquidatorAccount {
    pub liquidator_address: Pubkey,
    pub signer_keypair: Arc<Keypair>,
    program_id: Pubkey,
    group: Pubkey,
    rpc_client: RpcClient,
    compute_unit_limit: u32,
    pub cache: Arc<Cache>,
}

impl LiquidatorAccount {
    pub fn new(
        config: &GeneralConfig,
        marginfi_group_id: Pubkey,
        cache: Arc<Cache>,
        new_liquidator_account: &mut Option<Pubkey>,
    ) -> Result<Self> {
        let signer_keypair = read_keypair_file(&config.keypair_path).unwrap();
        let rpc_client = RpcClient::new(config.rpc_url.clone());

        let mut accounts = marginfi_account_by_authority(
            signer_keypair.pubkey(),
            &rpc_client,
            config.marginfi_program_id,
            marginfi_group_id,
        )?;
        info!(
            "Found {} MarginFi accounts for the provided signer",
            accounts.len()
        );
        for account in accounts.iter() {
            info!("MarginFi account: {:?}", account);
        }
        accounts.clear();

        let liquidator_address = if accounts.is_empty() {
            thread_info!("No MarginFi account found for the provided signer. Creating it...");

            let liquidator_marginfi_account = initialize_marginfi_account(
                &rpc_client,
                config.marginfi_program_id,
                marginfi_group_id,
                &signer_keypair,
            )?;

            thread::sleep(Duration::from_secs(20));

            *new_liquidator_account = Some(liquidator_marginfi_account);

            liquidator_marginfi_account
        } else {
            accounts[0]
        };

        Ok(Self {
            liquidator_address,
            signer_keypair: Arc::new(signer_keypair),
            program_id: config.marginfi_program_id,
            group: marginfi_group_id,
            rpc_client,
            compute_unit_limit: config.compute_unit_limit,
            cache,
        })
    }

    pub fn clone(&self) -> Result<Self> {
        let rpc_url = self.rpc_client.url();
        let rpc_client = RpcClient::new(rpc_url.clone());

        Ok(Self {
            liquidator_address: self.liquidator_address,
            signer_keypair: Arc::clone(&self.signer_keypair),
            program_id: self.program_id,
            group: self.group,
            rpc_client,
            compute_unit_limit: self.compute_unit_limit,
            cache: Arc::clone(&self.cache),
        })
    }

    pub fn liquidate(
        &mut self,
        liquidatee_account: &MarginfiAccountWrapper,
        asset_bank: &Pubkey,
        liab_bank: &Pubkey,
        asset_amount: u64,
    ) -> Result<(), LiquidationError> {
        let liquidatee_account_address = liquidatee_account.address;
        thread_info!(
            "Liquidating account {:?} with liquidator account {:?}. Amount: {}",
            liquidatee_account_address,
            self.liquidator_address,
            asset_amount
        );

        let asset_bank_wrapper = self
            .cache
            .try_get_bank_wrapper(asset_bank)
            .map_err(LiquidationError::from_anyhow_error)?;
        let liab_bank_wrapper = self
            .cache
            .try_get_bank_wrapper(liab_bank)
            .map_err(LiquidationError::from_anyhow_error)?;

        let signer_pk = self.signer_keypair.pubkey();
        let liab_mint = liab_bank_wrapper.bank.mint;

        let lending_account = &self
            .cache
            .marginfi_accounts
            .try_get_account(&self.liquidator_address)
            .map_err(LiquidationError::from_anyhow_error)?
            .lending_account;

        let banks_to_include: Vec<Pubkey> = vec![*liab_bank, *asset_bank];

        for bank_pk in banks_to_include.iter() {
            let bank_to_validate_against = self
                .cache
                .banks
                .try_get_bank(bank_pk)
                .map_err(LiquidationError::from_anyhow_error)?;
            if !check_asset_tags_matching(&bank_to_validate_against, lending_account) {
                // This is a precaution to not attempt to liquidate staked collateral positions when liquidator has non-SOL positions open.
                // Expected to happen quite often for now. Later on, we can add a more sophisticated filtering logic on the higher level.
                thread_debug!("Bank {:?} does not match the asset tags of the lending account -> skipping liquidation attempt", bank_pk);
                return Ok(());
            }
        }

        LIQUIDATION_ATTEMPTS.inc();

        let banks_to_exclude: Vec<Pubkey> = vec![];
        let (liquidator_observation_accounts, liquidator_swb_oracles) =
            MarginfiAccountWrapper::get_observation_accounts::<OracleWrapper>(
                lending_account,
                &banks_to_include,
                &banks_to_exclude,
                self.cache.clone(),
            )
            .map_err(LiquidationError::from_anyhow_error)?;
        thread_debug!(
            "The Liquidator {} observation accounts: {:?}",
            &self.liquidator_address,
            liquidator_observation_accounts
        );

        let banks_to_include: Vec<Pubkey> = vec![];
        let banks_to_exclude: Vec<Pubkey> = vec![];
        let (liquidatee_observation_accounts, liquidatee_swb_oracles) =
            MarginfiAccountWrapper::get_observation_accounts::<OracleWrapper>(
                &liquidatee_account.lending_account,
                &banks_to_include,
                &banks_to_exclude,
                self.cache.clone(),
            )
            .map_err(LiquidationError::from_anyhow_error)?;
        thread_debug!(
            "The Liquidatee {:?} observation accounts: {:?}",
            liquidatee_account_address,
            liquidatee_observation_accounts
        );

        let joined_observation_accounts = liquidator_observation_accounts
            .iter()
            .chain(liquidatee_observation_accounts.iter())
            .copied()
            .collect::<Vec<_>>();

        let cu_limit_ix = ComputeBudgetInstruction::set_compute_unit_limit(self.compute_unit_limit);

        let total_observation_accounts = joined_observation_accounts.len();

        let liquidate_ix = make_liquidate_ix(
            self.program_id,
            self.group,
            self.liquidator_address,
            &asset_bank_wrapper,
            &liab_bank_wrapper,
            signer_pk,
            liquidatee_account_address,
            self.cache
                .mints
                .try_get_account(&liab_mint)
                .map_err(LiquidationError::from_anyhow_error)?
                .account
                .owner,
            joined_observation_accounts,
            asset_amount,
        );

        let recent_blockhash = self
            .rpc_client
            .get_latest_blockhash()
            .map_err(|e| LiquidationError::from_anyhow_error(anyhow!(e)))?;

        // Use LUTs only when your transaction involves a large number of observation accounts.
        let luts: &Vec<AddressLookupTableAccount> = {
            if total_observation_accounts > 22 {
                thread_debug!(
                    "Using LUT for liquidating the Account {} .",
                    liquidatee_account_address
                );
                &self.cache.luts
            } else {
                &vec![]
            }
        };

        let msg = Message::try_compile(
            &signer_pk,
            &[cu_limit_ix.clone(), liquidate_ix.clone()],
            luts,
            recent_blockhash,
        )
        .map_err(LiquidationError::from_compile_error)?;

        let txn = VersionedTransaction::try_new(VersionedMessage::V0(msg), &[&self.signer_keypair])
            .map_err(LiquidationError::from_signer_error)?;

        thread_info!(
            "Sending liquidation txn for the Account {} .",
            liquidatee_account_address
        );
        match self
            .rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &txn,
                CommitmentConfig::confirmed(),
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    ..Default::default()
                },
            ) {
            Ok(signature) => {
                thread_info!(
                    "Liquidation txn for the Account {} was confirmed. Signature: {}",
                    liquidatee_account_address,
                    signature,
                );
                Ok(())
            }
            Err(err) => {
                let mut swb_oracles: Vec<Pubkey> = vec![];
                if is_stale_swb_price_error(&err) {
                    swb_oracles = liquidator_swb_oracles;
                    for swb_oracle in liquidatee_swb_oracles.into_iter() {
                        if !swb_oracles.contains(&swb_oracle) {
                            swb_oracles.push(swb_oracle);
                        }
                    }
                }
                Err(LiquidationError::from_anyhow_error_with_keys(
                    anyhow!(
                        "Liquidation txn for the Account {} failed: {} ",
                        liquidatee_account_address,
                        err
                    ),
                    swb_oracles,
                ))
            }
        }
    }

    pub fn withdraw(
        &self,
        bank: &BankWrapper,
        amount: u64,
        withdraw_all: Option<bool>,
    ) -> Result<()> {
        let marginfi_account = self.liquidator_address;

        let signer_pk = self.signer_keypair.pubkey();

        let banks_to_include: Vec<Pubkey> = vec![];
        let banks_to_exclude = if withdraw_all.unwrap_or(false) {
            vec![bank.address]
        } else {
            vec![]
        };
        thread_debug!("Collecting observation accounts for the account: {:?} with banks_to_include {:?} and banks_to_exclude {:?}", 
        &self.liquidator_address, &banks_to_include, &banks_to_exclude);
        let (observation_accounts, _) =
            MarginfiAccountWrapper::get_observation_accounts::<OracleWrapper>(
                &self
                    .cache
                    .marginfi_accounts
                    .try_get_account(&self.liquidator_address)?
                    .lending_account,
                &banks_to_include,
                &banks_to_exclude,
                self.cache.clone(),
            )?;

        let mint = bank.bank.mint;
        let token_account = self.cache.tokens.try_get_token_for_mint(&mint)?;
        let withdraw_ix = make_withdraw_ix(
            self.program_id,
            self.group,
            marginfi_account,
            signer_pk,
            bank,
            token_account,
            self.cache.mints.try_get_account(&mint)?.account.owner,
            observation_accounts,
            amount,
            withdraw_all,
        );

        let recent_blockhash = self.rpc_client.get_latest_blockhash()?;

        let tx: solana_sdk::transaction::Transaction =
            solana_sdk::transaction::Transaction::new_signed_with_payer(
                &[withdraw_ix.clone()],
                Some(&signer_pk),
                &[&self.signer_keypair],
                recent_blockhash,
            );

        thread_debug!("Withdrawing {:?} from {:?}", amount, token_account);

        let res = self
            .rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &tx,
                CommitmentConfig::confirmed(),
                RpcSendTransactionConfig {
                    skip_preflight: true,
                    ..Default::default()
                },
            )
            .map_err(|e| anyhow::anyhow!(e))?;

        thread_debug!(
            "Withdrawing result for Liquidator account {:?} (without preflight check): {:?} ",
            marginfi_account,
            res
        );
        Ok(())
    }

    pub fn repay(&self, bank: &BankWrapper, amount: u64, repay_all: Option<bool>) -> Result<()> {
        let marginfi_account = self.liquidator_address;

        let signer_pk = self.signer_keypair.pubkey();

        let mint = bank.bank.mint;
        let token_account = self.cache.tokens.try_get_token_for_mint(&mint)?;
        let repay_ix = make_repay_ix(
            self.program_id,
            self.group,
            marginfi_account,
            signer_pk,
            bank,
            token_account,
            self.cache.mints.try_get_account(&mint)?.account.owner,
            amount,
            repay_all,
        );

        let recent_blockhash = self.rpc_client.get_latest_blockhash()?;

        let tx: solana_sdk::transaction::Transaction =
            solana_sdk::transaction::Transaction::new_signed_with_payer(
                &[repay_ix.clone()],
                Some(&signer_pk),
                &[&self.signer_keypair],
                recent_blockhash,
            );

        thread_debug!("Repaying {:?}, token account {:?}", amount, token_account);

        let res = self
            .rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &tx,
                CommitmentConfig::confirmed(),
                RpcSendTransactionConfig {
                    skip_preflight: true,
                    ..Default::default()
                },
            );
        thread_debug!(
            "Repaying result for account {:?} (without preflight check): {:?} ",
            marginfi_account,
            res
        );

        Ok(())
    }

    pub fn deposit(&self, bank: &BankWrapper, amount: u64) -> Result<()> {
        let marginfi_account = self.liquidator_address;

        let signer_pk = self.signer_keypair.pubkey();

        let mint = bank.bank.mint;
        let token_account = self.cache.tokens.try_get_token_for_mint(&mint)?;
        let deposit_ix = make_deposit_ix(
            self.program_id,
            self.group,
            marginfi_account,
            signer_pk,
            bank,
            token_account,
            self.cache.mints.try_get_account(&mint)?.account.owner,
            amount,
        );

        let recent_blockhash = self.rpc_client.get_latest_blockhash()?;

        let instructions: Vec<Instruction> =
            if mint == pubkey!("So11111111111111111111111111111111111111112") {
                vec![transfer(&signer_pk, &token_account, amount), deposit_ix]
            } else {
                vec![deposit_ix]
            };

        let tx: solana_sdk::transaction::Transaction =
            solana_sdk::transaction::Transaction::new_signed_with_payer(
                &instructions,
                Some(&signer_pk),
                &[&self.signer_keypair],
                recent_blockhash,
            );

        thread_debug!("Depositing {:?}, token account {:?}", amount, token_account);

        let res = self
            .rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &tx,
                CommitmentConfig::confirmed(),
                RpcSendTransactionConfig {
                    skip_preflight: true,
                    ..Default::default()
                },
            );
        thread_debug!(
            "Depositing result for account {:?} (without preflight check): {:?} ",
            marginfi_account,
            res
        );

        Ok(())
    }
}
