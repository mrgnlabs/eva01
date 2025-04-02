use crate::{config::GeneralConfig, metrics::ERROR_COUNT, ward};
use crossbeam::channel::{Receiver, Sender};
use jito_sdk_rust::JitoJsonRpcSDK;
use log::{debug, error};
use serde_json::json;
use solana_address_lookup_table_program::state::AddressLookupTable;
use solana_client::{nonblocking::rpc_client::RpcClient, rpc_client::RpcClient as NonBlockRpc};
use solana_sdk::{
    address_lookup_table_account::AddressLookupTableAccount,
    bs58,
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
    system_instruction::transfer,
    transaction::VersionedTransaction,
};
use std::str::FromStr;
use std::sync::Arc;

/// Manages transactions for the liquidator and rebalancer
#[allow(dead_code)]
pub struct TransactionManager {
    rx: Receiver<TransactionData>,
    ack_tx: Sender<Pubkey>,
    keypair: Keypair,
    rpc: Arc<RpcClient>,
    non_block_rpc: NonBlockRpc,
    jito_sdk: Arc<JitoJsonRpcSDK>,
    jito_tip_account: Pubkey,
    lookup_tables: Vec<AddressLookupTableAccount>,
}

// Type alias for a batch of transactions
// A batch of transactions is a vector of vectors of instructions
// Each vector of instructions represents a single transaction
// The outer vector represents a batch of transactions
pub type BatchTransactions = Vec<RawTransaction>;

pub struct TransactionData {
    pub transactions: BatchTransactions,
    pub ack_id: Pubkey,
}

pub struct RawTransaction {
    pub instructions: Vec<Instruction>,
    pub lookup_tables: Option<Vec<AddressLookupTableAccount>>,
}

impl RawTransaction {
    pub fn new(instructions: Vec<Instruction>) -> Self {
        Self {
            instructions,
            lookup_tables: None,
        }
    }

    pub fn with_lookup_tables(mut self, lookup_tables: Vec<AddressLookupTableAccount>) -> Self {
        self.lookup_tables = Some(lookup_tables);
        self
    }
}

#[derive(Debug)]
struct BundleStatus {
    confirmation_status: Option<String>,
    err: Option<serde_json::Value>,
    transactions: Option<Vec<String>>,
}

impl TransactionManager {
    /// Creates a new transaction manager
    pub async fn new(
        rx: Receiver<TransactionData>,
        ack_tx: Sender<Pubkey>,
        config: GeneralConfig,
    ) -> anyhow::Result<Self> {
        let keypair = read_keypair_file(&config.keypair_path)
            .map_err(|e| {
                error!("Failed to read keypair file: {:?}", e);
                e
            })
            .unwrap();

        let rpc = Arc::new(RpcClient::new_with_commitment(
            config.rpc_url.clone(),
            CommitmentConfig::confirmed(),
        ));

        let non_block_rpc = NonBlockRpc::new(config.rpc_url.clone());

        // Loads the Address Lookup Table's accounts
        let mut lookup_tables = vec![];
        for table_address in &config.address_lookup_tables {
            let raw_account = rpc.get_account(table_address).await?;
            let address_lookup_table = AddressLookupTable::deserialize(&raw_account.data)?;
            let lookup_table = AddressLookupTableAccount {
                key: *table_address,
                addresses: address_lookup_table.addresses.to_vec(),
            };
            lookup_tables.push(lookup_table);
        }

        let jito_sdk = Arc::new(JitoJsonRpcSDK::new(&config.block_engine_url, None));
        let random_tip_account = jito_sdk.get_random_tip_account().await?;
        let jito_tip_account = Pubkey::from_str(&random_tip_account)?;

        Ok(Self {
            rx,
            ack_tx,
            keypair,
            rpc,
            non_block_rpc,
            jito_sdk,
            jito_tip_account,
            lookup_tables,
        })
    }

    /// Starts the transaction manager
    pub async fn start(&mut self) {
        let (jito_tx, jito_rx) = crossbeam::channel::unbounded::<(Pubkey, String)>();
        let jito_sdk = Arc::clone(&self.jito_sdk);
        let ack_tx = self.ack_tx.clone();
        tokio::spawn(async move {
            Self::check_bundle_status(jito_rx, jito_sdk, ack_tx).await;
        });

        while let Ok(TransactionData {
            transactions,
            ack_id,
        }) = self.rx.recv()
        {
            debug!("Ack ID: {:?}", ack_id);

            let serialized_txs = match self.configure_instructions(transactions).await {
                Ok(txs) => txs,
                Err(e) => {
                    ERROR_COUNT.inc();
                    error!("Failed to configure instructions: {:?}", e);
                    continue;
                }
            };

            let bundle = json!(serialized_txs);
            let response = match self.jito_sdk.send_bundle(Some(bundle), None).await {
                Ok(response) => response,
                Err(e) => {
                    ERROR_COUNT.inc();
                    error!("Failed to send JITO bundle: {:?}", e);
                    continue;
                }
            };

            // Extract bundle UUID from response
            let bundle_uuid = match response["result"].as_str() {
                Some(uuid) => uuid,
                None => {
                    ERROR_COUNT.inc();
                    error!("Failed to get bundle UUID from response: {:?}", response);
                    continue;
                }
            };

            ward!(jito_tx.send((ack_id, bundle_uuid.to_string())).ok(), break);
        }
        error!("Transaction manager stopped: internal stream closed");
    }

    async fn check_bundle_status(
        jito_rx: Receiver<(Pubkey, String)>,
        jito_sdk: Arc<JitoJsonRpcSDK>,
        ack_tx: Sender<Pubkey>,
    ) {
        let max_retries = 10;
        let retry_delay = std::time::Duration::from_millis(500);
        while let Ok((ack_id, uuid)) = jito_rx.recv() {
            for attempt in 1..=max_retries {
                debug!(
                    "Checking bundle {} (ack_id: {}) status (attempt {}/{})",
                    uuid, ack_id, attempt, max_retries
                );

                let status_response = jito_sdk
                    .get_in_flight_bundle_statuses(vec![uuid.to_string()])
                    .await;
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
                                if let Err(e) =
                                    Self::check_final_bundle_status(&jito_sdk, &uuid).await
                                {
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
                    tokio::time::sleep(retry_delay).await;
                } else {
                    ERROR_COUNT.inc();
                    error!(
                        "Failed to confirm bundle status: uuid = {}, ack_id = {}",
                        uuid, ack_id
                    );
                    break;
                }

                ack_tx.send(ack_id).unwrap();
            }
        }
    }

    async fn check_final_bundle_status(
        jito_sdk: &JitoJsonRpcSDK,
        uuid: &str,
    ) -> anyhow::Result<()> {
        let max_retries = 10;
        let retry_delay = std::time::Duration::from_millis(500);

        for attempt in 1..=max_retries {
            debug!(
                "({}) Checking final bundle status (attempt {}/{})",
                uuid, attempt, max_retries
            );

            let status_response = jito_sdk.get_bundle_statuses(vec![uuid.to_string()]).await?;
            let bundle_status = Self::get_bundle_status(&status_response)?;

            match bundle_status.confirmation_status.as_deref() {
                Some("confirmed") => {
                    debug!(
                        "({}) Bundle confirmed on-chain. Waiting for finalization...",
                        uuid
                    );
                    Self::check_transaction_error(&bundle_status)?;
                }
                Some("finalized") => {
                    debug!("({}) Bundle finalized on-chain successfully!", uuid);
                    Self::check_transaction_error(&bundle_status)?;
                    Self::print_transaction_url(&bundle_status);
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
                tokio::time::sleep(retry_delay).await;
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

    /*

               if let Some(BundleRejectionError::SimulationFailure(tx_str, msg)) =
               e.downcast_ref::<BundleRejectionError>()
           {
               if msg
                   .as_ref()
                   .is_some_and(|m| m.contains("custom program error: 0x1781"))
               {
                   error!("Illegal Liquidation");
               } else {
                   error!("SimulationFailure: {:?} - {:?}", tx_str, msg);
               }
           };
    */

    /// Configures the instructions
    /// Adds the compute budget instruction to each instruction
    /// and compiles the instructions into transactions
    /// Returns a vector of transactions
    async fn configure_instructions(
        &self,
        instructions: BatchTransactions,
    ) -> anyhow::Result<Vec<String>> {
        let blockhash = self.rpc.get_latest_blockhash().await?;

        let mut txs = Vec::new();
        for raw_transaction in instructions {
            let mut ixs = raw_transaction.instructions;
            ixs.push(ComputeBudgetInstruction::set_compute_unit_limit(1_000_000));
            ixs.push(transfer(
                &self.keypair.pubkey(),
                &self.jito_tip_account,
                10_000,
            ));
            let transaction = VersionedTransaction::try_new(
                VersionedMessage::V0(v0::Message::try_compile(
                    &self.keypair.pubkey(),
                    &ixs,
                    if raw_transaction.lookup_tables.is_some() {
                        raw_transaction.lookup_tables.as_ref().unwrap()
                    } else {
                        &self.lookup_tables
                    },
                    blockhash,
                )?),
                &[&self.keypair],
            )?;
            let transaction = bs58::encode(bincode::serialize(&transaction)?).into_string();
            txs.push(transaction);
        }
        Ok(txs)
    }
}
