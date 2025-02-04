use crate::config::GeneralConfig;
use crossbeam::channel::Receiver;
use jito_protos::searcher::{
    searcher_service_client::SearcherServiceClient, GetTipAccountsRequest,
    NextScheduledLeaderRequest, SubscribeBundleResultsRequest,
};
use jito_searcher_client::{get_searcher_client_no_auth, send_bundle_with_confirmation};
use log::{debug, error};
use solana_address_lookup_table_program::state::AddressLookupTable;
use solana_client::{nonblocking::rpc_client::RpcClient, rpc_client::RpcClient as NonBlockRpc};
use solana_sdk::{
    address_lookup_table_account::AddressLookupTableAccount,
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
use std::sync::{atomic::AtomicBool, Arc};
use tonic::transport::Channel;

/// The leadership threshold related to the jito block engine
const LEADERSHIP_THRESHOLD: u64 = 2;

/// The sleep duration for the transaction manager
/// to wait before checking for the next leader
const SLEEP_DURATION: std::time::Duration = std::time::Duration::from_millis(500);

/// Manages transactions for the liquidator and rebalancer
#[allow(dead_code)]
pub struct TransactionManager {
    rx: Receiver<BatchTransactions>,
    keypair: Keypair,
    rpc: Arc<RpcClient>,
    non_block_rpc: NonBlockRpc,
    /// The searcher client for the jito block engine
    searcher_client: SearcherServiceClient<Channel>,
    /// Atomic boolean to check if the current node is the jito leader
    is_jito_leader: AtomicBool,
    /// The tip accounts of the jito block engine
    tip_accounts: Vec<Pubkey>,
    lookup_tables: Vec<AddressLookupTableAccount>,
}

// Type alias for a batch of transactions
// A batch of transactions is a vector of vectors of instructions
// Each vector of instructions represents a single transaction
// The outer vector represents a batch of transactions
pub type BatchTransactions = Vec<RawTransaction>;

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
    pub async fn new(rx: Receiver<BatchTransactions>, config: GeneralConfig) -> Self {
        let keypair = read_keypair_file(&config.keypair_path)
            .map_err(|e| {
                error!("Failed to read keypair file: {:?}", e);
                e
            })
            .unwrap();
        let mut searcher_client = get_searcher_client_no_auth(&config.block_engine_url)
            .await
            .unwrap();

        let rpc = Arc::new(RpcClient::new_with_commitment(
            config.rpc_url.clone(),
            CommitmentConfig::confirmed(),
        ));

        let non_block_rpc = NonBlockRpc::new(config.rpc_url.clone());

        // Loads the Address Lookup Table's accounts
        let mut lookup_tables = vec![];
        for table_address in &config.address_lookup_tables {
            let raw_account = rpc.get_account(table_address).await.unwrap();
            let address_lookup_table = AddressLookupTable::deserialize(&raw_account.data).unwrap();
            let lookup_table = AddressLookupTableAccount {
                key: *table_address,
                addresses: address_lookup_table.addresses.to_vec(),
            };
            lookup_tables.push(lookup_table);
        }

        let tip_accounts = Self::get_tip_accounts(&mut searcher_client).await.unwrap();

        Self {
            rx,
            keypair,
            rpc,
            non_block_rpc,
            searcher_client,
            is_jito_leader: AtomicBool::new(false),
            tip_accounts,
            lookup_tables,
        }
    }

    /// Starts the transaction manager
    pub async fn start(&mut self) {
        for instructions in self.rx.clone().iter() {
            let transactions = match self.configure_instructions(instructions).await {
                Ok(txs) => txs,
                Err(e) => {
                    error!("Failed to configure instructions: {:?}", e);
                    continue;
                }
            };
            debug!("Waiting for Jito leader...");
            loop {
                let next_leader = match self
                    .searcher_client
                    .get_next_scheduled_leader(NextScheduledLeaderRequest {})
                    .await
                {
                    Ok(response) => response.into_inner(),
                    Err(e) => {
                        error!("Failed to get next scheduled leader: {:?}", e);
                        continue;
                    }
                };

                let num_slots = next_leader.next_leader_slot - next_leader.current_slot;

                if num_slots <= LEADERSHIP_THRESHOLD {
                    debug!("Sending bundle");
                    break;
                }

                tokio::time::sleep(SLEEP_DURATION).await;
            }
            let transaction = Self::send_transactions(
                transactions,
                self.searcher_client.clone(),
                self.rpc.clone(),
            );
            tokio::spawn(async move {
                if let Err(e) = transaction.await {
                    error!("Failed to send transaction: {:?}", e);
                }
            });
        }
    }

    /// Sends a transaction/bundle of transactions to the jito
    /// block engine and waits for confirmation
    async fn send_transactions(
        transactions: Vec<VersionedTransaction>,
        mut searcher_client: SearcherServiceClient<Channel>,
        rpc: Arc<RpcClient>,
    ) -> anyhow::Result<()> {
        let mut bundle_results_subscription = searcher_client
            .subscribe_bundle_results(SubscribeBundleResultsRequest {})
            .await?
            .into_inner();

        if let Err(e) = send_bundle_with_confirmation(
            &transactions,
            &rpc,
            &mut searcher_client,
            &mut bundle_results_subscription,
        )
        .await
        {
            return Err(anyhow::anyhow!("Failed to send transaction: {:?}", e));
        }

        Ok(())
    }

    /// Configures the instructions
    /// Adds the compute budget instruction to each instruction
    /// and compiles the instructions into transactions
    /// Returns a vector of transactions
    async fn configure_instructions(
        &self,
        instructions: BatchTransactions,
    ) -> anyhow::Result<Vec<VersionedTransaction>> {
        let blockhash = self.rpc.get_latest_blockhash().await?;

        let mut txs = Vec::new();
        for raw_transaction in instructions {
            let mut ixs = raw_transaction.instructions;
            ixs.push(ComputeBudgetInstruction::set_compute_unit_limit(1_000_000));
            ixs.push(transfer(
                &self.keypair.pubkey(),
                &self.tip_accounts[0],
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
            txs.push(transaction);
        }
        Ok(txs)
    }

    async fn get_tip_accounts(
        searcher_client: &mut SearcherServiceClient<Channel>,
    ) -> anyhow::Result<Vec<Pubkey>> {
        let tip_accounts = searcher_client
            .get_tip_accounts(GetTipAccountsRequest {})
            .await?
            .into_inner();

        let tip_accounts = tip_accounts
            .accounts
            .into_iter()
            .filter_map(|a| Pubkey::from_str(&a).ok())
            .collect::<Vec<Pubkey>>();

        Ok(tip_accounts)
    }
}
