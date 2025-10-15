use super::{bank::BankWrapper, marginfi_account::MarginfiAccountWrapper};
use crate::{
    cache::Cache,
    cli::setup::marginfi_account_by_authority,
    config::GeneralConfig,
    kamino_ixs::{make_refresh_obligation_ix, make_refresh_reserve_ix},
    marginfi_ixs::{
        initialize_marginfi_account, make_deposit_ix, make_end_liquidate_ix,
        make_init_liquidation_record_ix, make_liquidate_ix, make_repay_ix, make_start_liquidate_ix,
        make_withdraw_ix,
    },
    metrics::LIQUIDATION_ATTEMPTS,
    utils::{
        check_asset_tags_matching, lut_cache::LutCache, swb_cranker::is_stale_swb_price_error,
    },
    wrappers::oracle::{OracleWrapper, OracleWrapperTrait},
};
use anyhow::{anyhow, Context, Result};
use jupiter_swap_api_client::{
    quote::QuoteRequest,
    swap::{PrioritizationType, SwapInstructionsResponse, SwapRequest},
    transaction_config::{ComputeUnitPriceMicroLamports, TransactionConfig},
    JupiterSwapApiClient,
};
use log::{debug, info, warn};
use marginfi_type_crate::types::BalanceSide;
use solana_client::{rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig};

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
use tokio::runtime::{Builder, Runtime};

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
    jup_swap_client: JupiterSwapApiClient,
    slippage_bps: u16,
    compute_unit_price_micro_lamports: ComputeUnitPriceMicroLamports,
    tokio_rt: Runtime,
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
        info!(
            "Found {} MarginFi accounts for the provided signer: {:?}",
            accounts.len(),
            accounts
        );

        let liquidator_address = if accounts.is_empty() {
            info!("No MarginFi account found for the provided signer. Creating it...");
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
                info!("Waiting for the new account info to arrive...");
                thread::sleep(Duration::from_secs(5));
            }

            liquidator_marginfi_account
        } else {
            accounts[0]
        };

        let preferred_mint_bank = cache.banks.try_get_account_for_mint(&preferred_mint)?;
        let jup_swap_client = JupiterSwapApiClient::new(config.jup_swap_api_url.clone());
        let slippage_bps = config.slippage_bps;
        let compute_unit_price_micro_lamports =
            ComputeUnitPriceMicroLamports::MicroLamports(config.compute_unit_price_micro_lamports);

        let tokio_rt = Builder::new_multi_thread()
            .thread_name("rebalancer")
            .worker_threads(2)
            .enable_all()
            .build()?;

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
            jup_swap_client,
            slippage_bps,
            compute_unit_price_micro_lamports,
            tokio_rt,
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

    pub fn init_liq_record(&self, liquidatee_account: &MarginfiAccountWrapper) -> Result<()> {
        info!(
            "Initializing liquidation record for account {:?} with liquidator account {:?}.",
            liquidatee_account.address, self.liquidator_address
        );

        let signer_pk = self.signer.pubkey();
        let init_ix =
            make_init_liquidation_record_ix(self.program_id, liquidatee_account.address, signer_pk);

        let recent_blockhash = self
            .rpc_client
            .get_latest_blockhash()
            .map_err(|e| anyhow!(e))?;

        let msg = Message::try_compile(&signer_pk, &[init_ix.clone()], &[], recent_blockhash)
            .map_err(|e| anyhow!(e))?;

        let txn = VersionedTransaction::try_new(VersionedMessage::V0(msg), &[&self.signer])
            .map_err(|e| anyhow!(e))?;

        info!(
            "Sending liquidation tx for the Account {} .",
            liquidatee_account.address
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
                info!(
                    "Liquidation record init tx for the Account {} was confirmed. Signature: {}",
                    liquidatee_account.address, signature,
                );
                Ok(())
            }
            Err(err) => Err(anyhow!(
                "Liquidation record init tx for the Account {} failed: {} ",
                liquidatee_account.address,
                err
            )),
        }
    }

    pub fn liquidate(
        &self,
        liquidatee_account: &MarginfiAccountWrapper,
        asset_bank: &Pubkey,
        liab_bank: &Pubkey,
        asset_amount: u64,
        liab_amount: u64,
        stale_swb_oracles: &HashSet<Pubkey>,
        lut_cache: &mut LutCache,
    ) -> Result<(), LiquidationError> {
        let liquidatee_account_address = liquidatee_account.address;
        let asset_bank_wrapper = self
            .cache
            .banks
            .try_get_bank(asset_bank)
            .map_err(LiquidationError::from_anyhow_error)?;

        let liab_bank_wrapper = self
            .cache
            .banks
            .try_get_bank(liab_bank)
            .map_err(LiquidationError::from_anyhow_error)?;

        let signer_pk = self.signer.pubkey();
        let asset_mint = asset_bank_wrapper.bank.mint;
        let liab_mint = liab_bank_wrapper.bank.mint;

        let liquidator_account = &self
            .cache
            .marginfi_accounts
            .try_get_account(&self.liquidator_address)
            .map_err(LiquidationError::from_anyhow_error)?;

        // TODO: remove
        if *liab_bank == Pubkey::from_str_const("BeNBJrAh1tZg5sqgt8D6AWKJLD5KkBrfZvtcgd7EuiAR") {
            let uxd_balance = liquidator_account
                .get_balance_for_bank(&liab_bank_wrapper)
                .map(|(value, _)| value.to_num())
                .unwrap_or(0);
            if uxd_balance < liab_amount {
                info!("Not enough UXD collateral: ignoring liquidation");
                return Ok(());
            }
        }

        let lending_account = &liquidator_account.lending_account;
        for bank_to_validate_against in [&asset_bank_wrapper, &liab_bank_wrapper] {
            if !check_asset_tags_matching(&bank_to_validate_against.bank, lending_account) {
                // This is a precaution to not attempt to liquidate staked collateral positions when liquidator has non-SOL positions open.
                // Expected to happen quite often for now. Later on, we can add a more sophisticated filtering logic on the higher level.
                debug!("Bank {:?} does not match the asset tags of the lending account -> skipping liquidation attempt", bank_to_validate_against.address);
                return Ok(());
            }
        }

        LIQUIDATION_ATTEMPTS.inc();

        let banks_to_include: Vec<Pubkey> = vec![];
        let banks_to_exclude: Vec<Pubkey> = vec![];
        let (liquidatee_observation_accounts, liquidatee_swb_oracles, mut kamino_reserves) =
            MarginfiAccountWrapper::get_observation_accounts::<OracleWrapper>(
                &liquidatee_account.lending_account,
                &banks_to_include,
                &banks_to_exclude,
                self.cache.clone(),
            )
            .map_err(LiquidationError::from_anyhow_error)?;
        debug!(
            "The Liquidatee {:?} observation accounts: {:?}",
            liquidatee_account_address, liquidatee_observation_accounts
        );

        if contains_stale_oracles(stale_swb_oracles, &liquidatee_swb_oracles) {
            return Ok(());
        }

        let mut luts: Vec<AddressLookupTableAccount> = self.cache.luts.clone();
        let mut ixs = Vec::new();

        let banks_to_include: Vec<Pubkey> = vec![*liab_bank, *asset_bank];
        let banks_to_exclude: Vec<Pubkey> = vec![];
        let (liquidator_observation_accounts, liquidator_swb_oracles, liquidator_kamino_reserves) =
            MarginfiAccountWrapper::get_observation_accounts::<OracleWrapper>(
                lending_account,
                &banks_to_include,
                &banks_to_exclude,
                self.cache.clone(),
            )
            .map_err(LiquidationError::from_anyhow_error)?;
        debug!(
            "The Liquidator {} observation accounts: {:?}",
            &self.liquidator_address, liquidator_observation_accounts
        );

        kamino_reserves.extend(liquidator_kamino_reserves);

        let old_way = true;
        if old_way {
            if contains_stale_oracles(stale_swb_oracles, &liquidator_swb_oracles) {
                warn!("Skipping liquidation attempt because liquidator has stale oracles.");
                return Ok(());
            }

            let joined_observation_accounts = liquidator_observation_accounts
                .iter()
                .chain(liquidatee_observation_accounts.iter())
                .copied()
                .collect::<Vec<_>>();

            let asset_oracle_wrapper = OracleWrapper::build(&self.cache, asset_bank)
                .map_err(LiquidationError::from_anyhow_error)?;
            let liab_oracle_wrapper = OracleWrapper::build(&self.cache, liab_bank)
                .map_err(LiquidationError::from_anyhow_error)?;

            let liquidate_ix = make_liquidate_ix(
                self.program_id,
                self.group,
                self.liquidator_address,
                &asset_bank_wrapper,
                &liab_bank_wrapper,
                asset_oracle_wrapper.addresses.as_slice(),
                liab_oracle_wrapper.addresses.as_slice(),
                signer_pk,
                liquidatee_account_address,
                self.cache
                    .mints
                    .try_get_account(&liab_mint)
                    .map_err(LiquidationError::from_anyhow_error)?
                    .account
                    .owner,
                joined_observation_accounts.as_ref(),
                asset_amount,
            );
            ixs.push(self.cu_limit_ix.clone());

            for kamino_reserve in kamino_reserves {
                let &(lending_market, oracle_setup) = self
                    .cache
                    .kamino_reserves
                    .get(&kamino_reserve)
                    .context(format!(
                        "Couldn't find the data for kamino reserve: {}",
                        kamino_reserve
                    ))
                    .map_err(LiquidationError::from_anyhow_error)?;

                let refresh_reserve_ix =
                    make_refresh_reserve_ix(kamino_reserve, lending_market, oracle_setup);
                ixs.push(refresh_reserve_ix);

                let refresh_obligation_ix = make_refresh_obligation_ix(
                    asset_bank_wrapper.bank.kamino_obligation,
                    lending_market,
                    &[kamino_reserve],
                );
                ixs.push(refresh_obligation_ix);
            }

            ixs.push(liquidate_ix);
        } else {
            let (out_amount, jup) = self
                .swap(asset_mint, liab_mint, asset_amount)
                .map_err(LiquidationError::from_anyhow_error)?;

            let repay_amount = (0.95 * (out_amount as f64)) as u64;
            info!(
                "Exchanging {} ({}) for {} ({}) and trying to repay {} (out of {}).",
                asset_amount, asset_mint, out_amount, liab_mint, repay_amount, liab_amount
            );

            if jup.compute_unit_limit == 0 {
                ixs.push(self.cu_limit_ix.clone());
            }

            let start_ix = make_start_liquidate_ix(
                self.program_id,
                liquidatee_account_address,
                signer_pk,
                liquidatee_account.liquidation_record,
                liquidatee_observation_accounts.as_ref(),
            );
            ixs.push(start_ix);

            let asset_mint_wrapper = self
                .cache
                .mints
                .try_get_account(&asset_mint)
                .map_err(LiquidationError::from_anyhow_error)?;
            let withdraw_ix = make_withdraw_ix(
                self.program_id,
                self.group,
                liquidatee_account_address,
                signer_pk,
                &asset_bank_wrapper,
                asset_mint_wrapper.token,
                asset_mint_wrapper.account.owner,
                liquidatee_observation_accounts.as_ref(),
                asset_amount,
                None,
            );
            ixs.push(withdraw_ix);

            ixs.extend(jup.compute_budget_instructions);

            if let Some(ix) = jup.token_ledger_instruction {
                ixs.push(ix);
            }

            ixs.extend(jup.setup_instructions);
            ixs.push(jup.swap_instruction);
            if let Some(ix) = jup.cleanup_instruction {
                ixs.push(ix);
            }

            let liab_mint_wrapper = self
                .cache
                .mints
                .try_get_account(&liab_mint)
                .map_err(LiquidationError::from_anyhow_error)?;
            let repay_ix = make_repay_ix(
                self.program_id,
                self.group,
                liquidatee_account_address,
                signer_pk,
                &liab_bank_wrapper,
                liab_mint_wrapper.token,
                liab_mint_wrapper.account.owner,
                repay_amount,
                None,
            );
            ixs.push(repay_ix);

            let end_ix = make_end_liquidate_ix(
                self.program_id,
                liquidatee_account_address,
                signer_pk,
                liquidatee_account.liquidation_record,
                self.cache.global_fee_state_key,
                self.cache.global_fee_wallet,
                liquidatee_observation_accounts.as_ref(),
            );
            ixs.push(end_ix);

            if matches!(
                jup.prioritization_type,
                Some(PrioritizationType::Jito { .. })
            ) {
                ixs.extend(jup.other_instructions);
            }

            let jup_luts = lut_cache
                .fetch_missing(&self.rpc_client, &jup.address_lookup_table_addresses)
                .map_err(LiquidationError::from_anyhow_error)?;
            luts.extend(jup_luts);
        }
        let recent_blockhash = self
            .rpc_client
            .get_latest_blockhash()
            .map_err(|e| LiquidationError::from_anyhow_error(anyhow!(e)))?;

        let msg = Message::try_compile(&signer_pk, &ixs, &luts, recent_blockhash)
            .map_err(LiquidationError::from_compile_error)?;

        let tx = VersionedTransaction::try_new(VersionedMessage::V0(msg), &[&self.signer])
            .map_err(LiquidationError::from_signer_error)?;

        info!(
            "Liquidating account {:?} with liquidator account {:?}. Amount: {}",
            liquidatee_account_address, self.liquidator_address, asset_amount
        );

        match self
            .rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &tx,
                CommitmentConfig::confirmed(),
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    preflight_commitment: Some(CommitmentLevel::Processed),
                    ..Default::default()
                },
            ) {
            Ok(signature) => {
                info!(
                    "Liquidation tx for the Account {} was confirmed. Signature: {}",
                    liquidatee_account_address, signature,
                );
                Ok(())
            }
            Err(err) => {
                let mut swb_oracles: Vec<Pubkey> = vec![];
                if is_stale_swb_price_error(&err) {
                    swb_oracles.extend(liquidatee_swb_oracles);
                    swb_oracles.extend(liquidator_swb_oracles);
                }
                Err(LiquidationError::from_anyhow_error_with_keys(
                    anyhow!(
                        "Liquidation tx for the Account {} failed: {} ",
                        liquidatee_account_address,
                        err
                    ),
                    swb_oracles,
                ))
            }
        }
    }

    fn swap(
        &self,
        input_mint: Pubkey,
        output_mint: Pubkey,
        amount: u64,
    ) -> Result<(u64, SwapInstructionsResponse)> {
        let quote = self
            .tokio_rt
            .block_on(self.jup_swap_client.quote(&QuoteRequest {
                input_mint,
                output_mint,
                amount,
                slippage_bps: self.slippage_bps,
                ..Default::default()
            }))?;

        let out_amount = quote.out_amount;

        let si = self
            .tokio_rt
            .block_on(self.jup_swap_client.swap_instructions(&SwapRequest {
                user_public_key: self.signer.pubkey(),
                quote_response: quote.clone(),
                config: TransactionConfig {
                    wrap_and_unwrap_sol: false,
                    compute_unit_price_micro_lamports: Some(
                        self.compute_unit_price_micro_lamports.clone(),
                    ),
                    ..Default::default()
                },
            }))?;

        Ok((out_amount, si))
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
        debug!("Collecting observation accounts for the account: {:?} with banks_to_include {:?} and banks_to_exclude {:?}", 
        &self.liquidator_address, &banks_to_include, &banks_to_exclude);
        let (observation_accounts, _, _) =
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
            observation_accounts.as_ref(),
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

        debug!(
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

        debug!("Withdrawal txn: {:?} ", res);
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

        debug!(
            "Repaying {:?} unscaled tokens to the bank {}, token account {:?}",
            amount, bank.address, token_account
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
        debug!(
            "The repaying result for account {:?} (without preflight check): {:?} ",
            marginfi_account, res
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

        debug!("Depositing {:?}, token account {:?}", amount, token_account);

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
        debug!(
            "Depositing result for account {:?} (without preflight check): {:?} ",
            marginfi_account, res
        );

        Ok(())
    }
}

fn contains_stale_oracles(stale_oracles: &HashSet<Pubkey>, account_oracles: &[Pubkey]) -> bool {
    if let Some(oracle) = account_oracles
        .iter()
        .find(|oracle| stale_oracles.contains(*oracle))
    {
        debug!("Found stale oracle: {}.", oracle);
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
