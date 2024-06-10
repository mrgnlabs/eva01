use jito_protos::{
    bundle::{bundle_result::Result as BundleResultType, rejected::Reason, Bundle},
    searcher::{
        searcher_service_client::SearcherServiceClient, GetTipAccountsRequest,
        NextScheduledLeaderRequest, SubscribeBundleResultsRequest,
    },
};
use jito_searcher_client::{get_searcher_client_no_auth, send_bundle_with_confirmation};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::Instruction,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction::transfer,
    transaction::{Transaction, VersionedTransaction},
};
use std::str::FromStr;
use tokio::time::sleep;
use tonic::transport::Channel;

pub struct JitoClient {
    rpc: RpcClient,
    searcher_client: SearcherServiceClient<Channel>,
    keypair: Keypair,
    tip_accounts: Vec<String>,
}

impl JitoClient {
    pub async fn new(rpc_url: String, keypair: Keypair, block_engine_url: String) -> Self {
        let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
        let searcher_client = get_searcher_client_no_auth(&block_engine_url)
            .await
            .expect("Failed to create a searcher client");

        Self {
            rpc,
            searcher_client,
            keypair,
            tip_accounts: Vec::new(),
        }
    }

    pub async fn send_transaction(&mut self, ix: Instruction, lamports: u64) -> anyhow::Result<()> {
        let mut bundle_results_subscription = self
            .searcher_client
            .subscribe_bundle_results(SubscribeBundleResultsRequest {})
            .await
            .expect("subscribe to bundle results")
            .into_inner();

        let blockhash = self.rpc.get_latest_blockhash().await?;

        let mut is_jito_leader = false;
        while !is_jito_leader {
            let next_leader = self
                .searcher_client
                .get_next_scheduled_leader(NextScheduledLeaderRequest {})
                .await
                .expect("Failed to get next scheduled leader")
                .into_inner();

            let num_slots = next_leader.next_leader_slot - next_leader.current_slot;
            is_jito_leader = (num_slots <= 2).into();
            sleep(std::time::Duration::from_millis(500)).await;
        }

        let txs = vec![VersionedTransaction::from(
            Transaction::new_signed_with_payer(
                &[
                    ix,
                    transfer(
                        &self.keypair.pubkey(),
                        &Pubkey::from_str(&self.tip_accounts[0])?,
                        lamports,
                    ),
                ],
                Some(&self.keypair.pubkey()),
                &[&self.keypair],
                blockhash,
            ),
        )];

        send_bundle_with_confirmation(
            &txs,
            &self.rpc,
            &mut self.searcher_client,
            &mut bundle_results_subscription,
        )
        .await
        .expect("Sending bundle failed!");

        Ok(())
    }

    pub async fn get_tip_accounts(&mut self) -> anyhow::Result<()> {
        let tip_accounts = self
            .searcher_client
            .get_tip_accounts(GetTipAccountsRequest {})
            .await
            .expect("Failed to get tip accounts")
            .into_inner();

        self.tip_accounts = tip_accounts.accounts;

        Ok(())
    }
}

