#![allow(dead_code)]

use crate::{config::GeneralConfig, crossbar::CrossbarMaintainer};
use anyhow::Result;
use solana_client::{
    nonblocking::rpc_client::RpcClient as NonBlockingRpcClient, rpc_client::RpcClient,
    rpc_config::RpcSendTransactionConfig,
};
use solana_sdk::{
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair},
    signer::Signer,
    transaction::VersionedTransaction,
};
use std::str::FromStr;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use switchboard_on_demand_client::{
    FetchUpdateManyParams, Gateway, PullFeed, QueueAccountData, SbContext,
};
use tokio::runtime::{Builder, Runtime};

//TODO: parametrize the Swb Program ID.
pub const SWB_PROGRAM_ID: &str = "A43DyUGA7s8eXPxqEjJY6EBu1KKbNgfxF8h17VAHn13w";

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
    rpc_client: RpcClient,
    non_blocking_rpc_client: NonBlockingRpcClient,
    swb_gateway: Gateway,
    payer: Keypair,
    simulation_is_running: Arc<AtomicBool>,
}

impl SwbCranker {
    pub fn new(config: &GeneralConfig) -> Result<Self> {
        let payer = read_keypair_file(&config.keypair_path).unwrap();

        let tokio_rt = Builder::new_multi_thread()
            .thread_name("SwbCranker")
            .worker_threads(2)
            .enable_all()
            .build()?;

        let rpc_client = RpcClient::new(config.rpc_url.clone());
        let non_blocking_rpc_client = NonBlockingRpcClient::new(config.rpc_url.clone());
        let queue = tokio_rt.block_on(QueueAccountData::load(
            &non_blocking_rpc_client,
            &Pubkey::from_str(SWB_PROGRAM_ID).unwrap(),
        ))?;
        let swb_gateway =
            tokio_rt.block_on(queue.fetch_gateways(&non_blocking_rpc_client))?[0].clone();

        Ok(Self {
            tokio_rt,
            crossbar_client: CrossbarMaintainer::new(),
            rpc_client,
            non_blocking_rpc_client,
            swb_gateway,
            payer,
            simulation_is_running: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn crank_oracles(&self, swb_oracles: Vec<Pubkey>) -> Result<()> {
        let (crank_ix, crank_lut) = self.tokio_rt.block_on(PullFeed::fetch_update_consensus_ix(
            SbContext::new(),
            &self.non_blocking_rpc_client,
            FetchUpdateManyParams {
                feeds: swb_oracles,
                payer: self.payer.pubkey(),
                gateway: self.swb_gateway.clone(),
                num_signatures: Some(1),
                //                    debug: Some(true),
                ..Default::default()
            },
        ))?;

        let blockhash = self.rpc_client.get_latest_blockhash()?;

        let txn = VersionedTransaction::try_new(
            VersionedMessage::V0(v0::Message::try_compile(
                &self.payer.pubkey(),
                &crank_ix,
                &crank_lut,
                blockhash,
            )?),
            &[&self.payer],
        )?;

        self.rpc_client.send_transaction_with_config(
            &txn,
            RpcSendTransactionConfig {
                skip_preflight: true,
                ..Default::default()
            },
        )?;

        Ok(())
    }

    // FIXME: simulation is halting the thread that is why it is currently disabled.
    // Will be addressed in https://linear.app/marginfi/issue/LIQ-16/overhaul-the-way-eva-is-obtaining-recent-oracle-prices
    /* Probably will be removed by the end of the sprint
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
                wrapper.simulated_price = price.to_f64();
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
    */
}
