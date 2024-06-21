use std::{
    sync::atomic::{AtomicBool, Ordering},
    thread::sleep,
};

use crate::{config::GeneralConfig, sender::TransactionSender};
use crossbeam::channel::Receiver;
use futures::{
    stream::{iter, FuturesUnordered, StreamExt},
    FutureExt,
};
use jito_protos::{
    bundle,
    searcher::{
        searcher_service_client::SearcherServiceClient, GetTipAccountsRequest,
        NextScheduledLeaderRequest, SubscribeBundleResultsRequest,
    },
};
use jito_searcher_client::{get_searcher_client_no_auth, send_bundle_with_confirmation};
use log::{error, info};
use rayon::vec;
use solana_address_lookup_table_program::state::AddressLookupTable;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    address_lookup_table_account::AddressLookupTableAccount,
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
    transaction::VersionedTransaction,
};
use tonic::transport::Channel;

/// The leadership threshold related to the jito block engine
const LEADERSHIP_THRESHOLD: u64 = 2;

/// The sleep duration for the transaction manager
/// to wait before checking for the next leader
const SLEEP_DURATION: std::time::Duration = std::time::Duration::from_millis(500);

/// Manages transactions for the liquidator and rebalancer
pub struct TransactionManager {
    rx: Receiver<BatchTransactions>,
    keypair: Keypair,
    rpc: RpcClient,
    /// The searcher client for the jito block engine
    searcher_client: SearcherServiceClient<Channel>,
    /// Atomic boolean to check if the current node is the jito leader
    is_jito_leader: AtomicBool,
    /// The tip accounts of the jito block engine
    tip_accounts: Vec<String>,
    lookup_tables: Vec<AddressLookupTableAccount>,
}

/// Type alias for a batch of transactions
/// A batch of transactions is a vector of vectors of instructions
/// Each vector of instructions represents a single transaction
/// The outer vector represents a batch of transactions
pub type BatchTransactions = Vec<Vec<Instruction>>;

impl TransactionManager {
    /// Creates a new transaction manager
    pub async fn new(rx: Receiver<BatchTransactions>, config: GeneralConfig) -> Self {
        let keypair = read_keypair_file(&config.keypair_path).unwrap();
        let searcher_client = get_searcher_client_no_auth(&config.block_engine_url)
            .await
            .unwrap();

        let rpc =
            RpcClient::new_with_commitment(config.rpc_url.clone(), CommitmentConfig::confirmed());

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

        Self {
            rx,
            keypair,
            rpc,
            searcher_client,
            is_jito_leader: AtomicBool::new(false),
            tip_accounts: vec![],
            lookup_tables,
        }
    }

    /// Starts the transaction manager
    pub async fn start(&mut self) {
        // Load all tip accounts
        self.tip_accounts = self.get_tip_accounts().await.unwrap();

        //let handle = tokio::task::spawn(async move {
        //    if let Err(e) = self.listen_for_leader().await {
        //        error!("Failed to listen for the next leader: {:?}", e);
        //    }
        //});

        for mut instructions in self.rx.iter() {
            info!("Received instructions: {:?}", instructions);
            while !self.is_jito_leader.load(Ordering::Relaxed) {
                sleep(std::time::Duration::from_millis(500));
            }

            let mut transaction_futures = FuturesUnordered::new();

            for transaction in self.configure_instructions(instructions).await.unwrap() {
                transaction_futures.push(
                    self.send_transaction(transaction, self.searcher_client.clone())
                        .boxed(),
                );
            }

            while let Some(result) = transaction_futures.next().await {
                if let Err(e) = result {
                    error!("Failed to send bundle: {:?}", e);
                }
            }
        }
    }

    /// Sends a transaction/bundle of transactions to the jito
    /// block engine and waits for confirmation
    async fn send_transaction(
        &self,
        transaction: VersionedTransaction,
        mut searcher_client: SearcherServiceClient<Channel>,
    ) -> anyhow::Result<()> {
        let mut bundle_results_subscription = searcher_client
            .subscribe_bundle_results(SubscribeBundleResultsRequest {})
            .await?
            .into_inner();

        if let Err(e) = send_bundle_with_confirmation(
            &[transaction],
            &self.rpc,
            &mut searcher_client,
            &mut bundle_results_subscription,
        )
        .await
        {
            return Err(anyhow::anyhow!("Failed to send transaction"));
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
        for mut ixs in instructions {
            ixs.push(ComputeBudgetInstruction::set_compute_unit_limit(500_000));

            let transaction = VersionedTransaction::try_new(
                VersionedMessage::V0(v0::Message::try_compile(
                    &self.keypair.pubkey(),
                    &ixs,
                    &self.lookup_tables,
                    blockhash,
                )?),
                &[&self.keypair],
            )?;
            txs.push(transaction);
        }
        Ok(txs)
    }

    /// Listen for the next leader and update the AtomicBool accordingly
    async fn listen_for_leader(&mut self) -> anyhow::Result<()> {
        loop {
            let next_leader = self
                .searcher_client
                .get_next_scheduled_leader(NextScheduledLeaderRequest {})
                .await?
                .into_inner();

            let num_slots = next_leader.next_leader_slot - next_leader.current_slot;

            self.is_jito_leader
                .store(num_slots <= LEADERSHIP_THRESHOLD, Ordering::Relaxed);
        }
        Ok(())
    }

    async fn get_tip_accounts(&mut self) -> anyhow::Result<Vec<String>> {
        let tip_accounts = self
            .searcher_client
            .get_tip_accounts(GetTipAccountsRequest {})
            .await?
            .into_inner();

        Ok(tip_accounts.accounts)
    }
}
