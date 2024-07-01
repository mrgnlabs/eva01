use crate::config::GeneralConfig;
use crossbeam::channel::Receiver;
use jito_protos::searcher::{
    searcher_service_client::SearcherServiceClient, GetTipAccountsRequest,
    NextScheduledLeaderRequest, SubscribeBundleResultsRequest,
};
use jito_searcher_client::{get_searcher_client_no_auth, send_bundle_with_confirmation};
use log::{error, info};
use solana_address_lookup_table_program::state::AddressLookupTable;
use solana_client::{
    nonblocking::rpc_client::RpcClient, rpc_client::RpcClient as NonBlockRpc,
    rpc_client::SerializableTransaction, rpc_config::RpcSimulateTransactionConfig,
};
use solana_sdk::{
    address_lookup_table_account::AddressLookupTableAccount, commitment_config::CommitmentConfig, compute_budget::ComputeBudgetInstruction, instruction::Instruction, message::{v0, VersionedMessage}, pubkey::Pubkey, signature::{read_keypair_file, Keypair, Signature, Signer}, system_instruction::transfer, transaction::{Transaction, VersionedTransaction}
};
use std::{error::Error, str::FromStr};
use std::sync::atomic::{AtomicBool, Ordering};
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
    rpc: RpcClient,
    non_block_rpc: NonBlockRpc,
    /// The searcher client for the jito block engine
    searcher_client: SearcherServiceClient<Channel>,
    /// Atomic boolean to check if the current node is the jito leader
    is_jito_leader: AtomicBool,
    /// The tip accounts of the jito block engine
    tip_accounts: Vec<Pubkey>,
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
        let mut searcher_client = get_searcher_client_no_auth(&config.block_engine_url)
            .await
            .unwrap();

        let rpc =
            RpcClient::new_with_commitment(config.rpc_url.clone(), CommitmentConfig::confirmed());

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
        for instructions in self.rx.iter() {
            let transactions = self.configure_instructions(instructions).await.unwrap();
            for transaction in transactions {
                if let Err(e) = self.send_transaction(transaction, self.searcher_client.clone()).await {
                    error!("Failed to send transaction: {:?}", e);
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
        loop {
            let next_leader = searcher_client
                .get_next_scheduled_leader(NextScheduledLeaderRequest {})
                .await?
                .into_inner();

            let num_slots = next_leader.next_leader_slot - next_leader.current_slot;

            if num_slots <= LEADERSHIP_THRESHOLD {
                break;
            }

            tokio::time::sleep(SLEEP_DURATION).await;
        }

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
            return Err(anyhow::anyhow!("Failed to send transaction: {:?}", e));
        }

        Ok(())
    }

    /// Implements a alternative solution to jito transactions
    /// Sends a transaction to the network and waits for confirmation (non-jito)
    fn send_agressive_tx(&self, mut ixs: Vec<Instruction>) -> Result<Signature, Box<dyn Error>> {
        let recent_blockhash = self.non_block_rpc.get_latest_blockhash()?;

        ixs.push(ComputeBudgetInstruction::set_compute_unit_limit(500_000));

        let transaction = VersionedTransaction::try_new(
            VersionedMessage::V0(v0::Message::try_compile(
                &self.keypair.pubkey(),
                &ixs,
                &self.lookup_tables,
                recent_blockhash,
            )?),
            &[&self.keypair],
        )?;

        let signature = *transaction.get_signature();

        let simulation = self.non_block_rpc.simulate_transaction_with_config(
            &transaction,
            RpcSimulateTransactionConfig {
                commitment: Some(CommitmentConfig::processed()),
                ..Default::default()
            },
        )?;

        if simulation.value.err.is_some() {
            return Err(format!("Failed to simulate transaction {:?}", simulation.value).into());
        }

        (0..12).try_for_each(|_| {
            self.non_block_rpc.send_transaction(&transaction)?;
            Ok::<_, Box<dyn Error>>(())
        })?;

        let blockhash = transaction.get_recent_blockhash();

        self.non_block_rpc.confirm_transaction_with_spinner(
            &signature,
            blockhash,
            CommitmentConfig::confirmed(),
        )?;

        Ok(signature)
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
            ixs.push(transfer(
                &self.keypair.pubkey(),
                    &self.tip_accounts[0],
                10_000,
            ));
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
    }

    async fn get_tip_accounts(searcher_client: &mut SearcherServiceClient<Channel>) -> anyhow::Result<Vec<Pubkey>> {
        let tip_accounts = searcher_client
            .get_tip_accounts(GetTipAccountsRequest {})
            .await?
            .into_inner();
    
        let tip_accounts = tip_accounts.accounts.into_iter().filter_map(|a| Pubkey::from_str(&a).ok()).collect::<Vec<Pubkey>>();

        Ok(tip_accounts)
    }
}
