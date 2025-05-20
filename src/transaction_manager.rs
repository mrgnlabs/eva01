use crate::{config::GeneralConfig, metrics::ERROR_COUNT, thread_debug};
use crossbeam::channel::{Receiver, Sender};
use jito_sdk_rust::JitoJsonRpcSDK;
use log::{debug, error, info};
use serde_json::json;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    address_lookup_table::state::AddressLookupTable,
    address_lookup_table::AddressLookupTableAccount,
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
use tokio::runtime::{Builder, Runtime};

/// Manages transactions for the liquidator and rebalancer
#[allow(dead_code)]
pub struct TransactionManager {
    keypair: Keypair,
    jito_sdk: JitoJsonRpcSDK,
    jito_tip_account: Pubkey,
    rpc_client: RpcClient,
    lookup_tables: Vec<AddressLookupTableAccount>,
    tokio_rt: Runtime,
}

// Type alias for a batch of transactions
// A batch of transactions is a vector of vectors of instructions
// Each vector of instructions represents a single transaction
// The outer vector represents a batch of transactions
pub type BatchTransactions = Vec<RawTransaction>;

pub struct TransactionData {
    pub transactions: BatchTransactions,
    pub bundle_id: Pubkey,
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

impl TransactionManager {
    /// Creates a new transaction manager
    pub fn new(config: GeneralConfig) -> anyhow::Result<Self> {
        let keypair = read_keypair_file(&config.keypair_path)
            .map_err(|e| {
                error!(
                    "Failed to read keypair file ({:?}): {:?}",
                    &config.keypair_path, e
                );
            })
            .unwrap();

        debug!("Initializing RPC client with URL: {}", config.rpc_url);
        let rpc_client =
            RpcClient::new_with_commitment(config.rpc_url, CommitmentConfig::confirmed());

        // Loads the Address Lookup Table's accounts
        let mut lookup_tables = vec![];
        for table_address in &config.address_lookup_tables {
            let raw_account = rpc_client.get_account(table_address)?;
            let address_lookup_table = AddressLookupTable::deserialize(&raw_account.data)?;
            let lookup_table = AddressLookupTableAccount {
                key: *table_address,
                addresses: address_lookup_table.addresses.to_vec(),
            };
            lookup_tables.push(lookup_table);
        }

        let tokio_rt = Builder::new_multi_thread()
            .thread_name("transaction-manager")
            .worker_threads(2)
            .enable_all()
            .build()?;

        // Get JITO tip account
        let jito_block_engine_url = config.block_engine_url;
        debug!("Initializing JITO SDK with URL: {}", jito_block_engine_url);
        let jito_sdk: JitoJsonRpcSDK = JitoJsonRpcSDK::new(&jito_block_engine_url, None);
        let random_tip_account = tokio_rt.block_on(jito_sdk.get_random_tip_account())?;
        let jito_tip_account = Pubkey::from_str(&random_tip_account)?;

        Ok(Self {
            keypair,
            jito_sdk,
            rpc_client,
            jito_tip_account,
            lookup_tables,
            tokio_rt,
        })
    }

    /// Starts the transaction manager
    pub fn start(
        &mut self,
        jito_tx: Sender<(Pubkey, String)>,
        txn_rx: Receiver<TransactionData>,
    ) -> anyhow::Result<()> {
        info!("Starting the Transaction manager loop.");
        while let Ok(TransactionData {
            transactions,
            bundle_id,
        }) = txn_rx.recv()
        {
            thread_debug!(
                "Transaction manager received txn for Bundle ID: {:?}",
                bundle_id
            );

            let serialized_txs = match self.configure_instructions(transactions) {
                Ok(txs) => txs,
                Err(e) => {
                    ERROR_COUNT.inc();
                    error!("Failed to configure instructions: {:?}", e);
                    continue;
                }
            };

            let bundle = json!(serialized_txs);
            let response = match self
                .tokio_rt
                .block_on(self.jito_sdk.send_bundle(Some(bundle), None))
            {
                Ok(response) => response,
                Err(e) => {
                    ERROR_COUNT.inc();
                    error!("Failed to send JITO bundle: {:?}! {:?}", serialized_txs, e);
                    continue;
                }
            };

            // Extract bundle UUID from response
            let bundle_uuid = match response["result"].as_str() {
                Some(uuid) => uuid,
                None => {
                    ERROR_COUNT.inc();
                    error!(
                        "Failed to obtain bundle UUID from JITO response: {:?}",
                        response
                    );
                    continue;
                }
            };

            if let Err(error) = jito_tx.send((bundle_id, bundle_uuid.to_string())) {
                ERROR_COUNT.inc();
                error!(
                    "Failed to submit UUID JITO bundle id {:?} to JITO channel! {:?}",
                    bundle_id, error
                );
                break;
            }
        }

        info!("The Transaction manager loop is stopped.");
        Ok(())
    }

    /// Configures the instructions
    /// Adds the compute budget instruction to each instruction
    /// and compiles the instructions into transactions
    /// Returns a vector of transactions
    fn configure_instructions(
        &self,
        instructions: BatchTransactions,
    ) -> anyhow::Result<Vec<String>> {
        let blockhash = self.rpc_client.get_latest_blockhash()?;

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
