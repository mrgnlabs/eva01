use crate::{
    cache::Cache,
    clock_manager,
    geyser::{AccountType, GeyserUpdate},
    thread_debug, thread_error, thread_info,
    utils::load_swb_pull_account_from_bytes,
    wrappers::{bank::BankWrapper, marginfi_account::MarginfiAccountWrapper},
};
use anyhow::Result;
use crossbeam::channel::Receiver;
use marginfi::state::{
    marginfi_account::MarginfiAccount,
    price::{OraclePriceFeedAdapter, OracleSetup, SwitchboardPullPriceFeed},
};
use solana_sdk::{account_info::IntoAccountInfo, clock::Clock};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use switchboard_on_demand_client::PullFeedAccountData;

pub struct GeyserProcessor {
    geyser_rx: Receiver<GeyserUpdate>,
    run_liquidation: Arc<AtomicBool>,
    run_rebalance: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    clock: Arc<Mutex<Clock>>,
    cache: Arc<Cache>,
}

impl GeyserProcessor {
    pub fn new(
        geyser_rx: Receiver<GeyserUpdate>,
        run_liquidation: Arc<AtomicBool>,
        run_rebalance: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        clock: Arc<Mutex<Clock>>,
        cache: Arc<Cache>,
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
                        thread_error!("Failed to process Geyser update! {}", error);
                    }
                }
                Err(error) => {
                    thread_error!("Geyser procesor error: {}!", error);
                    break;
                }
            }
        }
        thread_info!("The GeyserProcessor loop is stopped.");
        Ok(())
    }

    fn process_update(&self, msg: GeyserUpdate) -> Result<()> {
        let mut msg_account = msg.account.clone();
        match msg.account_type {
            AccountType::Oracle => {
                let bank_address = self.cache.oracles.try_get_bank_from_oracle(&msg.address)?;
                let mut bank_to_update: BankWrapper =
                    self.cache.try_get_bank_wrapper(&bank_address)?;

                let oracle_price_adapter = match bank_to_update.bank.config.oracle_setup {
                    OracleSetup::SwitchboardPull => {
                        let mut offsets_data = [0u8; std::mem::size_of::<PullFeedAccountData>()];
                        offsets_data.copy_from_slice(
                            &msg.account.data[8..std::mem::size_of::<PullFeedAccountData>() + 8],
                        );
                        let swb_feed = load_swb_pull_account_from_bytes(&offsets_data).unwrap();

                        let feed_hash = hex::encode(swb_feed.feed_hash);
                        bank_to_update.oracle_adapter.swb_feed_hash = Some(feed_hash);

                        OraclePriceFeedAdapter::SwitchboardPull(SwitchboardPullPriceFeed {
                            feed: Box::new((&swb_feed).into()),
                        })
                    }
                    OracleSetup::StakedWithPythPush => {
                        thread_debug!(
                            "Getting update for STAKED oracle: {:?} for bank: {:?}",
                            msg.address,
                            bank_address
                        );
                        let mut accounts_info =
                            vec![(&msg.address, &mut msg_account).into_account_info()];

                        let keys = &bank_to_update.bank.config.oracle_keys[1..3];
                        let mut owned_accounts = self.cache.oracles.get_accounts(keys);
                        accounts_info.extend(
                            keys.iter()
                                .zip(owned_accounts.iter_mut())
                                .map(|(key, account)| (key, &mut account.1).into_account_info()),
                        );

                        let clock = clock_manager::get_clock(&self.clock)?;
                        OraclePriceFeedAdapter::try_from_bank_config(
                            &bank_to_update.bank.config,
                            &accounts_info,
                            &clock,
                        )?
                    }
                    _ => {
                        let clock = clock_manager::get_clock(&self.clock)?;
                        let oracle_account_info =
                            (&msg.address, &mut msg_account).into_account_info();
                        OraclePriceFeedAdapter::try_from_bank_config(
                            &bank_to_update.bank.config,
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
