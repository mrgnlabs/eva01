use crate::{
    cache::CacheT,
    clock_manager,
    geyser::{AccountType, GeyserUpdate},
    thread_debug, thread_error, thread_info,
    utils::load_swb_pull_account_from_bytes,
    wrappers::{marginfi_account::MarginfiAccountWrapper, oracle::OracleWrapperTrait},
};
use anyhow::Result;
use crossbeam::channel::Receiver;
use marginfi::{
    errors::MarginfiError,
    state::{
        marginfi_account::MarginfiAccount,
        price::{OraclePriceFeedAdapter, OracleSetup, SwitchboardPullPriceFeed},
    },
};
use solana_sdk::{account_info::IntoAccountInfo, clock::Clock};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use switchboard_on_demand_client::PullFeedAccountData;

pub struct GeyserProcessor<T: OracleWrapperTrait + Clone> {
    geyser_rx: Receiver<GeyserUpdate>,
    run_liquidation: Arc<AtomicBool>,
    run_rebalance: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    clock: Arc<Mutex<Clock>>,
    cache: Arc<CacheT<T>>,
}

impl<T: OracleWrapperTrait + Clone> GeyserProcessor<T> {
    pub fn new(
        geyser_rx: Receiver<GeyserUpdate>,
        run_liquidation: Arc<AtomicBool>,
        run_rebalance: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        clock: Arc<Mutex<Clock>>,
        cache: Arc<CacheT<T>>,
    ) -> Result<Self> {
        Ok(Self {
            geyser_rx,
            run_liquidation,
            run_rebalance,
            stop,
            clock,
            cache,
        })
    }

    pub fn start(&self) -> Result<()> {
        thread_info!("Staring the GeyserProcessor loop.");
        while !self.stop.load(Ordering::Relaxed) {
            match self.geyser_rx.recv() {
                Ok(geyser_update) => {
                    if let Err(error) = self.process_update(geyser_update) {
                        log_error(error);
                    }
                }
                Err(error) => {
                    thread_error!("Geyser processor error: {}!", error);
                }
            }
        }
        thread_info!("The GeyserProcessor loop is stopped.");
        Ok(())
    }

    fn process_update(&self, msg: GeyserUpdate) -> Result<()> {
        let mut msg_account = msg.account.clone();
        thread_debug!(
            "Processing the {:?} {:?} update.",
            msg.account_type,
            msg.address
        );

        match msg.account_type {
            AccountType::Oracle => {
                let bank_address = self.cache.oracles.try_get_bank_from_oracle(&msg.address)?;
                let bank = self.cache.banks.try_get_bank(&bank_address)?;

                let oracle_price_adapter = match bank.config.oracle_setup {
                    OracleSetup::SwitchboardPull => {
                        let mut offsets_data = [0u8; std::mem::size_of::<PullFeedAccountData>()];
                        offsets_data.copy_from_slice(
                            &msg.account.data[8..std::mem::size_of::<PullFeedAccountData>() + 8],
                        );
                        let swb_feed = load_swb_pull_account_from_bytes(&offsets_data)?;

                        OraclePriceFeedAdapter::SwitchboardPull(SwitchboardPullPriceFeed {
                            feed: Box::new((&swb_feed).into()),
                        })
                    }
                    OracleSetup::StakedWithPythPush => {
                        let mut accounts_info =
                            vec![(&msg.address, &mut msg_account).into_account_info()];

                        let keys = &bank.config.oracle_keys[1..3];
                        let mut owned_accounts = self.cache.oracles.get_accounts(keys);
                        accounts_info.extend(
                            keys.iter()
                                .zip(owned_accounts.iter_mut())
                                .map(|(key, account)| (key, &mut account.1).into_account_info()),
                        );

                        let clock = clock_manager::get_clock(&self.clock)?;
                        OraclePriceFeedAdapter::try_from_bank_config(
                            &bank.config,
                            &accounts_info,
                            &clock,
                        )?
                    }
                    _ => {
                        let clock = clock_manager::get_clock(&self.clock)?;
                        let oracle_account_info =
                            (&msg.address, &mut msg_account).into_account_info();

                        OraclePriceFeedAdapter::try_from_bank_config(
                            &bank.config,
                            &[oracle_account_info],
                            &clock,
                        )?
                    }
                };

                self.cache
                    .oracles
                    .try_update_account_wrapper(&msg.address, oracle_price_adapter)?;

                self.run_liquidation.store(true, Ordering::Relaxed);
                self.run_rebalance.store(true, Ordering::Relaxed);
            }
            AccountType::Marginfi => {
                let marginfi_account =
                    bytemuck::from_bytes::<MarginfiAccount>(&msg.account.data[8..]);

                thread_debug!("Received Marginfi account {:?} update", msg.address);
                self.cache.marginfi_accounts.try_insert({
                    MarginfiAccountWrapper::new(msg.address, marginfi_account.lending_account)
                })?;

                self.run_liquidation.store(true, Ordering::Relaxed);
                self.run_rebalance.store(true, Ordering::Relaxed);
            }
            AccountType::Token => {
                thread_debug!("Received Token account update for {:?}", &msg.address);
                self.cache
                    .tokens
                    .try_update_account(msg.address, msg.account)?;

                self.run_rebalance.store(true, Ordering::Relaxed);
            }
        }
        Ok(())
    }
}

fn log_error(error: anyhow::Error) {
    match error.downcast_ref::<MarginfiError>() {
        Some(mfi_error) => match mfi_error {
            MarginfiError::SwitchboardStalePrice | MarginfiError::PythPushStalePrice => {
                thread_debug!("Discarding the stale price Geyser update! {}", error);
            }
            _ => {
                thread_error!("Failed to process Geyser update! {}", error);
            }
        },
        None => thread_error!("Failed to process Geyser update! {}", error),
    }
}
#[cfg(test)]
mod tests {
    use crate::{
        cache::test_utils::create_test_cache, wrappers::bank::test_utils::TestBankWrapper,
    };

    use super::*;
    use crossbeam::channel::unbounded;
    use solana_sdk::{account::Account, clock::Clock, pubkey::Pubkey};
    use std::sync::{atomic::AtomicBool, Arc, Mutex};

    #[test]
    fn test_geyser_processor_new() {
        let (_, receiver) = unbounded();
        let run_liquidation = Arc::new(AtomicBool::new(false));
        let run_rebalance = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let clock = Arc::new(Mutex::new(Clock::default()));
        let cache = Arc::new(create_test_cache(&Vec::new()));

        let processor = GeyserProcessor::new(
            receiver,
            run_liquidation.clone(),
            run_rebalance.clone(),
            stop.clone(),
            clock.clone(),
            cache.clone(),
        );

        assert!(processor.is_ok());
    }

    #[test]
    fn test_geyser_processor_start_stop() {
        let (_, receiver) = unbounded();
        let run_liquidation = Arc::new(AtomicBool::new(false));
        let run_rebalance = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let clock = Arc::new(Mutex::new(Clock::default()));
        let cache = Arc::new(create_test_cache(&Vec::new()));

        let processor = GeyserProcessor::new(
            receiver,
            run_liquidation.clone(),
            run_rebalance.clone(),
            stop.clone(),
            clock.clone(),
            cache.clone(),
        )
        .unwrap();

        // Simulate stopping the processor
        stop.store(true, Ordering::Relaxed);
        let result = processor.start();
        assert!(result.is_ok());
    }

    #[test]
    fn test_process_update_token() {
        let (_, receiver) = unbounded();
        let run_liquidation = Arc::new(AtomicBool::new(false));
        let run_rebalance = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let clock = Arc::new(Mutex::new(Clock::default()));

        let sol_bank = TestBankWrapper::test_sol();
        let usdc_bank = TestBankWrapper::test_usdc();
        let mut cache = create_test_cache(&vec![sol_bank.clone(), usdc_bank.clone()]);

        let token_address = Pubkey::new_unique();
        cache
            .tokens
            .try_insert(token_address, Account::default(), sol_bank.bank.mint)
            .unwrap();

        let cache = Arc::new(cache);

        let processor = GeyserProcessor::new(
            receiver,
            run_liquidation.clone(),
            run_rebalance.clone(),
            stop.clone(),
            clock.clone(),
            cache.clone(),
        )
        .unwrap();

        let geyser_update = GeyserUpdate {
            account_type: AccountType::Token,
            address: token_address,
            account: Default::default(),
        };

        let result = processor.process_update(geyser_update);
        result.unwrap();
    }
}
