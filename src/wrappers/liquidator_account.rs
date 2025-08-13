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
    thread_debug, thread_info, thread_warn,
    utils::{check_asset_tags_matching, swb_cranker::is_stale_swb_price_error},
    wrappers::oracle::OracleWrapper,
};
use anyhow::{anyhow, Result};
use marginfi::state::marginfi_account::BalanceSide;
use solana_client::{rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig};

use crate::wrappers::oracle::OracleWrapperTrait;
use solana_program::pubkey::Pubkey;
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    commitment_config::{CommitmentConfig, CommitmentLevel},
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    message::{v0::Message, CompileError, VersionedMessage},
    pubkey,
    signature::Keypair,
    signer::{Signer, SignerError},
    system_instruction::transfer,
    transaction::VersionedTransaction,
};
use std::{collections::HashSet, sync::Arc, thread, time::Duration};

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
    pub signer: Keypair,
    program_id: Pubkey,
    group: Pubkey,
    preferred_mint_bank: Pubkey,
    rpc_client: RpcClient,
    cu_limit_ix: Instruction,
    pub cache: Arc<Cache>,
}

impl LiquidatorAccount {
    pub fn new(
        config: &GeneralConfig,
        marginfi_group_id: Pubkey,
        preferred_mint: Pubkey,
        cache: Arc<Cache>,
    ) -> Result<Self> {
        let signer = Keypair::from_bytes(&config.wallet_keypair)?;
        let rpc_client =
            RpcClient::new_with_commitment(config.rpc_url.clone(), CommitmentConfig::confirmed());

        let accounts = marginfi_account_by_authority(
            signer.pubkey(),
            &rpc_client,
            config.marginfi_program_id,
            marginfi_group_id,
        )?;
        thread_info!(
            "Found {} MarginFi accounts for the provided signer: {:?}",
            accounts.len(),
            accounts
        );

        let liquidator_address = if accounts.is_empty() {
            thread_info!("No MarginFi account found for the provided signer. Creating it...");
            let liquidator_marginfi_account = initialize_marginfi_account(
                &rpc_client,
                config.marginfi_program_id,
                marginfi_group_id,
                &signer,
            )?;

            while cache
                .marginfi_accounts
                .try_get_account(&liquidator_marginfi_account)
                .is_err()
            {
                thread_info!("Waiting for the new account info to arrive...");
                thread::sleep(Duration::from_secs(5));
            }

            liquidator_marginfi_account
        } else {
            accounts[0]
        };

        let preferred_mint_bank = cache.banks.try_get_account_for_mint(&preferred_mint)?;

        Ok(Self {
            liquidator_address,
            signer,
            program_id: config.marginfi_program_id,
            group: marginfi_group_id,
            preferred_mint_bank,
            rpc_client,
            cu_limit_ix: ComputeBudgetInstruction::set_compute_unit_limit(
                config.compute_unit_limit,
            ),
            cache,
        })
    }

    pub fn has_funds(&self) -> Result<bool> {
        let account = self
            .cache
            .marginfi_accounts
            .try_get_account(&self.liquidator_address)?;

        let preferred_mint_bank = self.cache.banks.try_get_bank(&self.preferred_mint_bank)?;

        let validation_result =
            account
                .get_balance_for_bank(&preferred_mint_bank)
                .map(|(balance, side)| match side {
                    BalanceSide::Assets => balance > 0,
                    _ => true,
                });

        Ok(validation_result.unwrap_or(false))
    }

    pub fn liquidate(
        &self,
        liquidatee_account: &MarginfiAccountWrapper,
        asset_bank: &Pubkey,
        liab_bank: &Pubkey,
        asset_amount: u64,
        stale_swb_oracles: &HashSet<Pubkey>,
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
            .banks
            .try_get_bank(asset_bank)
            .map_err(LiquidationError::from_anyhow_error)?;
        let asset_oracle_wrapper = OracleWrapper::build(&self.cache, asset_bank)
            .map_err(LiquidationError::from_anyhow_error)?;

        let liab_bank_wrapper = self
            .cache
            .banks
            .try_get_bank(liab_bank)
            .map_err(LiquidationError::from_anyhow_error)?;
        let liab_oracle_wrapper = OracleWrapper::build(&self.cache, liab_bank)
            .map_err(LiquidationError::from_anyhow_error)?;

        let signer_pk = self.signer.pubkey();
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
            if !check_asset_tags_matching(&bank_to_validate_against.bank, lending_account) {
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

        if contains_stale_oracles(stale_swb_oracles, &liquidator_swb_oracles) {
            thread_warn!("Skipping liquidation attempt because liquidator has stale oracles.");
            return Ok(());
        }

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

        if contains_stale_oracles(stale_swb_oracles, &liquidatee_swb_oracles) {
            thread_warn!("Skipping liquidation attempt because liquidatee has stale oracles.");
            return Ok(());
        }

        let joined_observation_accounts = liquidator_observation_accounts
            .iter()
            .chain(liquidatee_observation_accounts.iter())
            .copied()
            .collect::<Vec<_>>();

        let total_observation_accounts = joined_observation_accounts.len();

        let liquidate_ix = make_liquidate_ix(
            self.program_id,
            self.group,
            self.liquidator_address,
            &asset_bank_wrapper,
            asset_oracle_wrapper.address,
            &liab_bank_wrapper,
            liab_oracle_wrapper.address,
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
            &[self.cu_limit_ix.clone(), liquidate_ix.clone()],
            luts,
            recent_blockhash,
        )
        .map_err(LiquidationError::from_compile_error)?;

        let txn = VersionedTransaction::try_new(VersionedMessage::V0(msg), &[&self.signer])
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
                    preflight_commitment: Some(CommitmentLevel::Processed),
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

        let signer_pk = self.signer.pubkey();

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
                &[self.cu_limit_ix.clone(), withdraw_ix],
                Some(&signer_pk),
                &[&self.signer],
                recent_blockhash,
            );

        thread_debug!(
            "Withdrawing {:?} unscaled tokens of the Mint {} from the Liquidator account {:?}, Bank {:?}, ",
            amount,
            mint,
            token_account,
            self.preferred_mint_bank
        );

        let res = self
            .rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &tx,
                CommitmentConfig::finalized(),
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    preflight_commitment: Some(CommitmentLevel::Processed),
                    ..Default::default()
                },
            )
            .map_err(|e| anyhow::anyhow!(e))?;

        thread_debug!("Withdrawal txn: {:?} ", res);
        Ok(())
    }

    pub fn repay(&self, bank: &BankWrapper, amount: u64, repay_all: Option<bool>) -> Result<()> {
        let marginfi_account = self.liquidator_address;

        let signer_pk = self.signer.pubkey();

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
                &[&self.signer],
                recent_blockhash,
            );

        thread_debug!(
            "Repaying {:?} unscaled tokens to the bank {}, token account {:?}",
            amount,
            bank.address,
            token_account
        );

        let res = self
            .rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &tx,
                CommitmentConfig::finalized(),
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    preflight_commitment: Some(CommitmentLevel::Processed),
                    ..Default::default()
                },
            );
        thread_debug!(
            "The repaying result for account {:?} (without preflight check): {:?} ",
            marginfi_account,
            res
        );

        Ok(())
    }

    pub fn deposit(&self, bank: &BankWrapper, amount: u64) -> Result<()> {
        let marginfi_account = self.liquidator_address;

        let signer_pk = self.signer.pubkey();

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
                &[&self.signer],
                recent_blockhash,
            );

        thread_debug!("Depositing {:?}, token account {:?}", amount, token_account);

        let res = self
            .rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &tx,
                CommitmentConfig::finalized(),
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    preflight_commitment: Some(CommitmentLevel::Processed),
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

fn contains_stale_oracles(stale_oracles: &HashSet<Pubkey>, account_oracles: &[Pubkey]) -> bool {
    if let Some(oracle) = account_oracles
        .iter()
        .find(|oracle| stale_oracles.contains(*oracle))
    {
        thread_warn!("Found stale oracle: {}.", oracle);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_contains_stale_oracles_with_stale() {
        let stale_oracle = Pubkey::new_unique();
        let mut stale_oracles = HashSet::new();
        stale_oracles.insert(stale_oracle);
        let account_oracles = vec![Pubkey::new_unique(), stale_oracle, Pubkey::new_unique()];

        assert!(contains_stale_oracles(&stale_oracles, &account_oracles));
    }

    #[test]
    fn test_contains_stale_oracles_without_stale() {
        let stale_oracle = Pubkey::new_unique();
        let mut stale_oracles = HashSet::new();
        stale_oracles.insert(stale_oracle);
        let account_oracles = vec![Pubkey::new_unique(), Pubkey::new_unique()];

        assert!(!contains_stale_oracles(&stale_oracles, &account_oracles));
    }

    #[test]
    fn test_contains_stale_oracles_empty_account_oracles() {
        let stale_oracle = Pubkey::new_unique();
        let mut stale_oracles = HashSet::new();
        stale_oracles.insert(stale_oracle);
        let account_oracles = vec![];

        assert!(!contains_stale_oracles(&stale_oracles, &account_oracles));
    }

    #[test]
    fn test_contains_stale_oracles_empty_stale_oracles() {
        let account_oracles = vec![Pubkey::new_unique()];
        let stale_oracles = HashSet::new();

        assert!(!contains_stale_oracles(&stale_oracles, &account_oracles));
    }

    #[test]
    fn test_contains_stale_oracles_multiple_stale() {
        let stale1 = Pubkey::new_unique();
        let stale2 = Pubkey::new_unique();
        let mut stale_oracles = HashSet::new();
        stale_oracles.insert(stale1);
        stale_oracles.insert(stale2);
        let account_oracles = vec![stale2, Pubkey::new_unique()];

        assert!(contains_stale_oracles(&stale_oracles, &account_oracles));
    }
}
