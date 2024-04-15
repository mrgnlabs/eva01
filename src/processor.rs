use std::{
    collections::HashSet,
    sync::{Arc, RwLock},
    thread::{self, JoinHandle},
};

use crossbeam::channel::Receiver;
use fixed::types::I80F48;
use fixed_macro::types::I80F48;
use log::{error, info};
use marginfi::state::marginfi_account::BalanceSide;
use solana_sdk::{
    blake3::Hash,
    pubkey,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair},
};

use crate::{
    state_engine::{
        self,
        engine::{MarginfiAccountWrapper, StateEngineService},
    },
    utils::{fixed_from_float, from_pubkey_string, from_vec_str_to_pubkey},
};

#[derive(thiserror::Error, Debug)]
pub enum ProcessorError {
    #[error("Failed to read account")]
    FailedToReadAccount,
    #[error("Failed to start liquidator")]
    SetupFailed,
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
        default = "EvaLiquidatorCfg::default_token_account_dust_threshold",
        deserialize_with = "fixed_from_float"
    )]
    pub max_sol_balance: I80F48,
    #[serde(
        default = "EvaLiquidatorCfg::default_preferred_mints",
        deserialize_with = "from_vec_str_to_pubkey"
    )]
    pub preferred_mints: Vec<Pubkey>,
}

impl EvaLiquidatorCfg {
    pub fn default_token_account_dust_threshold() -> I80F48 {
        I80F48!(0.01)
    }

    pub fn default_max_sol_balance() -> I80F48 {
        I80F48!(0.5)
    }

    pub fn default_preferred_mints() -> Vec<Pubkey> {
        vec![
            pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
            pubkey!("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB"),
        ]
    }
}

pub struct EvaLiquidator {
    liquidator_account: Arc<RwLock<MarginfiAccountWrapper>>,
    state_engine: Arc<StateEngineService>,
    update_rx: Receiver<()>,
    signer_keypair: Arc<Keypair>,
    cfg: EvaLiquidatorCfg,
    preferred_mints: HashSet<Pubkey>,
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
                let liquidator_account = state_engine
                    .marginfi_accounts
                    .get(&cfg.liquidator_account)
                    .ok_or(ProcessorError::SetupFailed)?
                    .clone();

                let keypair = Arc::new(
                    read_keypair_file(&cfg.keypair_path)
                        .map_err(|_| ProcessorError::SetupFailed)?,
                );

                let processor = EvaLiquidator {
                    state_engine,
                    update_rx,
                    liquidator_account,
                    signer_keypair: keypair,
                };

                processor.run();

                Ok(())
            })
            .map_err(|_| ProcessorError::SetupFailed)
    }

    fn run(&self) -> Result<(), ProcessorError> {
        loop {
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
    /// - User has excess SOL in their account
    fn needs_to_be_rebalanced(&self) -> bool {}

    fn has_tokens_in_token_accounts(&self) -> bool {
        self.state_engine.token_accounts.iter().any(|account| {
            account
                .read()
                .map_err(|_| ProcessorError::FailedToReadAccount)
                .map(|account| account.get_value().unwrap() > self.cfg.token_account_dust_threshold)
                .unwrap_or(false)
        })
    }

    fn has_liabilties(&self) -> bool {
        self.liquidator_account
            .read()
            .map_err(|_| ProcessorError::FailedToReadAccount)
            .map(|account| account.has_liabs())
            .unwrap_or(false)
    }

    fn has_non_preferred_deposits(&self) -> bool {
        self.liquidator_account
            .read()
            .map_err(|_| ProcessorError::FailedToReadAccount)
            .unwrap()
            .account
            .lending_account
            .balances
            .iter()
            .any(|balance| {
                let bank = self
                    .state_engine
                    .banks
                    .get(&balance.bank_pk)
                    .unwrap()
                    .read()
                    .unwrap();

                matches!(balance.get_side(), Some(BalanceSide::Assets))
                    && self.preferred_mints.contains(&bank.bank.mint)
            })
    }

    fn has_excess_sol(&self) -> bool {
        let signer = self.signer_keypair.pubkey();

        self.state_engine
            .sol_accounts
            .get(&signer)
            .map_or(false, |account| {
                account
                    .read()
                    .map_err(|_| ProcessorError::FailedToReadAccount)
                    .map(|account| {
                        account.get_value().unwrap() > self.cfg.token_account_dust_threshold
                    })
                    .unwrap_or(false)
            })
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

        let (assets, liabs) = account.calc_health();

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
}
