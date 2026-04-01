use anyhow::{anyhow, Result};
use base64::{prelude::BASE64_STANDARD, Engine};
use log::{debug, warn};
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
use std::str::FromStr;
use switchboard_on_demand_client::{
    CrossbarClient, FetchUpdateManyParams, Gateway, PullFeed, QueueAccountData, SbContext,
};
use tokio::runtime::{Builder, Runtime};

use crate::{config::Eva01Config, utils::simulation_cache::decode_and_apply_simulated_accounts};

//TODO: parametrize the Swb Program ID.
pub const SWB_PROGRAM_ID: &str = "A43DyUGA7s8eXPxqEjJY6EBu1KKbNgfxF8h17VAHn13w";

pub const SWB_STALE_PRICE_ERROR_CODE: &str = "17a1";
pub const SWB_STALE_PRICE_ERROR_CODE_NUMBER: u32 = 6049;
pub const SWB_STALE_HANDLED_ERROR: &str = "STALE HANDLED";

const CHUNK_SIZE: usize = 6;
const JITO_SIMULATE_BUNDLE_METHOD: &str = "simulateBundle";
const RAW_SIMULATE_BUNDLE_RESPONSE_LOG_LIMIT: usize = 8_000;

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
    post_execution_accounts: Option<Vec<Option<UiAccount>>>,
}

pub struct SwbCranker {
    tokio_rt: Runtime,
    rpc_client: RpcClient,
    non_blocking_rpc_client: NonBlockingRpcClient,
    swb_gateway: Gateway,
    crossbar: Option<CrossbarClient>,
    payer: Keypair,
    all_swb_oracles: Vec<Pubkey>,
}

impl SwbCranker {
    pub fn new(config: &Eva01Config, cache: &crate::cache::Cache) -> Result<Self> {
        let payer = Keypair::from_bytes(&config.wallet_keypair)?;
        let all_swb_oracles: Vec<_> = cache.banks.get_swb_oracles().into_iter().collect();

        let tokio_rt = Builder::new_multi_thread()
            .thread_name("SwbCranker")
            .worker_threads(2)
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
            &Pubkey::from_str(SWB_PROGRAM_ID).unwrap(),
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
        })
    }

    pub fn crank_oracles(&self, swb_oracles: Vec<Pubkey>) -> Result<()> {
        // Run simulations to get more details on potential failures, if crossbar is available.
        if let Some(crossbar) = self.crossbar.as_ref() {
            let result = self
                .tokio_rt
                .block_on(crossbar.simulate_solana_feeds(ClusterType::MainnetBeta, &swb_oracles));
            if let Err(result) = result {
                warn!("SWB Simulation failed: {:?}", result);
            }
        }

        for chunk in swb_oracles.chunks(CHUNK_SIZE) {
            self.crank_oracles_internal(chunk.to_vec())?;
        }
        Ok(())
    }

    pub fn simulate_oracles(&self, cache: &crate::cache::Cache) -> Result<()> {
        if self.all_swb_oracles.is_empty() {
            return Ok(());
        }

        let bundle_txs: Vec<SimulateBundleTx> = self
            .all_swb_oracles
            .chunks(CHUNK_SIZE)
            .map(|chunk| {
                let chunk_oracles = chunk.to_vec();
                let tx = self.build_crank_transaction(chunk_oracles.clone())?;
                let encoded_tx = BASE64_STANDARD.encode(bincode::serialize(&tx)?);
                Ok(SimulateBundleTx {
                    encoded_tx,
                    oracle_addresses: chunk_oracles,
                    transaction: tx,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let (simulation_result, raw_response) = match self.simulate_bundle(&bundle_txs) {
            Ok(result) => result,
            Err(err) if err.to_string().contains("incomplete postExecutionAccounts") => {
                warn!(
                    "simulateBundle omitted postExecutionAccounts; falling back to simulateTransaction for account capture"
                );
                let tx_accounts = self.simulate_transactions_for_accounts(&bundle_txs)?;

                for (bundle_tx, post_execution_accounts) in
                    bundle_txs.iter().zip(tx_accounts.iter())
                {
                    decode_and_apply_simulated_accounts(
                        &bundle_tx.oracle_addresses,
                        post_execution_accounts,
                        "simulateTransaction fallback",
                        |oracle_address, account| cache.oracles.try_update(oracle_address, account),
                    )?;
                }

                return Ok(());
            }
            Err(err) => return Err(err),
        };

        if let Some(summary) = simulation_result.summary.as_ref() {
            if !simulation_summary_succeeded(summary) {
                warn!("simulateBundle summary indicates failure: {}", summary);
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

        let fallback_accounts =
            if has_complete_post_execution_accounts(&simulation_result, &bundle_txs) {
                None
            } else {
                debug!(
                    "simulateBundle did not provide postExecutionAccounts; using simulateTransaction fallback for account capture"
                );
                Some(self.simulate_transactions_for_accounts(&bundle_txs)?)
            };

        for (index, (bundle_tx, tx_result)) in bundle_txs
            .iter()
            .zip(simulation_result.transaction_results.iter())
            .enumerate()
        {
            if tx_result.err.is_some() {
                warn!(
                    "simulateBundle returned transaction error for {} feeds: {:?}",
                    bundle_tx.oracle_addresses.len(),
                    tx_result.err
                );
            }

            let post_execution_accounts = if let Some(fallback_accounts) = fallback_accounts.as_ref()
            {
                &fallback_accounts[index]
            } else {
                tx_result
                    .post_execution_accounts
                    .as_ref()
                    .ok_or_else(|| anyhow!("simulateBundle did not return post execution accounts"))?
            };

            decode_and_apply_simulated_accounts(
                &bundle_tx.oracle_addresses,
                post_execution_accounts,
                "simulateBundle",
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
        let (crank_ix, crank_lut) = self.tokio_rt.block_on(PullFeed::fetch_update_consensus_ix(
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
        ))?;

        let blockhash = self
            .rpc_client
            .get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())?
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

    fn simulate_bundle(
        &self,
        bundle_txs: &[SimulateBundleTx],
    ) -> Result<(RpcSimulateBundleResult, Value)> {
        let encoded_txs: Vec<String> = bundle_txs.iter().map(|tx| tx.encoded_tx.clone()).collect();
        let pre_execution_accounts_configs: Vec<Value> =
            (0..bundle_txs.len()).map(|_| Value::Null).collect();
        let post_execution_accounts_configs: Vec<Value> = bundle_txs
            .iter()
            .map(|tx| {
                json!({
                    "encoding": "base64",
                    "addresses": tx
                        .oracle_addresses
                        .iter()
                        .map(|pk| pk.to_string())
                        .collect::<Vec<_>>()
                })
            })
            .collect();

        let request = RpcRequest::Custom {
            method: JITO_SIMULATE_BUNDLE_METHOD,
        };
        // This endpoint expects `encodedTransactions` and camelCase config keys.
        let params = json!([
            {
                "encodedTransactions": encoded_txs,
                "config": {
                    "preExecutionAccountsConfigs": pre_execution_accounts_configs,
                    "postExecutionAccountsConfigs": post_execution_accounts_configs,
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

    fn simulate_transactions_for_accounts(
        &self,
        bundle_txs: &[SimulateBundleTx],
    ) -> Result<Vec<Vec<Option<UiAccount>>>> {
        bundle_txs
            .iter()
            .map(|bundle_tx| {
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
                    .rpc_client
                    .simulate_transaction_with_config(&bundle_tx.transaction, config)?;

                if response.value.err.is_some() {
                    warn!(
                        "simulateTransaction fallback returned transaction error for {} feeds: {:?}",
                        bundle_tx.oracle_addresses.len(),
                        response.value.err
                    );
                }

                let accounts = response.value.accounts.ok_or_else(|| {
                    anyhow!(
                        "simulateTransaction fallback did not return accounts for {} feeds",
                        bundle_tx.oracle_addresses.len()
                    )
                })?;

                if accounts.len() != bundle_tx.oracle_addresses.len() {
                    return Err(anyhow!(
                        "simulateTransaction fallback returned {} accounts, expected {}",
                        accounts.len(),
                        bundle_tx.oracle_addresses.len()
                    ));
                }

                Ok(accounts)
            })
            .collect()
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

fn has_complete_post_execution_accounts(
    result: &RpcSimulateBundleResult,
    bundle_txs: &[SimulateBundleTx],
) -> bool {
    if result.transaction_results.len() != bundle_txs.len() {
        return false;
    }

    result
        .transaction_results
        .iter()
        .zip(bundle_txs.iter())
        .all(|(tx_result, bundle_tx)| {
            tx_result
                .post_execution_accounts
                .as_ref()
                .is_some_and(|accounts| accounts.len() == bundle_tx.oracle_addresses.len())
        })
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
}
