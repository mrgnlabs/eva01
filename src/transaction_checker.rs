use std::thread;

use crossbeam::channel::{Receiver, Sender};
use futures::executor::block_on;
use jito_sdk_rust::JitoJsonRpcSDK;
use solana_sdk::pubkey::Pubkey;

use log::{debug, error};

use crate::metrics::ERROR_COUNT;

#[derive(Debug)]
struct BundleStatus {
    confirmation_status: Option<String>,
    err: Option<serde_json::Value>,
    transactions: Option<Vec<String>>,
}

pub struct TransactionChecker {
    jito_sdk: JitoJsonRpcSDK,
}

impl TransactionChecker {
    pub fn new(jito_block_engine_url: &String) -> Self {
        debug!("Initializing JITO SDK with URL: {}", jito_block_engine_url);
        let jito_sdk = JitoJsonRpcSDK::new(&jito_block_engine_url, None);
        Self { jito_sdk }
    }

    pub fn check_bundle_status(&self, jito_rx: Receiver<(Pubkey, String)>, ack_tx: Sender<Pubkey>) {
        let max_retries = 10;
        let retry_delay = std::time::Duration::from_millis(500);
        while let Ok((ack_id, uuid)) = jito_rx.recv() {
            for attempt in 1..=max_retries {
                debug!(
                    "Checking bundle {} (ack_id: {}) status (attempt {}/{})",
                    uuid, ack_id, attempt, max_retries
                );

                let status_response = block_on(
                    self.jito_sdk
                        .get_in_flight_bundle_statuses(vec![uuid.to_string()]),
                );
                if let Err(e) = status_response {
                    debug!(
                        "Failed to check bundle {} (ack_id: {}) status: {:?}",
                        uuid, ack_id, e
                    );
                    continue;
                }

                let status_response = status_response.unwrap();
                match status_response.get("result") {
                    Some(result) => {
                        match result
                            .get("value")
                            .and_then(|value| value.as_array())
                            .and_then(|statuses| statuses.first())
                            .and_then(|bundle_status| bundle_status.get("status"))
                            .and_then(|status| status.as_str())
                        {
                            Some("Landed") => {
                                debug!(
                                    "({}) Bundle landed on-chain. Checking final status...",
                                    uuid
                                );
                                if let Err(e) = check_final_bundle_status(&self.jito_sdk, &uuid) {
                                    error!("({}) Final status: {}", uuid, e.to_string());
                                }
                                break;
                            }
                            Some("Pending") => {
                                debug!("({}) Bundle is pending. Waiting...", uuid);
                            }
                            Some(status) => {
                                debug!(
                                    "({}) Unexpected bundle status: {}. Waiting...",
                                    uuid, status
                                );
                            }
                            None => {
                                debug!("({}) Unable to parse bundle status. Waiting...", uuid);
                            }
                        }
                    }
                    None => match status_response.get("error") {
                        Some(error) => {
                            debug!("({}) Error checking bundle status: {:?}", uuid, error);
                        }
                        None => {
                            debug!("({}) Unexpected response format. Waiting...", uuid);
                        }
                    },
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
                        "Failed to confirm bundle status: uuid = {}, ack_id = {}",
                        uuid, ack_id
                    );
                    break;
                }
            }

            // TODO: replace with concurrent hashset.
            debug!("Sending ACK for bundle {} (ack_id: {})", uuid, ack_id);
            ack_tx.send(ack_id).unwrap();
        }
    }
}

fn check_final_bundle_status(jito_sdk: &JitoJsonRpcSDK, uuid: &str) -> anyhow::Result<()> {
    let max_retries = 10;
    let retry_delay = std::time::Duration::from_millis(500);

    for attempt in 1..=max_retries {
        debug!(
            "({}) Checking final bundle status (attempt {}/{})",
            uuid, attempt, max_retries
        );

        let status_response = block_on(jito_sdk.get_bundle_statuses(vec![uuid.to_string()]))?;
        let bundle_status = get_bundle_status(&status_response)?;

        match bundle_status.confirmation_status.as_deref() {
            Some("confirmed") => {
                debug!(
                    "({}) Bundle confirmed on-chain. Waiting for finalization...",
                    uuid
                );
                check_transaction_error(&bundle_status)?;
            }
            Some("finalized") => {
                debug!("({}) Bundle finalized on-chain successfully!", uuid);
                check_transaction_error(&bundle_status)?;
                print_transaction_url(&bundle_status);
                return Ok(());
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
        }

        if attempt < max_retries {
            thread::sleep(retry_delay);
        }
    }

    Err(anyhow::anyhow!(
        "({}) Failed to get finalized status after {} attempts",
        uuid,
        max_retries
    ))
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
