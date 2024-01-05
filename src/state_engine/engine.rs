use std::{rc::Rc, sync::Arc};

use dashmap::DashMap;
use fixed::types::I80F48;
use marginfi::state::{
    marginfi_account::MarginfiAccount,
    marginfi_group::Bank,
    price::{OraclePriceFeedAdapter, PriceAdapter},
};
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_filter::{Memcmp, RpcFilterType},
};
use solana_program::pubkey::Pubkey;
use solana_sdk::signature::Keypair;

use crate::utils::batch_get_mutliple_accounts;

const BANK_GROUP_PK_OFFSET: usize = 8 + 8 + 1;

pub struct MarginfiAccountWrapper {
    pub address: Pubkey,
    pub account: MarginfiAccount,
    pub health: Option<I80F48>,
}

pub struct BankWrapper {
    pub address: Pubkey,
    pub bank: Bank,
    pub price_adapter: OraclePriceFeedAdapter,
}

pub struct TokenAccountWrapper {
    pub address: Pubkey,
    pub mint: Pubkey,
    pub balance: u64,
    pub mint_decimals: u8,
}

pub struct StateEngineConfig {
    rpc_url: String,
    yellowstone_endpoint: String,
    yellowstone_x_token: Option<String>,
    marginfi_program_id: Pubkey,
    marginfi_group_address: Pubkey,
}

pub struct StateEngineService {
    rpc_client: Arc<RpcClient>,
    anchor_client: anchor_client::Client<Rc<Keypair>>,
    marginfi_accounts: DashMap<Pubkey, MarginfiAccountWrapper>,
    banks: DashMap<Pubkey, Bank>,

    token_accounts: DashMap<Pubkey, TokenAccountWrapper>,
    config: StateEngineConfig,
}

impl StateEngineService {
    pub fn start(config: StateEngineConfig) -> anyhow::Result<Arc<Self>> {
        let rpc_client =
            solana_client::nonblocking::rpc_client::RpcClient::new(config.rpc_url.clone());
        let anchor_client = anchor_client::Client::new(
            anchor_client::Cluster::Custom(config.rpc_url.clone(), "".to_string()),
            Rc::new(Keypair::new()),
        );

        let state_engine_service = Arc::new(Self {
            marginfi_accounts: DashMap::new(),
            banks: DashMap::new(),
            token_accounts: DashMap::new(),
            rpc_client,
            anchor_client,
            config,
        });

        Ok(state_engine_service)
    }

    async fn load_banks(self: Arc<Self>) -> anyhow::Result<()> {
        let program = self.anchor_client.program(marginfi::id())?;
        let banks =
            program.accounts::<Bank>(vec![RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                BANK_GROUP_PK_OFFSET,
                self.config.marginfi_group_address.as_ref(),
            ))])?;

        let oracle_keys = banks
            .iter()
            .map(|(_, bank)| bank.config.oracle_keys[0])
            .collect::<Vec<_>>();

        let oracle_accounts =
            batch_get_mutliple_accounts(self.rpc_client.clone(), oracle_keys, 64, 8).await?;

        for (address, bank) in banks {
            self.banks.insert(address, bank);
        }

        Ok(())
    }

    async fn load_oracles(self: Arc<Self>) -> anyhow::Result<()> {
        Ok(())
    }
}
