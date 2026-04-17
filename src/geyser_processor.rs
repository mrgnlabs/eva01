use crate::{
    cache::Cache,
    geyser::{AccountType, GeyserUpdate},
    metrics::{GEYSER_TRIGGERED_SCANS_TOTAL, GEYSER_UPDATES_TOTAL},
    utils::log_genuine_error,
    wrappers::marginfi_account::MarginfiAccountWrapper,
};
use anyhow::Result;
use crossbeam::channel::Receiver;
use log::{debug, error, info};
use marginfi_type_crate::types::MarginfiAccount;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub struct GeyserProcessor {
    geyser_rx: Receiver<GeyserUpdate>,
    run_liquidation: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    cache: Arc<Cache>,
}

impl GeyserProcessor {
    pub fn new(
        geyser_rx: Receiver<GeyserUpdate>,
        run_liquidation: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        cache: Arc<Cache>,
    ) -> Result<Self> {
        Ok(Self {
            geyser_rx,
            run_liquidation,
            stop,
            cache,
        })
    }

    pub fn start(&self) -> Result<()> {
        info!("Staring the GeyserProcessor loop.");
        while !self.stop.load(Ordering::Relaxed) {
            match self.geyser_rx.recv() {
                Ok(geyser_update) => {
                    if let Err(error) = self.process_update(geyser_update) {
                        log_genuine_error("Failed to process Geyser update", error);
                    }
                }
                Err(error) => {
                    error!("Geyser processor error: {}!", error);
                }
            }
        }
        info!("The GeyserProcessor loop is stopped.");
        Ok(())
    }

    fn process_update(&self, msg: GeyserUpdate) -> Result<()> {
        let msg_account = msg.account.clone();
        debug!(
            "Processing the {:?} {:?} update.",
            msg.account_type, msg.address
        );

        match msg.account_type {
            AccountType::Oracle => {
                self.cache.oracles.try_update(&msg.address, msg_account)?;

                self.run_liquidation.store(true, Ordering::Relaxed);
                GEYSER_TRIGGERED_SCANS_TOTAL.inc();
            }
            AccountType::Marginfi => {
                let marginfi_account =
                    bytemuck::from_bytes::<MarginfiAccount>(&msg.account.data[8..]);
                self.cache
                    .marginfi_accounts
                    .try_insert(MarginfiAccountWrapper::new(msg.address, *marginfi_account))?;

                self.run_liquidation.store(true, Ordering::Relaxed);
                GEYSER_TRIGGERED_SCANS_TOTAL.inc();
            }
            AccountType::Token => {
                self.cache
                    .tokens
                    .try_update_account(msg.address, msg.account)?;
            }
        }
        GEYSER_UPDATES_TOTAL.inc();
        Ok(())
    }
}
