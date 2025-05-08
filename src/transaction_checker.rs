use std::{
    collections::HashSet,
    sync::{Arc, RwLock},
    thread,
};

use crossbeam::channel::Receiver;
use jito_sdk_rust::JitoJsonRpcSDK;
use solana_sdk::pubkey::Pubkey;

use log::{debug, error, info};
use tokio::runtime::{Builder, Runtime};

use crate::metrics::ERROR_COUNT;

#[derive(Debug)]
struct BundleStatus {
    confirmation_status: Option<String>,
    err: Option<serde_json::Value>,
    transactions: Option<Vec<String>>,
}

pub struct TransactionChecker {
    jito_sdk: JitoJsonRpcSDK,
    tokio_rt: Runtime,
}

impl TransactionChecker {
    pub fn new(jito_block_engine_url: &str) -> anyhow::Result<Self> {
        debug!("Initializing JITO SDK with URL: {}", jito_block_engine_url);
        let jito_sdk = JitoJsonRpcSDK::new(jito_block_engine_url, None);

        let tokio_rt = Builder::new_multi_thread()
            .thread_name("transaction-checker")
            .worker_threads(2)
            .enable_all()
            .build()?;

        Ok(Self { jito_sdk, tokio_rt })
    }

    pub fn start(
        &self,
        jito_rx: Receiver<(Pubkey, String)>,
        pending_bundles: Arc<RwLock<HashSet<Pubkey>>>,
    ) -> anyhow::Result<()> {
        let max_retries = 20;
        let retry_delay = std::time::Duration::from_secs(2);

        info!("Starting the Transaction checker loop.");
        while let Ok((bundle_id, uuid)) = jito_rx.recv() {
            for attempt in 1..=max_retries {
                debug!(
                    "({}) Checking bundle status (attempt {}/{})",
                    uuid, attempt, max_retries
                );

                let status_response = self
                    .tokio_rt
                    .block_on(self.jito_sdk.get_bundle_statuses(vec![uuid.to_string()]))?;
                let bundle_status = get_bundle_status(&status_response);
                match bundle_status {
                    Ok(status) => match status.confirmation_status.as_deref() {
                        Some("confirmed") => {
                            debug!(
                                "({}) Bundle confirmed on-chain. Waiting for finalization...",
                                uuid
                            );
                            check_transaction_error(&status)?;
                        }
                        Some("finalized") => {
                            debug!("({}) Bundle finalized on-chain successfully!", uuid);
                            check_transaction_error(&status)?;
                            print_transaction_url(&status);
                            break;
                        }
                        Some(status) => {
                            debug!(
                                "({}) Unexpected final bundle status: {}. Continuing to poll...",
                                uuid, status
                            );
                        }
                        None => {
                            debug!(
                                "({}) Unable to parse final bundle status. Continuing to poll...",
                                uuid
                            );
                        }
                    },
                    Err(e) => {
                        debug!(
                            "Failed to get bundle status: uuid = {}, bundle_id = {}: {:?}",
                            uuid, bundle_id, e
                        );
                    }
                }

                if attempt < max_retries {
                    debug!(
                        "Sleeping for {} ms before retrying...",
                        retry_delay.as_millis()
                    );
                    thread::sleep(retry_delay);
                } else {
                    ERROR_COUNT.inc();
                    error!(
                        "Failed to confirm bundle status: uuid = {}, bundle_id = {}",
                        uuid, bundle_id
                    );
                    break;
                }
            }

            debug!(
                "Removing bundle {} (uuid: {}) from pending",
                bundle_id, uuid
            );
            pending_bundles.write().unwrap().remove(&bundle_id);
        }
        info!("The Transaction checker loop stopped.");
        Ok(())
    }
}

fn get_bundle_status(status_response: &serde_json::Value) -> anyhow::Result<BundleStatus> {
    status_response
        .get("result")
        .and_then(|result| result.get("value"))
        .and_then(|value| value.as_array())
        .and_then(|statuses| statuses.first())
        .ok_or_else(|| anyhow::anyhow!("Failed to parse bundle status"))
        .map(|bundle_status| BundleStatus {
            confirmation_status: bundle_status
                .get("confirmation_status")
                .and_then(|s| s.as_str())
                .map(String::from),
            err: bundle_status.get("err").cloned(),
            transactions: bundle_status
                .get("transactions")
                .and_then(|t| t.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                }),
        })
}

fn check_transaction_error(bundle_status: &BundleStatus) -> anyhow::Result<()> {
    if let Some(err) = &bundle_status.err {
        if err["Ok"].is_null() {
            println!("Transaction executed without errors.");
            Ok(())
        } else {
            println!("Transaction encountered an error: {:?}", err);
            Err(anyhow::anyhow!("Transaction encountered an error"))
        }
    } else {
        Ok(())
    }
}

fn print_transaction_url(bundle_status: &BundleStatus) {
    if let Some(transactions) = &bundle_status.transactions {
        if let Some(tx_id) = transactions.first() {
            debug!("Transaction URL: https://solscan.io/tx/{}", tx_id);
        } else {
            debug!("Unable to extract transaction ID.");
        }
    } else {
        debug!("No transactions found in the bundle status.");
    }
}
