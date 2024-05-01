use std::{
    cmp::min,
    collections::HashSet,
    error::Error,
    sync::{Arc, RwLock, RwLockReadGuard},
    thread::{self, JoinHandle},
};

use crossbeam::channel::Receiver;
use fixed::types::I80F48;
use fixed_macro::types::I80F48;
use jupiter_swap_api_client::{
    quote::QuoteRequest,
    swap::SwapRequest,
    transaction_config::{ComputeUnitPriceMicroLamports, TransactionConfig},
    JupiterSwapApiClient,
};
use log::{debug, error, info, trace, warn};
use marginfi::{
    constants::EXP_10_I80F48,
    state::{
        marginfi_account::{BalanceSide, RequirementType},
        price::{OraclePriceType, PriceAdapter, PriceBias},
    },
};
use sha2::{Digest, Sha256};
use solana_sdk::{
    pubkey,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair},
    signer::{SeedDerivable, Signer},
    transaction::VersionedTransaction,
};

use crate::{
    marginfi_account::{MarginfiAccountError, TxConfig},
    sender::{aggressive_send_tx, SenderCfg},
    state_engine::{
        engine::StateEngineService,
        marginfi_account::{MarginfiAccountWrapper, MarginfiAccountWrapperError},
    },
    utils::{
        calc_weighted_assets, calc_weighted_liabs, fixed_from_float, from_pubkey_string,
        from_vec_str_to_pubkey,
    },
};

#[derive(thiserror::Error, Debug)]
pub enum ProcessorError {
    #[error("Failed to read account")]
    FailedToReadAccount,
    #[error("Failed to start liquidator")]
    SetupFailed,
    #[error("MarginfiAccountWrapperError: {0}")]
    MarginfiAccountWrapperError(#[from] MarginfiAccountWrapperError),
    #[error("Error: {0}")]
    Error(&'static str),
    #[error("MarginfiAccountError: {0}")]
    MarginfiAccountError(#[from] MarginfiAccountError),
    #[error("ReqwsetError: {0}")]
    ReqwsetError(#[from] reqwest::Error),
    #[error("AnyhowError: {0}")]
    AnyhowError(#[from] anyhow::Error),
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct EvaLiquidatorCfg {
    pub keypair_path: String,
    #[serde(deserialize_with = "from_pubkey_string")]
    pub liquidator_account: Pubkey,
    #[serde(
        default = "EvaLiquidatorCfg::default_token_account_dust_threshold",
        deserialize_with = "fixed_from_float"
    )]
    pub token_account_dust_threshold: I80F48,
    #[serde(
        default = "EvaLiquidatorCfg::default_max_sol_balance",
        deserialize_with = "fixed_from_float"
    )]
    pub max_sol_balance: I80F48,
    #[serde(
        default = "EvaLiquidatorCfg::default_preferred_mints",
        deserialize_with = "from_vec_str_to_pubkey"
    )]
    pub preferred_mints: Vec<Pubkey>,

    #[serde(
        default = "EvaLiquidatorCfg::default_swap_mint",
        deserialize_with = "from_pubkey_string"
    )]
    pub swap_mint: Pubkey,
    #[serde(default = "EvaLiquidatorCfg::default_jup_swap_api_url")]
    pub jup_swap_api_url: String,
    #[serde(default = "EvaLiquidatorCfg::default_slippage_bps")]
    pub slippage_bps: u16,
    #[serde(default = "EvaLiquidatorCfg::default_compute_unit_price_micro_lamports")]
    pub compute_unit_price_micro_lamports: Option<u64>,
    /// Minimum profit on a liquidation to be considered, denominated in USD
    ///
    /// Example:
    /// 0.01 is $0.01
    ///
    /// Default: 0.1
    #[serde(default = "EvaLiquidatorCfg::default_min_profit")]
    pub min_profit: f64,
    /// Maximum liquidation value in USD
    pub max_liquidation_value: Option<f64>,
}

impl EvaLiquidatorCfg {
    pub fn default_token_account_dust_threshold() -> I80F48 {
        I80F48!(0.01)
    }

    pub fn default_max_sol_balance() -> I80F48 {
        I80F48!(1)
    }

    pub fn default_preferred_mints() -> Vec<Pubkey> {
        vec![pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v")]
    }

    pub fn default_swap_mint() -> Pubkey {
        pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v")
    }

    pub fn default_jup_swap_api_url() -> String {
        "https://quote-api.jup.ag/v6".to_string()
    }

    pub fn default_slippage_bps() -> u16 {
        250
    }

    pub fn default_compute_unit_price_micro_lamports() -> Option<u64> {
        Some(10_000)
    }

    pub fn default_min_profit() -> f64 {
        0.1
    }

    pub fn get_tx_config(&self) -> TxConfig {
        TxConfig {
            compute_unit_price_micro_lamports: self.compute_unit_price_micro_lamports,
        }
    }
}

pub struct EvaLiquidator {
    // liquidator_account: Arc<RwLock<MarginfiAccountWrapper>>,
    liquidator_account: crate::marginfi_account::MarginfiAccount,
    state_engine: Arc<StateEngineService>,
    update_rx: Receiver<()>,
    signer_keypair: Arc<Keypair>,
    config: EvaLiquidatorCfg,
    preferred_mints: HashSet<Pubkey>,
    swap_mint_bank_pk: Pubkey,
}

impl EvaLiquidator {
    pub fn start(
        state_engine: Arc<StateEngineService>,
        update_rx: Receiver<()>,
        cfg: EvaLiquidatorCfg,
    ) -> Result<JoinHandle<Result<(), ProcessorError>>, ProcessorError> {
        thread::Builder::new()
            .name("evaLiquidatorProcessor".to_string())
            .spawn(move || -> Result<(), ProcessorError> {
                info!("Starting liquidator processor");
                let liquidator_account = {
                    let account_ref = state_engine.marginfi_accounts.get(&cfg.liquidator_account);

                    if account_ref.is_none() {
                        error!("Liquidator account not found");
                        return Err(ProcessorError::SetupFailed);
                    }

                    let account = account_ref.as_ref().unwrap().value().clone();

                    drop(account_ref);

                    account
                };

                debug!(
                    "Liquidator account: {:?}",
                    liquidator_account.read().unwrap().address
                );

                let keypair = Arc::new(read_keypair_file(&cfg.keypair_path).map_err(|_| {
                    error!("Failed to read keypair file at {}", cfg.keypair_path);
                    ProcessorError::SetupFailed
                })?);

                state_engine
                    .token_account_manager
                    .create_token_accounts(keypair.clone())
                    .map_err(|e| {
                        error!("Failed to create token accounts: {:?}", e);
                        ProcessorError::SetupFailed
                    })?;

                let preferred_mints = cfg.preferred_mints.iter().cloned().collect();

                let swap_mint_bank_pk = state_engine
                    .get_bank_for_mint(&cfg.swap_mint)
                    .ok_or(ProcessorError::Error("Failed to get bank for swap mint"))?
                    .read()
                    .unwrap()
                    .address;

                let rpc_client = state_engine.rpc_client.clone();

                let processor = EvaLiquidator {
                    state_engine: state_engine.clone(),
                    update_rx,
                    liquidator_account: crate::marginfi_account::MarginfiAccount::new(
                        liquidator_account,
                        state_engine.clone(),
                        keypair.clone(),
                        rpc_client,
                    ),
                    signer_keypair: keypair,
                    config: cfg,
                    preferred_mints,
                    swap_mint_bank_pk,
                };

                if let Err(e) = tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(processor.run_outer())
                {
                    error!("Error running processor: {:?}", e);
                }

                warn!("Processor thread exiting");

                Ok(())
            })
            .map_err(|_| ProcessorError::SetupFailed)
    }

    async fn run_outer(&self) -> Result<(), ProcessorError> {
        loop {
            match self.run().await {
                Ok(_) => {
                    warn!("Processor exited, restarting...");
                }
                Err(e) => {
                    error!("Error running processor: {:?}, restarting...", e);
                }
            }
        }
    }

    async fn run(&self) -> Result<(), ProcessorError> {
        loop {
            while self.needs_to_be_rebalanced() {
                self.rebalance_with_recovery().await?;
            }

            match self.evaluate_all_accounts() {
                Err(e) => {
                    error!("Error processing accounts: {:?}", e);
                }
                _ => {}
            };

            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        }

        Ok(())
    }

    async fn rebalance_with_recovery(&self) -> Result<(), ProcessorError> {
        let mut retries = 0;
        while self.rebalance_accounts().await.is_err() {
            retries += 1;

            if retries > 5 {
                error!("Failed to rebalance accounts after 5 retries, exiting...");
                self.state_engine
                    .load_initial_state(self.config.liquidator_account)
                    .await?;
                return Err(ProcessorError::Error("Failed to rebalance accounts"));
            }

            error!("Error rebalancing accounts, retrying...");
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        }

        debug!("Rebalanced accounts");

        Ok(())
    }

    async fn rebalance_accounts(&self) -> Result<(), ProcessorError> {
        self.sell_non_preferred_deposits().await?;
        self.replay_liabilities().await?;
        self.handle_tokens_in_token_accounts().await?;
        self.deposit_preferred_tokens().await?;

        Ok(())
    }

    /// Check if a user needs to be rebalanced
    ///
    /// - User has tokens in token accounts
    /// - User has non-stable deposits
    /// - User has any liabilities
    fn needs_to_be_rebalanced(&self) -> bool {
        debug!("Checking if liquidator needs to be rebalanced");
        let rebalance_needed = self.has_tokens_in_token_accounts()
            || self.has_non_preferred_deposits()
            || self.has_liabilties();

        if rebalance_needed {
            info!("Liquidator needs to be rebalanced");
        } else {
            debug!("Liquidator does not need to be rebalanced");
        }

        rebalance_needed
    }

    fn has_tokens_in_token_accounts(&self) -> bool {
        debug!("Checking if liquidator has tokens in token accounts");
        let has_tokens_in_tas = self.state_engine.token_accounts.iter().any(|account| {
            account
                .read()
                .map_err(|_| ProcessorError::FailedToReadAccount)
                .map(|account| {
                    let value = account.get_value().unwrap();
                    debug!("Token account {} value: {:?}", account.mint, value);
                    value > self.config.token_account_dust_threshold
                })
                .unwrap_or(false)
        });

        if has_tokens_in_tas {
            info!("Liquidator has tokens in token accounts");
        } else {
            debug!("Liquidator has no tokens in token accounts");
        }

        has_tokens_in_tas
    }

    async fn handle_tokens_in_token_accounts(&self) -> Result<(), ProcessorError> {
        debug!("Handling tokens in token accounts");
        let bank_addresses = self
            .state_engine
            .banks
            .iter()
            .map(|e| e.key().clone())
            .filter(|bank_pk| self.swap_mint_bank_pk != *bank_pk)
            .collect::<Vec<_>>();

        for bank_pk in bank_addresses {
            self.handle_token_in_token_account(&bank_pk).await?;
        }

        self.state_engine
            .refresh_token_account(&self.swap_mint_bank_pk)
            .await?;

        let balance = self.get_token_balance_for_bank(&self.swap_mint_bank_pk)?;

        if let Some(balance) = balance {
            if !balance.is_zero() {
                self.liquidator_account.deposit(
                    self.swap_mint_bank_pk,
                    balance.to_num(),
                    self.config.get_tx_config(),
                )?;
            }
        }

        Ok(())
    }

    async fn handle_token_in_token_account(&self, bank_pk: &Pubkey) -> Result<(), ProcessorError> {
        trace!("Handle token in token account for bank {}", bank_pk);

        let amount = self.get_token_balance_for_bank(bank_pk)?;

        if amount.is_none() {
            warn!("No token balance found for bank {}", bank_pk);
            return Ok(());
        }

        let amount = amount.unwrap();

        trace!("Found token balance of {} for bank {}", amount, bank_pk);

        let value = self.get_value(
            amount,
            &bank_pk,
            RequirementType::Equity,
            BalanceSide::Assets,
        )?;

        trace!("Token balance value: ${}", value);

        if value < self.config.token_account_dust_threshold {
            trace!("Token balance value is below dust threshold");
            return Ok(());
        }

        self.swap(amount.to_num(), bank_pk, &self.swap_mint_bank_pk)
            .await?;

        Ok(())
    }

    async fn deposit_preferred_tokens(&self) -> Result<(), ProcessorError> {
        debug!("Depositing preferred tokens");
        let balance = self.get_token_balance_for_bank(&self.swap_mint_bank_pk)?;

        if balance.is_none() {
            debug!("No token balance found for bank {}", self.swap_mint_bank_pk);
            return Ok(());
        }

        let balance = balance.unwrap();

        if balance.is_zero() {
            debug!("No token balance found for bank {}", self.swap_mint_bank_pk);
            return Ok(());
        }

        debug!(
            "Found token balance of {} for bank {}",
            balance, self.swap_mint_bank_pk
        );

        self.liquidator_account.deposit(
            self.swap_mint_bank_pk,
            balance.to_num(),
            self.config.get_tx_config(),
        )?;

        Ok(())
    }

    fn has_liabilties(&self) -> bool {
        debug!("Checking if liquidator has liabilities");

        let has_liabs = self
            .liquidator_account
            .account_wrapper
            .read()
            .map_err(|_| ProcessorError::FailedToReadAccount)
            .map(|account| account.has_liabs())
            .unwrap_or(false);

        if has_liabs {
            info!("Liquidator has liabilities");
        } else {
            debug!("Liquidator has no liabilities");
        }

        has_liabs
    }

    fn get_liquidator_account(
        &self,
    ) -> Result<RwLockReadGuard<MarginfiAccountWrapper>, ProcessorError> {
        Ok(self
            .liquidator_account
            .account_wrapper
            .read()
            .map_err(|_| ProcessorError::FailedToReadAccount)?)
    }

    fn get_token_balance_for_bank(
        &self,
        bank_pk: &Pubkey,
    ) -> Result<Option<I80F48>, ProcessorError> {
        let mint = self
            .state_engine
            .banks
            .get(bank_pk)
            .and_then(|bank| bank.read().ok().map(|bank| bank.bank.mint));

        if mint.is_none() {
            warn!("No mint found for bank {}", bank_pk);
            return Ok(None);
        }

        let mint = mint.unwrap();

        let balance = self
            .state_engine
            .token_accounts
            .get(&mint)
            .and_then(|account| account.read().ok().map(|account| account.get_amount()));

        if balance.is_none() {
            warn!("No token balance found for mint {}", mint);
            return Ok(None);
        }

        Ok(balance)
    }

    async fn replay_liabilities(&self) -> Result<(), ProcessorError> {
        debug!("Replaying liabilities");
        let liabilties = self
            .liquidator_account
            .account_wrapper
            .read()
            .map_err(|_| ProcessorError::FailedToReadAccount)?
            .get_liabilites()
            .map_err(|_| ProcessorError::FailedToReadAccount)?;

        if liabilties.is_empty() {
            debug!("No liabilities to replay");
            return Ok(());
        }

        info!("Replaying liabilities");

        for (_, bank_pk) in liabilties {
            self.repay_liability(bank_pk).await?;
        }

        Ok(())
    }

    /// Repay a liability for a given bank
    ///
    /// - Find any bank tokens in token accounts
    /// - Calc $ value of liab
    /// - Find USDC in token accounts
    /// - Calc additional USDC to withdraw
    /// - Withdraw USDC
    /// - Swap USDC for bank tokens
    /// - Repay liability
    async fn repay_liability(&self, bank_pk: Pubkey) -> Result<(), ProcessorError> {
        let balance = self
            .get_liquidator_account()?
            .get_balance_for_bank(&bank_pk)?;

        if matches!(balance, None) || matches!(balance, Some((_, BalanceSide::Assets))) {
            warn!("No liability found for bank {}", bank_pk);
            return Ok(());
        }

        let (liab_balance, _) = balance.unwrap();

        debug!("Found liability of {} for bank {}", liab_balance, bank_pk);

        let token_balance = self
            .get_token_balance_for_bank(&bank_pk)?
            .unwrap_or_default();

        if !token_balance.is_zero() {
            debug!(
                "Found token balance of {} for bank {}",
                token_balance, bank_pk
            );
        }

        let liab_to_purchase = liab_balance - token_balance;

        debug!("Liability to purchase: {}", liab_to_purchase);

        if !liab_to_purchase.is_zero() {
            let liab_usd_value = self.get_value(
                liab_to_purchase,
                &bank_pk,
                RequirementType::Initial,
                BalanceSide::Liabilities,
            )?;

            debug!("Liability value: ${}", liab_usd_value);

            let required_swap_token =
                self.get_amount(liab_usd_value, &self.swap_mint_bank_pk, None)?;

            debug!(
                "Required swap token amount: {} for ${}",
                required_swap_token, liab_usd_value
            );

            let swap_token_balance = self
                .get_token_balance_for_bank(&self.swap_mint_bank_pk)?
                .unwrap_or_default();

            debug!(
                "Found swap token balance of {} for bank {}",
                swap_token_balance, self.swap_mint_bank_pk
            );

            // Log if token balance is > 0
            if !swap_token_balance.is_zero() {
                debug!(
                    "Found swap token balance of {} for bank {}",
                    swap_token_balance, self.swap_mint_bank_pk
                );
            }

            // Token balance to withdraw
            let token_balance_to_withdraw = required_swap_token - swap_token_balance;

            // Withdraw token balance
            let withdrawn_amount = if token_balance_to_withdraw.is_positive() {
                debug!(
                    "Token balance to withdraw: {} for bank {}",
                    token_balance_to_withdraw, self.swap_mint_bank_pk
                );

                let (max_withdraw_amount, withdraw_all) =
                    self.get_max_withdraw_for_bank(&self.swap_mint_bank_pk)?;

                let withdraw_amount = min(max_withdraw_amount, token_balance_to_withdraw);

                self.liquidator_account.withdraw(
                    &self.swap_mint_bank_pk,
                    withdraw_amount.to_num(),
                    Some(withdraw_all),
                    self.config.get_tx_config(),
                )?;

                withdraw_amount
            } else {
                I80F48::ZERO
            };

            let amount_to_swap = min(liab_balance + withdrawn_amount, required_swap_token);

            if amount_to_swap.is_positive() {
                self.swap(amount_to_swap.to_num(), &self.swap_mint_bank_pk, &bank_pk)
                    .await?;

                self.state_engine.refresh_token_account(&bank_pk).await?;
            }

            let token_balance = self
                .get_token_balance_for_bank(&bank_pk)?
                .unwrap_or_default();

            let repay_all = token_balance >= liab_balance;

            self.liquidator_account.repay(
                bank_pk,
                token_balance.to_num(),
                Some(repay_all),
                self.config.get_tx_config(),
            )?;
        }

        Ok(())
    }

    async fn sell_non_preferred_deposits(&self) -> Result<(), ProcessorError> {
        debug!("Selling non-preferred deposits");

        let non_preferred_deposits = self
            .liquidator_account
            .account_wrapper
            .read()
            .map_err(|_| ProcessorError::FailedToReadAccount)?
            .get_deposits(&self.config.preferred_mints)
            .map_err(|_| ProcessorError::FailedToReadAccount)?;

        if non_preferred_deposits.is_empty() {
            debug!("No non-preferred deposits to sell");
            return Ok(());
        }

        info!("Selling non-preferred deposits");

        for (_, bank_pk) in non_preferred_deposits {
            self.withdraw_and_sell_deposit(&bank_pk).await?;
        }

        Ok(())
    }

    async fn withdraw_and_sell_deposit(&self, bank_pk: &Pubkey) -> Result<(), ProcessorError> {
        let balance = self
            .get_liquidator_account()?
            .get_balance_for_bank(bank_pk)?;

        if !matches!(&balance, Some((_, BalanceSide::Assets))) {
            warn!("No deposit found for bank {}", bank_pk);
            return Ok(());
        }

        let (balance, _) = balance.unwrap();

        debug!("Found deposit of {} for bank {}", balance, bank_pk);

        let (withdraw_amount, withdraw_all) = self.get_max_withdraw_for_bank(bank_pk)?;

        let amount = withdraw_amount.to_num::<u64>();

        self.liquidator_account.withdraw(
            bank_pk,
            amount,
            Some(withdraw_all),
            self.config.get_tx_config(),
        )?;

        self.swap(amount, bank_pk, &self.swap_mint_bank_pk).await?;

        Ok(())
    }

    pub fn get_value(
        &self,
        amount: I80F48,
        bank_pk: &Pubkey,
        requirement_type: RequirementType,
        side: BalanceSide,
    ) -> Result<I80F48, ProcessorError> {
        let bank_ref = self
            .state_engine
            .get_bank(bank_pk)
            .ok_or(ProcessorError::Error("Failed to get bank"))?;

        let value = match side {
            BalanceSide::Assets => {
                calc_weighted_assets(bank_ref, amount.to_num(), requirement_type)?
            }
            BalanceSide::Liabilities => {
                calc_weighted_liabs(bank_ref, amount.to_num(), requirement_type)?
            }
        };

        Ok(value)
    }

    pub fn get_amount(
        &self,
        value: I80F48,
        bank_pk: &Pubkey,
        price_bias: Option<PriceBias>,
    ) -> Result<I80F48, ProcessorError> {
        let bank_ref = self
            .state_engine
            .get_bank(bank_pk)
            .ok_or(ProcessorError::Error("Failed to get bank"))?;

        let bank = bank_ref
            .read()
            .map_err(|_| ProcessorError::Error("Failed to get bank"))?;

        let price = bank
            .oracle_adapter
            .price_adapter
            .get_price_of_type(
                marginfi::state::price::OraclePriceType::RealTime,
                price_bias,
            )
            .map_err(|_| ProcessorError::Error("Failed to get price"))?;

        let amount_ui = value / price;

        Ok(amount_ui * EXP_10_I80F48[bank.bank.mint_decimals as usize])
    }

    fn has_non_preferred_deposits(&self) -> bool {
        debug!("Checking if liquidator has non-preferred deposits");

        let has_non_preferred_deposits = self
            .liquidator_account
            .account_wrapper
            .read()
            .map_err(|_| ProcessorError::FailedToReadAccount)
            .unwrap()
            .account
            .lending_account
            .balances
            .iter()
            .filter(|balance| balance.active)
            .any(|balance| {
                let mint = self
                    .state_engine
                    .banks
                    .get(&balance.bank_pk)
                    .and_then(|bank| bank.read().ok().map(|bank| bank.bank.mint))
                    .unwrap();

                let has_non_preferred_deposit =
                    matches!(balance.get_side(), Some(BalanceSide::Assets))
                        && !self.preferred_mints.contains(&mint);

                debug!("Found non-preferred {} deposits", mint);

                has_non_preferred_deposit
            });

        if has_non_preferred_deposits {
            info!("Liquidator has non-preferred deposits");
        } else {
            debug!("Liquidator has no non-preferred deposits");
        }

        has_non_preferred_deposits
    }

    fn evaluate_all_accounts(&self) -> Result<bool, ProcessorError> {
        let start = std::time::Instant::now();

        let mut accounts = self
            .state_engine
            .marginfi_accounts
            .iter()
            .filter_map(|account| {
                let account = account.value();

                if !account.read().unwrap().has_liabs() {
                    return None;
                }

                let (max_liquidation_amount, profit) = account
                    .read()
                    .unwrap()
                    .compute_max_liquidatable_asset_amount()
                    .ok()?;

                if max_liquidation_amount.is_zero() || profit < self.config.min_profit {
                    return None;
                }

                Some((account.clone(), (max_liquidation_amount, profit)))
            })
            .collect::<Vec<_>>();

        accounts.sort_by(|(_, (_, profit_a)), (_, (_, profit_b))| profit_a.cmp(profit_b));

        accounts
            .iter()
            .rev()
            .take(10)
            .for_each(|(account, (lv, profit))| {
                info!(
                    "Account {} liquidatable amount: {}, profit: {}",
                    account.read().unwrap().address,
                    lv,
                    profit
                );
            });

        let unhealty_top_10 = accounts.iter().rev().take(10).collect::<Vec<_>>();

        let end = start.elapsed();

        debug!(
            "Processed accounts {} in {:?}",
            self.state_engine.marginfi_accounts.len(),
            end
        );

        let first = unhealty_top_10.first();

        if let Some((account, _)) = first {
            info!("Liquidating account {}", account.read().unwrap().address);
            self.liquidate_account(account.clone())?;

            return Ok(true);
        } else {
            debug!("No accounts to liquidate");
        }

        Ok(false)
    }

    fn liquidate_account(
        &self,
        liquidate_account: Arc<RwLock<MarginfiAccountWrapper>>,
    ) -> Result<(), ProcessorError> {
        let (asset_bank_pk, liab_bank_pk, max_asset_liquidation_amount) = {
            let account = liquidate_account
                .read()
                .map_err(|_| ProcessorError::FailedToReadAccount)?;

            let (assets_bank, liab_bank) = account.find_liquidaiton_bank_canididates()?;

            let (max_liquidation_amount, _) = account
                .compute_max_liquidatable_asset_amount_with_banks(
                    self.state_engine.banks.clone(),
                    &assets_bank,
                    &liab_bank,
                )?;

            (assets_bank, liab_bank, max_liquidation_amount)
        };

        // Max amount of liability the liquidator can cover
        let max_liab_coverage_amount = self.get_max_borrow_for_bank(&liab_bank_pk)?;

        let liab_bank_ref = self
            .state_engine
            .banks
            .get(&liab_bank_pk)
            .ok_or(ProcessorError::Error("Failed to get bank"))?;

        let liab_bank = liab_bank_ref
            .read()
            .map_err(|_| ProcessorError::Error("Failed to get bank"))?;

        let asset_bank_ref = self
            .state_engine
            .banks
            .get(&asset_bank_pk)
            .ok_or(ProcessorError::Error("Failed to get bank"))?;

        let asset_bank = asset_bank_ref
            .read()
            .map_err(|_| ProcessorError::Error("Failed to get bank"))?;

        debug!(
            "Max liquidatable amount: {} of {} for {}",
            max_asset_liquidation_amount, asset_bank.bank.mint, liab_bank.bank.mint
        );

        // Max USD amount the liquidator can cover
        let mut liquidator_capacity = liab_bank.calc_value(
            max_liab_coverage_amount,
            BalanceSide::Liabilities,
            RequirementType::Initial,
        )?;

        if let Some(max_liquidation_value) = self.config.max_liquidation_value {
            liquidator_capacity = min(liquidator_capacity, I80F48::from_num(max_liquidation_value));
        }

        debug!("Liquidator capacity: ${}", liquidator_capacity);

        let liquidation_asset_amount_capacity = asset_bank.calc_amount(
            liquidator_capacity,
            BalanceSide::Assets,
            RequirementType::Initial,
        )?;

        let asset_amount_to_liquidate = min(
            max_asset_liquidation_amount,
            liquidation_asset_amount_capacity,
        );

        let slippage_adjusted_asset_amount = asset_amount_to_liquidate * I80F48!(0.98);

        info!(
            "Liquidating {} of {} for {}",
            slippage_adjusted_asset_amount, asset_bank.bank.mint, liab_bank.bank.mint
        );

        drop(liab_bank);
        drop(liab_bank_ref);
        drop(asset_bank);
        drop(asset_bank_ref);

        self.liquidator_account.liquidate(
            liquidate_account,
            asset_bank_pk,
            liab_bank_pk,
            slippage_adjusted_asset_amount.to_num(),
            self.config.get_tx_config(),
        )?;

        Ok(())
    }

    fn process_account(
        &self,
        account: &Arc<RwLock<MarginfiAccountWrapper>>,
    ) -> Result<(), ProcessorError> {
        let account = account
            .read()
            .map_err(|_| ProcessorError::FailedToReadAccount)?;

        if !account.has_liabs() {
            return Ok(());
        }

        let (assets, liabs) = account.calc_health(RequirementType::Maintenance);

        if liabs > assets {
            info!(
                "Account {} can be liquidated health: {}, {} < {}",
                account.address,
                assets - liabs,
                assets,
                liabs
            );
        }

        Ok(())
    }

    pub fn get_free_collateral(&self) -> Result<I80F48, ProcessorError> {
        let account = self.get_liquidator_account()?;
        let (assets, liabs) = account.calc_health(RequirementType::Initial);

        if assets > liabs {
            Ok(assets - liabs)
        } else {
            Ok(I80F48!(0))
        }
    }

    pub fn get_max_withdraw_for_bank(
        &self,
        bank_pk: &Pubkey,
    ) -> Result<(I80F48, bool), ProcessorError> {
        let free_collateral = self.get_free_collateral()?;
        let balance = self
            .get_liquidator_account()?
            .get_balance_for_bank(bank_pk)?;

        debug!("Free collateral: {}", free_collateral);

        Ok(match balance {
            Some((balance, BalanceSide::Assets)) => {
                let value = self.get_value(
                    balance,
                    &bank_pk,
                    RequirementType::Initial,
                    BalanceSide::Assets,
                )?;
                let max_withdraw = value.min(free_collateral);

                trace!("Balance {}", balance);

                trace!(
                    "Max withdraw for bank {}: {} (balance_value: {} free_collateral: {})",
                    bank_pk,
                    max_withdraw,
                    value,
                    free_collateral
                );

                (
                    self.get_amount(max_withdraw, bank_pk, Some(PriceBias::Low))?,
                    value <= free_collateral,
                )
            }
            _ => (I80F48!(0), false),
        })
    }

    pub fn get_max_borrow_for_bank(&self, bank_pk: &Pubkey) -> Result<I80F48, ProcessorError> {
        let free_collateral = self.get_free_collateral()?;

        let bank_ref = self
            .state_engine
            .banks
            .get(bank_pk)
            .ok_or(ProcessorError::Error("Failed to get bank"))?
            .clone();

        let bank = bank_ref
            .read()
            .map_err(|_| ProcessorError::Error("Failed to get bank"))?;

        let (asset_amount, _) = self
            .liquidator_account
            .account_wrapper
            .read()
            .map_err(|_| ProcessorError::FailedToReadAccount)?
            .get_balance_for_bank_2(bank_pk)?;

        let untied_collateral_for_bank = min(
            free_collateral,
            bank.calc_value(asset_amount, BalanceSide::Assets, RequirementType::Initial)?,
        );

        let asset_weight: I80F48 = bank.bank.config.asset_weight_init.into();
        let liab_weight: I80F48 = bank.bank.config.liability_weight_init.into();

        let lower_price = bank
            .oracle_adapter
            .price_adapter
            .get_price_of_type(OraclePriceType::TimeWeighted, Some(PriceBias::Low))
            .map_err(|_| ProcessorError::Error("Failed to get price"))?;

        let higher_price = bank
            .oracle_adapter
            .price_adapter
            .get_price_of_type(OraclePriceType::TimeWeighted, Some(PriceBias::High))
            .map_err(|_| ProcessorError::Error("Failed to get price"))?;

        let token_decimals = bank.bank.mint_decimals as usize;

        let max_borrow_amount = if asset_weight == I80F48::ZERO {
            let max_additional_borrow_ui =
                (free_collateral - untied_collateral_for_bank) / (higher_price * liab_weight);

            let max_additional = max_additional_borrow_ui * EXP_10_I80F48[token_decimals];

            max_additional + asset_amount
        } else {
            let ui_amount = untied_collateral_for_bank / (lower_price * asset_weight)
                + (free_collateral - untied_collateral_for_bank) / (higher_price * liab_weight);

            ui_amount * EXP_10_I80F48[token_decimals]
        };

        debug!("Max borrow for bank {}: {}", bank_pk, max_borrow_amount);

        Ok(max_borrow_amount)
    }

    async fn swap(
        &self,
        amount: u64,
        src_bank: &Pubkey,
        dst_bank: &Pubkey,
    ) -> Result<(), ProcessorError> {
        let src_mint = {
            let bank_ref = self
                .state_engine
                .banks
                .get(&src_bank)
                .ok_or(ProcessorError::Error("Failed to get bank"))?;

            let bank_w = bank_ref
                .read()
                .map_err(|_| ProcessorError::Error("Failed to get bank"))?;

            bank_w.bank.mint
        };

        let dst_mint = {
            let bank_ref = self
                .state_engine
                .banks
                .get(&dst_bank)
                .ok_or(ProcessorError::Error("Failed to get bank"))?;

            let bank_w = bank_ref
                .read()
                .map_err(|_| ProcessorError::Error("Failed to get bank"))?;

            bank_w.bank.mint
        };

        info!("Swapping {} from {} to {}", amount, src_mint, dst_mint);

        let jup_swap_client = JupiterSwapApiClient::new(self.config.jup_swap_api_url.clone());

        debug!("Requesting quote for swap");
        let quote_response = jup_swap_client
            .quote(&QuoteRequest {
                input_mint: src_mint,
                output_mint: dst_mint,
                amount,
                slippage_bps: self.config.slippage_bps,
                ..Default::default()
            })
            .await
            .map_err(|e| {
                error!("Failed to get quote: {:?}", e);
                ProcessorError::Error("Failed to get quote")
            })?;

        debug!("Received quote for swap: {:?}", quote_response);

        debug!("Swapping tokens");
        let swap = jup_swap_client
            .swap(&SwapRequest {
                user_public_key: self.signer_keypair.pubkey(),
                quote_response,
                config: TransactionConfig {
                    wrap_and_unwrap_sol: false,
                    compute_unit_price_micro_lamports: self
                        .config
                        .compute_unit_price_micro_lamports
                        .map(|v| ComputeUnitPriceMicroLamports::MicroLamports(v)),
                    ..Default::default()
                },
            })
            .await
            .map_err(|e| {
                error!("Failed to swap: {:?}", e);
                ProcessorError::Error("Failed to swap")
            })?;

        debug!("Deserializing swap transaction");
        let mut tx =
            bincode::deserialize::<VersionedTransaction>(&swap.swap_transaction).map_err(|_| {
                error!("Failed to deserialize swap transaction");
                ProcessorError::Error("Failed to deserialize swap transaction")
            })?;

        let recent_blockhash = self
            .state_engine
            .rpc_client
            .get_latest_blockhash()
            .map_err(|e| {
                error!("Failed to get latest blockhash: {:?}", e);
                ProcessorError::Error("Failed to get latest blockhash")
            })?;

        tx.message.set_recent_blockhash(recent_blockhash);

        debug!("Signing swap transaction");
        let tx = VersionedTransaction::try_new(tx.message, &[self.signer_keypair.as_ref()])
            .map_err(|e| {
                error!("Failed to sign swap transaction: {:?}", e);
                ProcessorError::Error("Failed to sign swap transaction")
            })?;

        debug!("Sending swap transaction");
        aggressive_send_tx(
            self.state_engine.rpc_client.clone(),
            &tx,
            SenderCfg::DEFAULT,
        )
        .map_err(|e| {
            error!("Failed to send swap transaction: {:?}", e);
            ProcessorError::Error("Failed to send swap transaction")
        })?;

        debug!("Swap completed successfully");

        Ok(())
    }
}

fn get_liquidator_seed(signer: Pubkey, mint: Pubkey, seed: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(signer.as_ref());
    hasher.update(mint.as_ref());
    hasher.update(seed);
    hasher.finalize().try_into().unwrap()
}

fn get_keypair_for_token_account(
    signer: Pubkey,
    mint: Pubkey,
    seed: &[u8],
) -> Result<Keypair, Box<dyn Error>> {
    let keypair_seed = get_liquidator_seed(signer, mint, seed);
    Keypair::from_seed(&keypair_seed)
}

fn get_address_for_token_account(
    signer: Pubkey,
    mint: Pubkey,
    seed: &[u8],
) -> Result<Pubkey, Box<dyn Error>> {
    let keypair = get_keypair_for_token_account(signer, mint, seed)?;
    Ok(keypair.pubkey())
}
