#![allow(dead_code)]

use crate::{
    cache::Cache, clock_manager, config::GeneralConfig, thread_debug, thread_info,
    utils::load_swb_pull_account_from_bytes,
};
use anyhow::{anyhow, Result};
use marginfi::state::price::OracleSetup;
use solana_client::{
    nonblocking::rpc_client::RpcClient as NonBlockingRpcClient, rpc_client::RpcClient,
    rpc_config::RpcSendTransactionConfig,
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    genesis_config::ClusterType,
    instruction::InstructionError,
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    transaction::{TransactionError, VersionedTransaction},
};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use std::{collections::HashSet, str::FromStr};
use switchboard_on_demand_client::{
    FetchUpdateManyParams, Gateway, PullFeed, PullFeedAccountData, QueueAccountData, SbContext,
};
use tokio::runtime::{Builder, Runtime};

use solana_client::client_error::ClientError;
use solana_client::client_error::ClientErrorKind;
use switchboard_on_demand_client::CrossbarClient;

use rust_decimal::prelude::ToPrimitive;

const SWB_STALE_PRICE_ERROR_CODE: u32 = 6049;
const CHUNK_SIZE: usize = 20;

struct ResetFlag {
    flag: Arc<AtomicBool>,
}

impl Drop for ResetFlag {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::SeqCst);
    }
}

pub struct SwbCranker {
    cluster_type: ClusterType,
    tokio_rt: Runtime,
    rpc_client: RpcClient,
    non_blocking_rpc_client: NonBlockingRpcClient,
    crossbar_client: CrossbarClient,
    swb_gateway: Gateway,
    payer: Keypair,
}

impl SwbCranker {
    pub fn new(config: &GeneralConfig) -> Result<Self> {
        let payer = Keypair::from_bytes(&config.wallet_keypair)?;

        let tokio_rt = Builder::new_multi_thread()
            .thread_name("SwbCranker")
            .worker_threads(2)
            .enable_all()
            .build()?;

        let crossbar_client =
            CrossbarClient::new(&config.swb_crossbar_url, config.swb_crossbar_verbose);

        let rpc_client = RpcClient::new(config.rpc_url.clone());
        let non_blocking_rpc_client = NonBlockingRpcClient::new(config.rpc_url.clone());
        let queue = tokio_rt.block_on(QueueAccountData::load(
            &non_blocking_rpc_client,
            &config.swb_program_id,
        ))?;
        let swb_gateway =
            tokio_rt.block_on(queue.fetch_gateways(&non_blocking_rpc_client))?[0].clone();

        Ok(Self {
            cluster_type: config.cluster_type,
            tokio_rt,
            rpc_client,
            non_blocking_rpc_client,
            crossbar_client,
            swb_gateway,
            payer,
        })
    }

    pub fn simulate_stale_prices(&self, cache: &Cache) -> Result<()> {
        thread_info!("Simulating stale Swb prices...");

        let stale_oracles = self.find_stale_oracles(cache)?;

        let mut sim_prices: HashMap<Pubkey, f64> = HashMap::new();
        if !stale_oracles.is_empty() {
            for chunk in stale_oracles.chunks(CHUNK_SIZE) {
                let sim_responses = self.tokio_rt.block_on(
                    self.crossbar_client
                        .simulate_solana_feeds(self.cluster_type, chunk),
                )?;
                for sim_response in sim_responses {
                    if let Some(price) = sim_response.result {
                        sim_prices.insert(
                            Pubkey::from_str(&sim_response.feed)?,
                            price.to_f64().ok_or(anyhow!(
                                "Failed to convert simulated price {} to f64 for the Swb feed {}",
                                price,
                                sim_response.feed
                            ))?,
                        );
                    }
                }
            }
        }

        cache.oracles.try_update_simulated_prices(sim_prices)?;
        thread_info!("Simulation for the stale Swb prices completed.");

        Ok(())
    }

    fn calculate_sim_price(mut numbers: Vec<f64>) -> Option<f64> {
        if numbers.is_empty() {
            return None;
        }

        numbers.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mid = numbers.len() / 2;

        if numbers.len() % 2 == 0 {
            Some((numbers[mid - 1] + numbers[mid]) / 2.0)
        } else {
            Some(numbers[mid])
        }
    }

    fn find_stale_oracles(&self, cache: &Cache) -> Result<Vec<Pubkey>> {
        let clock = &clock_manager::get_clock(&cache.clock)?;

        let mut result: HashSet<Pubkey> = HashSet::new();
        for oracle_data in cache
            .banks
            .get_oracle_configs(Some(OracleSetup::SwitchboardPull))
            .iter()
        {
            let oracle_account = cache
                .oracles
                .try_get_account(&oracle_data.oracle_addresses[0])?;
            let mut offsets_data = [0u8; std::mem::size_of::<PullFeedAccountData>()];
            offsets_data.copy_from_slice(
                &oracle_account.data[8..std::mem::size_of::<PullFeedAccountData>() + 8],
            );
            let feed = load_swb_pull_account_from_bytes(&offsets_data)?;

            if clock
                .unix_timestamp
                .saturating_sub(feed.last_update_timestamp)
                > oracle_data.oracle_max_age as i64
            {
                thread_debug!(
                    "The Swb oracle {} is stale, last updated at {}",
                    oracle_data.oracle_addresses[0],
                    feed.last_update_timestamp
                );

                //                let feed_hash = hex::encode(feed.feed_hash);

                result.insert(oracle_data.oracle_addresses[0]);
            }
        }

        Ok(result.into_iter().collect())
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

        self.rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &txn,
                CommitmentConfig::confirmed(),
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    ..Default::default()
                },
            )?;

        Ok(())
    }
}

pub fn is_stale_swb_price_error(err: &ClientError) -> bool {
    matches!(
        err.kind(),
        ClientErrorKind::TransactionError(TransactionError::InstructionError(
            _,
            InstructionError::Custom(SWB_STALE_PRICE_ERROR_CODE)
        ))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_client::client_error::ClientError;
    use solana_client::client_error::ClientErrorKind;

    #[test]
    fn test_is_stale_swb_price_true_transaction_error() {
        let err = ClientError {
            request: None,
            kind: ClientErrorKind::TransactionError(TransactionError::InstructionError(
                0,
                InstructionError::Custom(6049), // The stale Swb price error code
            )),
        };
        assert!(is_stale_swb_price_error(&err));
    }

    #[test]
    fn test_is_stale_swb_price_false_wrong_custom_code() {
        let err = ClientError {
            request: None,
            kind: ClientErrorKind::TransactionError(TransactionError::InstructionError(
                0,
                InstructionError::Custom(1234),
            )),
        };
        assert!(!is_stale_swb_price_error(&err));
    }

    #[test]
    fn test_is_stale_swb_price_false_other_instruction_error() {
        let err = ClientError {
            request: None,
            kind: ClientErrorKind::TransactionError(TransactionError::InstructionError(
                0,
                InstructionError::InvalidArgument,
            )),
        };
        assert!(!is_stale_swb_price_error(&err));
    }

    #[test]
    fn test_is_stale_swb_price_false_other_transaction_error() {
        let err = ClientError {
            request: None,
            kind: ClientErrorKind::TransactionError(TransactionError::AccountNotFound),
        };
        assert!(!is_stale_swb_price_error(&err));
    }

    #[test]
    fn test_is_stale_swb_price_false_wrong_code() {
        let err = ClientError {
            request: None,
            kind: ClientErrorKind::Custom("Some other error".to_string()),
        };
        assert!(!is_stale_swb_price_error(&err));
    }

    #[test]
    fn test_is_stale_swb_price_false_other_kind() {
        let err = ClientError {
            request: None,
            kind: ClientErrorKind::Io(std::io::Error::new(std::io::ErrorKind::Other, "io error")),
        };
        assert!(!is_stale_swb_price_error(&err));
    }

    #[test]
    fn test_calculate_sim_price_empty() {
        let numbers: Vec<f64> = vec![];
        assert_eq!(SwbCranker::calculate_sim_price(numbers), None);
    }

    #[test]
    fn test_calculate_sim_price_single_element() {
        let numbers = vec![42.0];
        assert_eq!(SwbCranker::calculate_sim_price(numbers), Some(42.0));
    }

    #[test]
    fn test_calculate_sim_price_odd_number_of_elements() {
        let numbers = vec![3.0, 1.0, 2.0];
        assert_eq!(SwbCranker::calculate_sim_price(numbers), Some(2.0));
    }

    #[test]
    fn test_calculate_sim_price_even_number_of_elements() {
        let numbers = vec![4.0, 2.0, 1.0, 3.0];
        assert_eq!(SwbCranker::calculate_sim_price(numbers), Some(2.5));
    }

    #[test]
    fn test_calculate_sim_price_with_negative_numbers() {
        let numbers = vec![-1.0, -3.0, -2.0];
        assert_eq!(SwbCranker::calculate_sim_price(numbers), Some(-2.0));
    }

    #[test]
    fn test_calculate_sim_price_with_duplicates() {
        let numbers = vec![1.0, 2.0, 2.0, 3.0];
        assert_eq!(SwbCranker::calculate_sim_price(numbers), Some(2.0));
    }
}
