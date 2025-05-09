use crate::{cache::Cache, crossbar::CrossbarMaintainer, thread_info};
use anyhow::Result;
use marginfi::state::price::OraclePriceFeedAdapter;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};
use tokio::runtime::{Builder, Runtime};

struct ResetFlag {
    flag: Arc<AtomicBool>,
}

impl Drop for ResetFlag {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::SeqCst);
    }
}

pub struct SwbCranker {
    tokio_rt: Runtime,
    crossbar_client: CrossbarMaintainer,
    cache: Arc<Cache>,
    simulation_is_running: Arc<AtomicBool>,
}

impl SwbCranker {
    pub fn new(cache: Arc<Cache>) -> Result<Self> {
        let tokio_rt = Builder::new_multi_thread()
            .thread_name("SwbPriceSimulator")
            .worker_threads(2)
            .enable_all()
            .build()?;
        Ok(Self {
            tokio_rt,
            crossbar_client: CrossbarMaintainer::new(),
            cache,
            simulation_is_running: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn simulate_swb_prices(&self) -> Result<()> {
        if !self.simulation_is_running.load(Ordering::SeqCst) {
            thread_info!("Simulating Swb prices...");

            self.simulation_is_running.store(true, Ordering::SeqCst);
            let _guard = ResetFlag {
                flag: self.simulation_is_running.clone(),
            };

            let swb_feed_hashes = self
                .cache
                .oracles
                .try_get_wrappers()?
                .iter()
                .filter_map(|oracle_wrapper| match &oracle_wrapper.price_adapter {
                    OraclePriceFeedAdapter::SwitchboardPull(price_feed) => Some((
                        oracle_wrapper.address,
                        hex::encode(price_feed.feed.feed_hash),
                    )),
                    _ => None,
                })
                .collect::<Vec<_>>();

            let simulated_prices = self
                .tokio_rt
                .block_on(self.crossbar_client.simulate(swb_feed_hashes));

            for (oracle_address, price) in simulated_prices {
                let mut wrapper = self.cache.oracles.try_get_wrapper(&oracle_address)?;
                wrapper.simulated_price = Some(price);
                self.cache
                    .oracles
                    .try_update_account_wrapper(&wrapper.address, wrapper.price_adapter)?;
            }
        } else {
            thread_info!("Swb price simulation is already running. Waiting for it to complete...");
            while self.simulation_is_running.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(1000));
            }
        }
        thread_info!("Swb price simulation is complete.");

        Ok(())
    }
}
