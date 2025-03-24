use bincode::deserialize;
//use log::debug;
use solana_client::rpc_client::RpcClient;
use solana_sdk::clock::Clock;
use solana_sdk::sysvar::{self};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct CachedClock {
    clock: Arc<Mutex<Clock>>,
    last_updated: Arc<Mutex<Instant>>,
    cache_duration: Duration,
}

impl CachedClock {
    pub fn new(cache_duration: Duration) -> Self {
        Self {
            clock: Arc::new(Mutex::new(Clock::default())),
            last_updated: Arc::new(Mutex::new(Instant::now() - cache_duration)),
            cache_duration,
        }
    }

    pub async fn get_clock(&self, rpc_client: &RpcClient) -> anyhow::Result<Clock> {
        let mut last_updated = self.last_updated.lock().unwrap();
        let mut clock = self.clock.lock().unwrap();

        // Check if the cache is stale
        if Instant::now() - *last_updated >= self.cache_duration {
            //debug!("Updating clock cache");
            let clock_account = rpc_client.get_account(&sysvar::clock::id())?;
            //debug!("Updating clock cache done");
            *clock = deserialize(&clock_account.data)?;
            *last_updated = Instant::now();
        }

        Ok(clock.clone())
    }
}
