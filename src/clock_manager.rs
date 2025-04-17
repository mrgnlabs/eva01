use bincode::deserialize;
use log::{debug, error, info};
use solana_client::rpc_client::RpcClient;
use solana_sdk::clock::Clock;
use solana_sdk::sysvar::{self};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub struct ClockManager {
    rpc_client: RpcClient,
    clock: Arc<Mutex<Clock>>,
    refresh_interval: Duration,
}

impl ClockManager {
    pub fn new(
        clock: Arc<Mutex<Clock>>,
        rpc_url: String,
        refresh_interval_sec: u64,
    ) -> anyhow::Result<Self> {
        info!("Initializing ClockManager with RPC URL: {}", rpc_url);

        let rpc_client = RpcClient::new(rpc_url);
        //        let clock = Arc::new(Mutex::new(fetch_clock(&rpc_client)?));
        let refresh_interval = Duration::from_secs(refresh_interval_sec);

        Ok(Self {
            rpc_client,
            clock,
            refresh_interval,
        })
    }

    pub fn start(&mut self) {
        info!(
            "Thread {:?}: Starting the ClockManager loop.",
            thread::current().id()
        );
        loop {
            std::thread::sleep(self.refresh_interval);
            debug!(
                "Thread {:?}: Updating the Solana Clock...",
                thread::current().id()
            );
            match fetch_clock(&self.rpc_client) {
                Ok(clock) => {
                    match self.clock.lock() {
                        Ok(mut clock_guard) => {
                            *clock_guard = clock;
                        }
                        Err(err) => {
                            error!(
                                "Thread {:?}: Failed to lock the clock mutex for update: {:?}",
                                thread::current().id(),
                                err
                            );
                        }
                    }
                    debug!(
                        "Thread {:?}: Updated the Solana Clock: {:?}",
                        thread::current().id(),
                        self.clock
                    );
                }
                Err(e) => {
                    error!(
                        "Thread {:?}: Failed to update the Solana Clock! {:?}",
                        thread::current().id(),
                        e
                    );
                }
            }
        }
    }
}

pub fn fetch_clock(rpc_client: &RpcClient) -> anyhow::Result<Clock> {
    let clock_account = rpc_client.get_account(&sysvar::clock::id())?;
    let clock = deserialize(&clock_account.data)?;
    Ok(clock)
}

pub fn get_clock(clock: &Arc<Mutex<Clock>>) -> anyhow::Result<Clock> {
    let clock_guard = clock
        .lock()
        .map_err(|e| anyhow::anyhow!("Failed to obtain the Clock lock: {:?}", e))?;
    Ok(clock_guard.clone())
}
