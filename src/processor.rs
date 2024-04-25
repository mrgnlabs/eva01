use std::{
    collections::HashSet,
    error::Error,
    sync::{Arc, RwLock, RwLockReadGuard},
    thread::{self, JoinHandle},
};

use crossbeam::channel::Receiver;
use fixed::{traits::Fixed, types::I80F48};
use fixed_macro::types::I80F48;
use log::{debug, error, info, warn};
use marginfi::{
    constants::EXP_10_I80F48,
    state::{
        marginfi_account::{BalanceSide, RequirementType},
        price::{PriceAdapter, PriceBias},
    },
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use solana_sdk::{
    pubkey,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair},
    signer::{SeedDerivable, Signer},
};

use crate::{
    marginfi_account::MarginfiAccountError,
    state_engine::{
        engine::StateEngineService,
        marginfi_account::{MarginfiAccountWrapper, MarginfiAccountWrapperError},
    },
    utils::{fixed_from_float, from_pubkey_string, from_vec_str_to_pubkey},
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
    pub slippage_bps: u64,
}

impl EvaLiquidatorCfg {
    pub fn default_token_account_dust_threshold() -> I80F48 {
        I80F48!(0.01)
    }

    pub fn default_max_sol_balance() -> I80F48 {
        I80F48!(1)
    }

    pub fn default_preferred_mints() -> Vec<Pubkey> {
        vec![
            pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
            pubkey!("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB"),
        ]
    }

    pub fn default_swap_mint() -> Pubkey {
        pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v")
    }

    pub fn default_jup_swap_api_url() -> String {
        "https://quote-api.jup.ag/v6".to_string()
    }

    pub fn default_slippage_bps() -> u64 {
        250
    }
}

pub struct EvaLiquidator {
    // liquidator_account: Arc<RwLock<MarginfiAccountWrapper>>,
    liquidator_account: crate::marginfi_account::MarginfiAccount,
    state_engine: Arc<StateEngineService>,
    update_rx: Receiver<()>,
    signer_keypair: Arc<Keypair>,
    cfg: EvaLiquidatorCfg,
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
                    cfg,
                    preferred_mints,
                    swap_mint_bank_pk,
                };

                if let Err(e) = processor.run() {
                    error!("Error running processor: {:?}", e);
                }

                warn!("Processor thread exiting");

                Ok(())
            })
            .map_err(|_| ProcessorError::SetupFailed)
    }

    fn run(&self) -> Result<(), ProcessorError> {
        loop {
            if self.needs_to_be_rebalanced() {}

            while let Ok(_) = self.update_rx.recv() {
                match self.calc_health_for_all_accounts() {
                    Err(e) => {
                        error!("Error processing accounts: {:?}", e);
                    }
                    _ => {}
                };
            }
        }

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
                    value > self.cfg.token_account_dust_threshold
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
        bank_pk: Pubkey,
    ) -> Result<Option<I80F48>, ProcessorError> {
        let mint = self
            .state_engine
            .banks
            .get(&bank_pk)
            .and_then(|bank| bank.read().ok().map(|bank| bank.bank.mint));

        if mint.is_none() {
            warn!("No mint found for bank {}", bank_pk);
            return Ok(None);
        }

        let mint = mint.unwrap();

        let token_account = self
            .state_engine
            .token_account_manager
            .get_address_for_mint(mint);

        if token_account.is_none() {
            warn!("No token account found for mint {}", mint);
            return Ok(None);
        }

        let token_account = token_account.unwrap();

        let balance = self
            .state_engine
            .token_accounts
            .get(&token_account)
            .and_then(|account| account.read().ok().map(|account| account.get_amount()));

        if balance.is_none() {
            warn!("No balance found for token account {}", token_account);
            return Ok(None);
        }

        Ok(balance)
    }

    fn replay_liabilities(&self) -> Result<(), ProcessorError> {
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
            self.repay_liability(bank_pk)?;
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
    fn repay_liability(&self, bank_pk: Pubkey) -> Result<(), ProcessorError> {
        let balance = self
            .get_liquidator_account()?
            .get_balance_for_bank(&bank_pk)?;

        if matches!(balance, None) || matches!(balance, Some((_, BalanceSide::Assets))) {
            warn!("No liability found for bank {}", bank_pk);
            return Ok(());
        }

        let (balance, _) = balance.unwrap();

        debug!("Found liability of {} for bank {}", balance, bank_pk);

        let token_balance = self
            .get_token_balance_for_bank(bank_pk)?
            .unwrap_or_default();

        if !token_balance.is_zero() {
            debug!(
                "Found token balance of {} for bank {}",
                token_balance, bank_pk
            );
        }

        let liab_to_purchase = balance - token_balance;

        debug!("Liability to purchase: {}", liab_to_purchase);

        if !liab_to_purchase.is_zero() {
            let liab_usd_value = self.get_value(liab_to_purchase, &bank_pk, None)?;

            debug!("Liability value: ${}", liab_usd_value);

            let required_swap_token =
                self.get_amount(liab_usd_value, &self.swap_mint_bank_pk, None)?;

            debug!(
                "Required swap token amount: {} for ${}",
                required_swap_token, liab_usd_value
            );

            let swap_token_balance = self
                .get_token_balance_for_bank(self.swap_mint_bank_pk)?
                .unwrap_or_default();

            // Log if token balance is > 0
            if !swap_token_balance.is_zero() {
                debug!(
                    "Found swap token balance of {} for bank {}",
                    swap_token_balance, self.swap_mint_bank_pk
                );
            }

            // Token balance to withdraw
            let token_balance_to_withdraw = required_swap_token - swap_token_balance;

            // Log if token balance to withdraw is > 0
            if !token_balance_to_withdraw.is_zero() {
                debug!(
                    "Token balance to withdraw: {} for bank {}",
                    token_balance_to_withdraw, self.swap_mint_bank_pk
                );
            }

            // Withdraw token balance
        }

        Ok(())
    }

    fn sell_non_preferred_deposits(&self) -> Result<(), ProcessorError> {
        debug!("Selling non-preferred deposits");

        let non_preferred_deposits = self
            .liquidator_account
            .account_wrapper
            .read()
            .map_err(|_| ProcessorError::FailedToReadAccount)?
            .get_deposits(&self.cfg.preferred_mints)
            .map_err(|_| ProcessorError::FailedToReadAccount)?;

        if non_preferred_deposits.is_empty() {
            debug!("No non-preferred deposits to sell");
            return Ok(());
        }

        info!("Selling non-preferred deposits");

        for (_, bank_pk) in non_preferred_deposits {
            self.withdraw_and_sell_deposit(&bank_pk)?;
        }

        Ok(())
    }

    fn withdraw_and_sell_deposit(&self, bank_pk: &Pubkey) -> Result<(), ProcessorError> {
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

        self.liquidator_account
            .withdraw(bank_pk, withdraw_amount.to_num(), Some(withdraw_all))?;

        Ok(())
    }

    pub fn get_value(
        &self,
        amount: I80F48,
        bank_pk: &Pubkey,
        bias: Option<PriceBias>,
    ) -> Result<I80F48, ProcessorError> {
        let bank_ref = self
            .state_engine
            .get_bank(bank_pk)
            .ok_or(ProcessorError::Error("Failed to get bank"))?;

        let bank = bank_ref
            .read()
            .map_err(|_| ProcessorError::Error("Failed to get bank"))?;

        let amount_ui = amount / EXP_10_I80F48[bank.bank.mint_decimals as usize];

        let price = bank
            .oracle_adapter
            .price_adapter
            .get_price_of_type(marginfi::state::price::OraclePriceType::RealTime, bias)
            .map_err(|_| ProcessorError::Error("Failed to get price"))?;

        Ok(amount_ui * price)
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

    fn calc_health_for_all_accounts(&self) -> Result<(), ProcessorError> {
        self.state_engine.marginfi_accounts.iter().try_for_each(
            |account| -> Result<(), ProcessorError> {
                self.process_account(&account)?;

                Ok(())
            },
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

        Ok(match balance {
            Some((balance, BalanceSide::Assets)) => {
                let value =
                    self.get_value(balance, &self.swap_mint_bank_pk, Some(PriceBias::Low))?;
                let max_withdraw = value.min(free_collateral);

                (
                    self.get_amount(max_withdraw, bank_pk, Some(PriceBias::Low))?,
                    value <= free_collateral,
                )
            }
            _ => (I80F48!(0), false),
        })
    }

    async fn swap(
        &self,
        amount: u64,
        src_bank: Pubkey,
        dst_bank: Pubkey,
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

        let request_url = format!(
            "{}/quote?inputMint={}&outputMint={}&amount={}&slippageBps={}",
            self.cfg.jup_swap_api_url, src_mint, dst_mint, amount, self.cfg.slippage_bps
        );

        let res = reqwest::get(request_url).await?;

        if res.status().is_success() {
            let body = res.text().await?;
        }

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
