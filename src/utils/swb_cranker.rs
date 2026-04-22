use anyhow::{anyhow, Context, Result};
use base64::{prelude::BASE64_STANDARD, Engine};
use futures::stream::{self, StreamExt, TryStreamExt};
use log::warn;
use serde::Deserialize;
use serde_json::{json, Value};
use solana_account_decoder::{UiAccount, UiAccountEncoding};
use solana_client::{
    client_error::{ClientError, ClientErrorKind},
    nonblocking::rpc_client::RpcClient as NonBlockingRpcClient,
    rpc_client::RpcClient,
    rpc_config::{
        RpcSendTransactionConfig, RpcSimulateTransactionAccountsConfig,
        RpcSimulateTransactionConfig,
    },
    rpc_request::{RpcError, RpcRequest},
};
use solana_sdk::{
    commitment_config::{CommitmentConfig, CommitmentLevel},
    genesis_config::ClusterType,
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    transaction::VersionedTransaction,
};
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};
use switchboard_on_demand_client::{
    CrossbarClient, FetchUpdateManyParams, Gateway, PullFeed, QueueAccountData, SbContext,
};
use tokio::runtime::{Builder, Runtime};

use crate::{config::Eva01Config, utils::simulation_cache::decode_and_apply_simulated_accounts};

pub const SWB_STALE_PRICE_ERROR_CODE: &str = "17a1";
pub const SWB_STALE_PRICE_ERROR_CODE_NUMBER: u32 = 6049;
pub const SWB_STALE_HANDLED_ERROR: &str = "STALE HANDLED";

const CHUNK_SIZE: usize = 6;
const JITO_SIMULATE_BUNDLE_METHOD: &str = "simulateBundle";
const RAW_SIMULATE_BUNDLE_RESPONSE_LOG_LIMIT: usize = 8_000;
const SIMULATION_LOG_LINE_LIMIT: usize = 30;
const SIMULATION_LOG_CHAR_LIMIT: usize = 8_000;
const SIM_BUILD_TX_CONCURRENCY: usize = 4;
const SIM_TX_FALLBACK_CONCURRENCY: usize = 16;
const ORACLE_QUARANTINE_DURATION: Duration = Duration::from_secs(60 * 60);

struct SimulateBundleTx {
    encoded_tx: String,
    oracle_addresses: Vec<Pubkey>,
    transaction: VersionedTransaction,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RpcSimulateBundleResult {
    summary: Option<Value>,
    transaction_results: Vec<RpcSimulateBundleTransactionResult>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RpcSimulateBundleTransactionResult {
    err: Option<Value>,
    #[serde(default)]
    logs: Option<Vec<String>>,
}

pub struct SwbCranker {
    tokio_rt: Runtime,
    rpc_client: RpcClient,
    non_blocking_rpc_client: NonBlockingRpcClient,
    swb_gateway: Gateway,
    crossbar: Option<CrossbarClient>,
    payer: Keypair,
    all_swb_oracles: Vec<Pubkey>,
    oracle_quarantine: Mutex<HashMap<Pubkey, Instant>>,
}

impl SwbCranker {
    pub fn new(config: &Eva01Config, cache: &crate::cache::Cache) -> Result<Self> {
        let payer = Keypair::from_bytes(&config.wallet_keypair)?;
        let all_swb_oracles: Vec<_> = cache.banks.get_swb_oracles().into_iter().collect();

        let tokio_rt = Builder::new_multi_thread()
            .thread_name("SwbCranker")
            .worker_threads(4)
            .enable_all()
            .build()?;

        let rpc_client =
            RpcClient::new_with_commitment(config.rpc_url.clone(), CommitmentConfig::confirmed());
        let non_blocking_rpc_client = NonBlockingRpcClient::new_with_commitment(
            config.rpc_url.clone(),
            CommitmentConfig::confirmed(),
        );
        let queue = tokio_rt.block_on(QueueAccountData::load(
            &non_blocking_rpc_client,
            &config.swb_program_id,
        ))?;

        // Prefer private gateway from env; fall back to first on-chain gateway
        let (swb_gateway, crossbar) = if let Some(url) = config.crossbar_api_url.as_ref() {
            let crossbar = CrossbarClient::new(url.as_str(), true);
            (
                tokio_rt.block_on(queue.fetch_gateway_from_crossbar(&crossbar))?,
                Some(crossbar),
            )
        } else {
            (
                tokio_rt.block_on(queue.fetch_gateways(&non_blocking_rpc_client))?[0].clone(),
                None,
            )
        };

        Ok(Self {
            tokio_rt,
            rpc_client,
            non_blocking_rpc_client,
            swb_gateway,
            crossbar,
            payer,
            all_swb_oracles,
            oracle_quarantine: Mutex::new(HashMap::new()),
        })
    }

    pub fn crank_oracles(&self, swb_oracles: Vec<Pubkey>) -> Result<()> {
        let swb_oracles = self.filter_quarantined_oracles(&swb_oracles, "crank");
        if swb_oracles.is_empty() {
            return Ok(());
        }

        // Run simulations to get more details on potential failures, if crossbar is available.
        if let Some(crossbar) = self.crossbar.as_ref() {
            let result = self
                .tokio_rt
                .block_on(crossbar.simulate_solana_feeds(ClusterType::MainnetBeta, &swb_oracles));
            if let Err(result) = result {
                warn!("SWB Simulation failed: {:?}", result);
            }
        }

        for (chunk_index, chunk) in swb_oracles.chunks(CHUNK_SIZE).enumerate() {
            let chunk_oracles = chunk.to_vec();
            if let Err(err) = self.crank_oracles_internal(chunk_oracles.clone()) {
                warn!(
                    "SWB crank failed for chunk {} ({} feeds): {}. Retrying feeds individually.",
                    chunk_index,
                    chunk_oracles.len(),
                    err
                );

                let mut recovered_count = 0usize;
                let mut failed_individual: Vec<(Pubkey, anyhow::Error)> = Vec::new();
                for oracle in chunk_oracles {
                    match self.crank_oracles_internal(vec![oracle]) {
                        Ok(()) => recovered_count += 1,
                        Err(single_err) => failed_individual.push((oracle, single_err)),
                    }
                }

                if failed_individual.is_empty() {
                    continue;
                }

                if recovered_count > 0 {
                    let failed_oracles: Vec<Pubkey> = failed_individual
                        .iter()
                        .map(|(oracle, _)| *oracle)
                        .collect();
                    self.quarantine_oracles(
                        &failed_oracles,
                        "crank",
                        "individual crank failures after partial recovery",
                    );
                } else {
                    warn!(
                        "SWB crank chunk {} failed for all feeds even individually ({} feeds). Skipping this chunk without quarantine.",
                        chunk_index,
                        failed_individual.len()
                    );
                }
            }
        }
        Ok(())
    }

    pub fn simulate_oracles(&self, cache: &crate::cache::Cache) -> Result<()> {
        if self.all_swb_oracles.is_empty() {
            return Ok(());
        }

        let active_oracles = self.filter_quarantined_oracles(&self.all_swb_oracles, "simulation");
        if active_oracles.is_empty() {
            warn!("All SWB oracles are currently quarantined; skipping simulation.");
            return Ok(());
        }

        let chunked_oracles: Vec<Vec<Pubkey>> = active_oracles
            .chunks(CHUNK_SIZE)
            .map(|chunk| chunk.to_vec())
            .collect();

        let mut build_outcomes = self.tokio_rt.block_on(async {
            stream::iter(chunked_oracles.into_iter().enumerate())
                .map(|(chunk_index, chunk_oracles)| async move {
                    let tx_result = self
                        .build_crank_transaction_async(chunk_oracles.clone())
                        .await;
                    (chunk_index, chunk_oracles, tx_result)
                })
                .buffer_unordered(SIM_BUILD_TX_CONCURRENCY)
                .collect::<Vec<_>>()
                .await
        });
        build_outcomes.sort_by_key(|(chunk_index, _, _)| *chunk_index);

        let mut bundle_txs: Vec<SimulateBundleTx> = Vec::new();
        for (chunk_index, chunk_oracles, tx_result) in build_outcomes {
            match tx_result {
                Ok(tx) => {
                    let encoded_tx =
                        BASE64_STANDARD.encode(bincode::serialize(&tx).with_context(|| {
                            format!(
                                "failed to serialize SWB simulation transaction for chunk {}",
                                chunk_index
                            )
                        })?);
                    bundle_txs.push(SimulateBundleTx {
                        encoded_tx,
                        oracle_addresses: chunk_oracles,
                        transaction: tx,
                    });
                }
                Err(err) => {
                    let recovered_txs =
                        self.recover_failed_simulation_chunk(chunk_index, chunk_oracles, &err)?;
                    bundle_txs.extend(recovered_txs);
                }
            }
        }

        if bundle_txs.is_empty() {
            warn!("No buildable SWB simulation transactions for this round; skipping oracle simulation.");
            return Ok(());
        }

        let (simulation_result, raw_response) = self.simulate_bundle(&bundle_txs)?;

        if let Some(summary) = simulation_result.summary.as_ref() {
            if !simulation_summary_succeeded(summary) {
                let raw = truncate_for_log(
                    &raw_response.to_string(),
                    RAW_SIMULATE_BUNDLE_RESPONSE_LOG_LIMIT,
                );
                return Err(anyhow!(
                    "simulateBundle summary indicates failure: {} (raw response truncated: {})",
                    summary,
                    raw
                ));
            }
        }

        if simulation_result.transaction_results.len() != bundle_txs.len() {
            let summary = simulation_result
                .summary
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "null".to_string());
            let raw = truncate_for_log(
                &raw_response.to_string(),
                RAW_SIMULATE_BUNDLE_RESPONSE_LOG_LIMIT,
            );
            warn!(
                "simulateBundle unexpected transaction_results length. Raw response (truncated): {}",
                raw
            );
            return Err(anyhow!(
                "simulateBundle returned {} transaction results, expected {} (summary: {})",
                simulation_result.transaction_results.len(),
                bundle_txs.len(),
                summary
            ));
        }

        for (chunk_index, (bundle_tx, tx_result)) in bundle_txs
            .iter()
            .zip(simulation_result.transaction_results.iter())
            .enumerate()
        {
            if let Some(err) = tx_result.err.as_ref() {
                let logs = format_simulation_logs(
                    tx_result.logs.as_deref(),
                    SIMULATION_LOG_LINE_LIMIT,
                    SIMULATION_LOG_CHAR_LIMIT,
                );
                return Err(anyhow!(
                    "simulateBundle chunk {} failed for {} feeds {:?}: err={:?}; logs={}",
                    chunk_index,
                    bundle_tx.oracle_addresses.len(),
                    bundle_tx.oracle_addresses,
                    err,
                    logs
                ));
            }
        }

        // Keep bundle simulation as the authoritative bundle-level check and
        // capture post-execution accounts via simulateTransaction.
        let tx_accounts = self
            .tokio_rt
            .block_on(self.simulate_transactions_for_accounts(&bundle_txs))
            .context("failed to capture simulated accounts via simulateTransaction")?;
        let account_source = "simulateTransaction";

        for (bundle_tx, post_execution_accounts) in bundle_txs.iter().zip(tx_accounts.iter()) {
            decode_and_apply_simulated_accounts(
                &bundle_tx.oracle_addresses,
                post_execution_accounts,
                account_source,
                |oracle_address, account| cache.oracles.try_update(oracle_address, account),
            )?;
        }

        Ok(())
    }

    fn crank_oracles_internal(&self, swb_oracles: Vec<Pubkey>) -> Result<()> {
        let tx = self.build_crank_transaction(swb_oracles)?;

        self.rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &tx,
                CommitmentConfig::confirmed(),
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    preflight_commitment: Some(CommitmentLevel::Processed),
                    ..Default::default()
                },
            )?;

        Ok(())
    }

    fn build_crank_transaction(&self, swb_oracles: Vec<Pubkey>) -> Result<VersionedTransaction> {
        self.tokio_rt
            .block_on(self.build_crank_transaction_async(swb_oracles))
    }

    async fn build_crank_transaction_async(
        &self,
        swb_oracles: Vec<Pubkey>,
    ) -> Result<VersionedTransaction> {
        let (crank_ix, crank_lut) = PullFeed::fetch_update_consensus_ix(
            SbContext::new(),
            &self.non_blocking_rpc_client,
            FetchUpdateManyParams {
                feeds: swb_oracles,
                payer: self.payer.pubkey(),
                gateway: self.swb_gateway.clone(),
                crossbar: self.crossbar.clone(),
                num_signatures: Some(1),
                ..Default::default()
            },
        )
        .await?;

        let blockhash = self
            .non_blocking_rpc_client
            .get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())
            .await?
            .0;

        let tx = VersionedTransaction::try_new(
            VersionedMessage::V0(v0::Message::try_compile(
                &self.payer.pubkey(),
                &crank_ix,
                &crank_lut,
                blockhash,
            )?),
            &[&self.payer],
        )?;

        Ok(tx)
    }

    fn filter_quarantined_oracles(&self, oracles: &[Pubkey], context: &str) -> Vec<Pubkey> {
        let now = Instant::now();
        let mut quarantine_guard = match self.oracle_quarantine.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                warn!(
                    "SWB oracle quarantine lock poisoned while filtering for {}. Continuing with recovered state.",
                    context
                );
                poisoned.into_inner()
            }
        };

        quarantine_guard.retain(|_, until| *until > now);

        let mut active_oracles: Vec<Pubkey> = Vec::with_capacity(oracles.len());
        let mut skipped_count = 0usize;
        for oracle in oracles {
            if quarantine_guard.contains_key(oracle) {
                skipped_count += 1;
            } else {
                active_oracles.push(*oracle);
            }
        }

        if skipped_count > 0 {
            warn!(
                "Skipping {} quarantined SWB feeds for {} (cooldown {}s).",
                skipped_count,
                context,
                ORACLE_QUARANTINE_DURATION.as_secs()
            );
        }

        active_oracles
    }

    fn quarantine_oracles(&self, oracles: &[Pubkey], context: &str, reason: &str) {
        if oracles.is_empty() {
            return;
        }
        let until = Instant::now() + ORACLE_QUARANTINE_DURATION;

        let mut quarantine_guard = match self.oracle_quarantine.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                warn!(
                    "SWB oracle quarantine lock poisoned while quarantining for {}. Continuing with recovered state.",
                    context
                );
                poisoned.into_inner()
            }
        };

        for oracle in oracles {
            quarantine_guard.insert(*oracle, until);
        }

        warn!(
            "Quarantined {} SWB feeds for {} ({}s cooldown): {:?}",
            oracles.len(),
            context,
            ORACLE_QUARANTINE_DURATION.as_secs(),
            oracles
        );
        warn!("SWB quarantine reason for {}: {}", context, reason);
    }

    fn recover_failed_simulation_chunk(
        &self,
        chunk_index: usize,
        chunk_oracles: Vec<Pubkey>,
        batch_error: &anyhow::Error,
    ) -> Result<Vec<SimulateBundleTx>> {
        warn!(
            "SWB simulation build failed for chunk {} ({} feeds). Trying individual feeds. Batch error: {}",
            chunk_index,
            chunk_oracles.len(),
            batch_error
        );

        let mut recovered_txs: Vec<SimulateBundleTx> = Vec::new();
        let mut failed_oracles: Vec<(Pubkey, String)> = Vec::new();

        for oracle in chunk_oracles {
            match self.build_crank_transaction(vec![oracle]) {
                Ok(tx) => {
                    let encoded_tx =
                        BASE64_STANDARD.encode(bincode::serialize(&tx).with_context(|| {
                            format!("failed to serialize recovered tx for {}", oracle)
                        })?);
                    recovered_txs.push(SimulateBundleTx {
                        encoded_tx,
                        oracle_addresses: vec![oracle],
                        transaction: tx,
                    });
                }
                Err(err) => {
                    failed_oracles.push((oracle, err.to_string()));
                }
            }
        }

        if failed_oracles.is_empty() {
            return Ok(recovered_txs);
        }

        if recovered_txs.is_empty() {
            warn!(
                "SWB simulation chunk {} could not recover any feed individually ({} feeds). Skipping this chunk this round without quarantine.",
                chunk_index,
                failed_oracles.len()
            );
            return Ok(Vec::new());
        }

        let failed_feed_keys: Vec<Pubkey> =
            failed_oracles.iter().map(|(oracle, _)| *oracle).collect();
        let first_error = failed_oracles
            .first()
            .map(|(_, err)| err.as_str())
            .unwrap_or("unknown");
        self.quarantine_oracles(
            &failed_feed_keys,
            "simulation",
            &format!(
                "chunk {} partial recovery: {} failed feeds; first error: {}",
                chunk_index,
                failed_feed_keys.len(),
                first_error
            ),
        );

        Ok(recovered_txs)
    }

    fn simulate_bundle(
        &self,
        bundle_txs: &[SimulateBundleTx],
    ) -> Result<(RpcSimulateBundleResult, Value)> {
        let encoded_txs: Vec<String> = bundle_txs.iter().map(|tx| tx.encoded_tx.clone()).collect();

        let request = RpcRequest::Custom {
            method: JITO_SIMULATE_BUNDLE_METHOD,
        };
        // This endpoint expects `encodedTransactions` and camelCase config keys.
        let params = json!([
            {
                "encodedTransactions": encoded_txs,
                "config": {
                    "transactionEncoding": "base64",
                    "skipSigVerify": true,
                    "replaceRecentBlockhash": true
                }
            }
        ]);

        let raw_result = self
            .rpc_client
            .send::<Value>(request, params)
            .map_err(|err| anyhow!("simulateBundle RPC failed: {err}"))?;

        let parse_target = raw_result
            .get("value")
            .cloned()
            .unwrap_or_else(|| raw_result.clone());
        let parsed =
            serde_json::from_value::<RpcSimulateBundleResult>(parse_target).map_err(|err| {
                let raw = truncate_for_log(
                    &raw_result.to_string(),
                    RAW_SIMULATE_BUNDLE_RESPONSE_LOG_LIMIT,
                );
                warn!(
                    "simulateBundle response parse failed. Raw response (truncated): {}",
                    raw
                );
                anyhow!("simulateBundle response parse failed: {err}")
            })?;

        Ok((parsed, raw_result))
    }

    async fn simulate_transactions_for_accounts(
        &self,
        bundle_txs: &[SimulateBundleTx],
    ) -> Result<Vec<Vec<Option<UiAccount>>>> {
        let mut indexed = stream::iter(bundle_txs.iter().enumerate())
            .map(|(chunk_index, bundle_tx)| async move {
                let accounts_config = RpcSimulateTransactionAccountsConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    addresses: bundle_tx
                        .oracle_addresses
                        .iter()
                        .map(|pk| pk.to_string())
                        .collect(),
                };

                let config = RpcSimulateTransactionConfig {
                    sig_verify: false,
                    replace_recent_blockhash: true,
                    commitment: Some(CommitmentConfig::confirmed()),
                    accounts: Some(accounts_config),
                    ..Default::default()
                };

                let response = self
                    .non_blocking_rpc_client
                    .simulate_transaction_with_config(&bundle_tx.transaction, config)
                    .await
                    .with_context(|| {
                        format!(
                            "simulateTransaction RPC failed for chunk {} ({} feeds): {:?}",
                            chunk_index,
                            bundle_tx.oracle_addresses.len(),
                            bundle_tx.oracle_addresses
                        )
                    })?;

                let simulation_value = response.value;

                if let Some(err) = simulation_value.err.as_ref() {
                    let logs = format_simulation_logs(
                        simulation_value.logs.as_deref(),
                        SIMULATION_LOG_LINE_LIMIT,
                        SIMULATION_LOG_CHAR_LIMIT,
                    );
                    return Err(anyhow!(
                        "simulateTransaction chunk {} failed for {} feeds {:?}: err={:?}; logs={}",
                        chunk_index,
                        bundle_tx.oracle_addresses.len(),
                        bundle_tx.oracle_addresses,
                        err,
                        logs
                    ));
                }

                let accounts = simulation_value.accounts.ok_or_else(|| {
                    anyhow!(
                        "simulateTransaction chunk {} did not return accounts for {} feeds: {:?}",
                        chunk_index,
                        bundle_tx.oracle_addresses.len(),
                        bundle_tx.oracle_addresses
                    )
                })?;

                if accounts.len() != bundle_tx.oracle_addresses.len() {
                    return Err(anyhow!(
                        "simulateTransaction chunk {} returned {} accounts, expected {} for feeds: {:?}",
                        chunk_index,
                        accounts.len(),
                        bundle_tx.oracle_addresses.len(),
                        bundle_tx.oracle_addresses
                    ));
                }

                Ok((chunk_index, accounts))
            })
            .buffer_unordered(SIM_TX_FALLBACK_CONCURRENCY)
            .try_collect::<Vec<_>>()
            .await?;

        indexed.sort_by_key(|(chunk_index, _)| *chunk_index);
        Ok(indexed.into_iter().map(|(_, accounts)| accounts).collect())
    }
}

fn truncate_for_log(input: &str, max_len: usize) -> String {
    if input.len() <= max_len {
        return input.to_string();
    }

    let mut out = input.chars().take(max_len).collect::<String>();
    out.push_str("...<truncated>");
    out
}

fn format_simulation_logs(logs: Option<&[String]>, max_lines: usize, max_chars: usize) -> String {
    let Some(logs) = logs else {
        return "none".to_string();
    };
    if logs.is_empty() {
        return "none".to_string();
    }

    let mut lines: Vec<String> = logs.iter().take(max_lines).cloned().collect();
    if logs.len() > max_lines {
        lines.push(format!(
            "...<{} additional log lines truncated>",
            logs.len() - max_lines
        ));
    }

    truncate_for_log(&lines.join(" | "), max_chars)
}

fn simulation_summary_succeeded(summary: &Value) -> bool {
    match summary {
        Value::String(status) => status.eq_ignore_ascii_case("succeeded"),
        Value::Object(obj) => obj.contains_key("succeeded") || obj.contains_key("Succeeded"),
        _ => false,
    }
}

pub fn is_stale_swb_price_error(err: &ClientError) -> bool {
    if let ClientErrorKind::RpcError(RpcError::RpcResponseError { message, .. }) = err.kind() {
        message.contains(SWB_STALE_PRICE_ERROR_CODE)
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_client::client_error::ClientError;
    use solana_client::client_error::ClientErrorKind;
    use solana_client::rpc_request::RpcResponseErrorData;

    #[test]
    fn test_is_stale_swb_price_true_transaction_error() {
        let err = ClientError {
            request: None,
            kind: ClientErrorKind::RpcError(RpcError::RpcResponseError {
                code: -32000,
                message: SWB_STALE_PRICE_ERROR_CODE.to_string(),
                data: RpcResponseErrorData::Empty,
            }),
        };
        assert!(is_stale_swb_price_error(&err));
    }

    #[test]
    fn test_is_stale_swb_price_false_wrong_custom_code() {
        let err = ClientError {
            request: None,
            kind: ClientErrorKind::RpcError(RpcError::RpcResponseError {
                code: -32000,
                message: "12a4".to_string(),
                data: RpcResponseErrorData::Empty,
            }),
        };
        assert!(!is_stale_swb_price_error(&err));
    }

    #[test]
    fn test_is_stale_swb_price_false_other_instruction_error() {
        let err = ClientError {
            request: None,
            kind: ClientErrorKind::RpcError(RpcError::ParseError("Test error".to_string())),
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
    fn test_format_simulation_logs_with_none() {
        assert_eq!(
            format_simulation_logs(None, SIMULATION_LOG_LINE_LIMIT, SIMULATION_LOG_CHAR_LIMIT),
            "none"
        );
    }

    #[test]
    fn test_format_simulation_logs_line_limit() {
        let logs = vec![
            "line1".to_string(),
            "line2".to_string(),
            "line3".to_string(),
        ];
        let formatted = format_simulation_logs(Some(&logs), 2, 1024);
        assert!(formatted.contains("line1"));
        assert!(formatted.contains("line2"));
        assert!(formatted.contains("additional log lines truncated"));
    }
}
