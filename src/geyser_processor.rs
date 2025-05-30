use crate::{
    cache::Cache,
    geyser::{AccountType, GeyserUpdate},
    thread_debug, thread_error, thread_info,
    utils::log_genuine_error,
    wrappers::marginfi_account::MarginfiAccountWrapper,
};
use anyhow::Result;
use crossbeam::channel::Receiver;
use marginfi::state::marginfi_account::MarginfiAccount;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub struct GeyserProcessor {
    geyser_rx: Receiver<GeyserUpdate>,
    run_liquidation: Arc<AtomicBool>,
    run_rebalance: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    cache: Arc<Cache>,
}

impl GeyserProcessor {
    pub fn new(
        geyser_rx: Receiver<GeyserUpdate>,
        run_liquidation: Arc<AtomicBool>,
        run_rebalance: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        cache: Arc<Cache>,
    ) -> Result<Self> {
        Ok(Self {
            geyser_rx,
            run_liquidation,
            run_rebalance,
            stop,
            cache,
        })
    }

    pub fn start(&self) -> Result<()> {
        thread_info!("Staring the GeyserProcessor loop.");
        while !self.stop.load(Ordering::Relaxed) {
            match self.geyser_rx.recv() {
                Ok(geyser_update) => {
                    if let Err(error) = self.process_update(geyser_update) {
                        log_genuine_error("Failed to process Geyser update", error);
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
        let msg_account = msg.account.clone();
        thread_debug!(
            "Processing the {:?} {:?} update.",
            msg.account_type,
            msg.address
        );

        match msg.account_type {
            AccountType::Oracle => {
                thread_debug!("Received Oracle account {:?} update", msg.address);
                self.cache.oracles.try_update(&msg.address, msg_account)?;

                self.run_liquidation.store(true, Ordering::Relaxed);
                self.run_rebalance.store(true, Ordering::Relaxed);
            }
            AccountType::Marginfi => {
                thread_debug!("Received Marginfi account {:?} update", msg.address);
                let marginfi_account =
                    bytemuck::from_bytes::<MarginfiAccount>(&msg.account.data[8..]);
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

#[cfg(test)]
mod tests {
    use crate::{
        cache::test_utils::create_test_cache, wrappers::bank::test_utils::TestBankWrapper,
    };

    use super::*;
    use crossbeam::channel::unbounded;
    use solana_sdk::{account::Account, pubkey::Pubkey};
    use std::sync::{atomic::AtomicBool, Arc};

    #[test]
    fn test_geyser_processor_new() {
        let (_, receiver) = unbounded();
        let run_liquidation = Arc::new(AtomicBool::new(false));
        let run_rebalance = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let cache = Arc::new(create_test_cache(&Vec::new()));

        let processor = GeyserProcessor::new(
            receiver,
            run_liquidation.clone(),
            run_rebalance.clone(),
            stop.clone(),
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
        let cache = Arc::new(create_test_cache(&Vec::new()));

        let processor = GeyserProcessor::new(
            receiver,
            run_liquidation.clone(),
            run_rebalance.clone(),
            stop.clone(),
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
