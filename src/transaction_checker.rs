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
    pub fn new(
        jito_block_engine_url: &str,
        jito_block_engine_uuid: String,
    ) -> anyhow::Result<Self> {
        debug!("Initializing JITO SDK with URL: {}", jito_block_engine_url);
        //TODO: parameterize UUID
        let jito_sdk = JitoJsonRpcSDK::new(jito_block_engine_url, Some(jito_block_engine_uuid));

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

                if attempt <= max_retries {
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
    let statuses = status_response
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            anyhow::anyhow!(format!(
                "Missing or invalid 'result.value' in response: {}",
                status_response
            ))
        })?;

    let bundle_status = statuses.first().ok_or_else(|| {
        anyhow::anyhow!(format!(
            "Empty statuses array in response: {:?}",
            status_response
        ))
    })?;

    Ok(BundleStatus {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_valid_bundle_status_response() {
        let input = json!({
            "result": {
                "value": [{
                    "confirmation_status": "confirmed",
                    "err": null,
                    "transactions": ["tx1", "tx2"]
                }]
            }
        });

        let status = get_bundle_status(&input).expect("should parse correctly");
        assert_eq!(status.confirmation_status, Some("confirmed".to_string()));
        assert_eq!(status.err, Some(json!(null)));
        assert_eq!(
            status.transactions,
            Some(vec!["tx1".to_string(), "tx2".to_string()])
        );
    }

    #[test]
    fn test_missing_bundle_status_result() {
        let input = json!({});
        let err = get_bundle_status(&input).unwrap_err();
        assert!(
            err.to_string()
                .contains("Missing or invalid 'result.value'"),
            "Unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_bundle_status_value_not_array() {
        let input = json!({
            "result": {
                "value": "not-an-array"
            }
        });

        let err = get_bundle_status(&input).unwrap_err();
        assert!(
            err.to_string()
                .contains("Missing or invalid 'result.value'"),
            "Unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_empty_bundle_statuses_array() {
        let input = json!({
            "result": {
                "value": []
            }
        });

        let err = get_bundle_status(&input).unwrap_err();
        assert!(
            err.to_string().contains("Empty statuses array"),
            "Unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_missing_bundle_status_optional_fields() {
        let input = json!({
            "result": {
                "value": [{
                    // No confirmation_status, err, or transactions
                }]
            }
        });

        let status = get_bundle_status(&input).expect("should still parse");
        assert_eq!(status.confirmation_status, None);
        assert_eq!(status.err, None);
        assert_eq!(status.transactions, None);
    }

    #[test]
    fn test_bundle_status_transactions_with_invalid_types() {
        let input = json!({
            "result": {
                "value": [{
                    "confirmation_status": "processed",
                    "err": null,
                    "transactions": ["tx1", 123, true, null]
                }]
            }
        });

        let status = get_bundle_status(&input).expect("should parse with filtered transactions");
        assert_eq!(
            status.transactions,
            Some(vec!["tx1".to_string()]), // Only valid string is kept
            "Only valid string transactions should be included"
        );
    }

    #[test]
    fn test_valid_bundle_status() {
        let input = json!({
            "context": {
                "slot": 123456
            },
            "result": {
                "value": [{
                    "confirmation_status": "finalized",
                    "err": null,
                    "transactions": ["abc123", "def456"]
                }]
            }
        });

        let status = get_bundle_status(&input).expect("should parse correctly");
        assert_eq!(status.confirmation_status, Some("finalized".to_string()));
        assert_eq!(status.err, Some(json!(null)));
        assert_eq!(
            status.transactions,
            Some(vec!["abc123".to_string(), "def456".to_string()])
        );
    }
}
