use std::{
    rc::Rc,
    sync::{Arc, RwLock},
};

use anchor_client::Program;
use anyhow::Result;
use dashmap::DashMap;
use fixed::types::I80F48;
use log::error;
use marginfi::{
    program::Marginfi,
    state::{
        marginfi_account::MarginfiAccount,
        marginfi_group::Bank,
        price::{OraclePriceFeedAdapter, PriceAdapter},
    },
    utils,
};
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_filter::{Memcmp, RpcFilterType},
};
use solana_program::{account_info::IntoAccountInfo, pubkey::Pubkey};
use solana_sdk::signature::Keypair;

use crate::utils::{accessor, batch_get_mutliple_accounts, BatchLoadingConfig};

const BANK_GROUP_PK_OFFSET: usize = 8 + 8 + 1;

pub struct MarginfiAccountWrapper {
    pub address: Pubkey,
    pub account: MarginfiAccount,
    pub banks: Vec<Arc<RwLock<BankWrapper>>>,
}

impl MarginfiAccountWrapper {
    pub fn new(
        address: Pubkey,
        account: MarginfiAccount,
        banks: Vec<Arc<RwLock<BankWrapper>>>,
    ) -> Self {
        Self {
            address,
            account,
            banks,
        }
    }
}

pub struct OracleWrapper {
    pub address: Pubkey,
    pub price_adapter: OraclePriceFeedAdapter,
}

impl OracleWrapper {
    pub fn new(address: Pubkey, price_adapter: OraclePriceFeedAdapter) -> Self {
        Self {
            address,
            price_adapter,
        }
    }
}

pub struct BankWrapper {
    pub address: Pubkey,
    pub bank: Bank,
    pub price_adapter: Arc<RwLock<OracleWrapper>>,
}

impl BankWrapper {
    pub fn new(address: Pubkey, bank: Bank, price_adapter: Arc<RwLock<OracleWrapper>>) -> Self {
        Self {
            address,
            bank,
            price_adapter,
        }
    }
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
    signer_pubkey: Pubkey,
}

pub struct StateEngineService {
    nb_rpc_client: Arc<solana_client::nonblocking::rpc_client::RpcClient>,
    rpc_client: Arc<solana_client::rpc_client::RpcClient>,
    anchor_client: anchor_client::Client<Rc<Keypair>>,
    marginfi_accounts: DashMap<Pubkey, Arc<RwLock<MarginfiAccountWrapper>>>,
    banks: DashMap<Pubkey, Arc<RwLock<BankWrapper>>>,
    token_accounts: DashMap<Pubkey, Arc<RwLock<TokenAccountWrapper>>>,
    oracles: DashMap<Pubkey, Arc<RwLock<OracleWrapper>>>,
    config: StateEngineConfig,
    accounts_to_track: Arc<RwLock<Vec<Pubkey>>>,
}

impl StateEngineService {
    pub async fn start(config: StateEngineConfig) -> anyhow::Result<Arc<Self>> {
        let anchor_client = anchor_client::Client::new(
            anchor_client::Cluster::Custom(config.rpc_url.clone(), "".to_string()),
            Rc::new(Keypair::new()),
        );

        let nb_rpc_client = Arc::new(solana_client::nonblocking::rpc_client::RpcClient::new(
            config.rpc_url.clone(),
        ));
        let rpc_client = Arc::new(solana_client::rpc_client::RpcClient::new(
            config.rpc_url.clone(),
        ));

        let state_engine_service = Arc::new(Self {
            marginfi_accounts: DashMap::new(),
            banks: DashMap::new(),
            token_accounts: DashMap::new(),
            anchor_client,
            config,
            nb_rpc_client,
            rpc_client,
            oracles: DashMap::new(),
            accounts_to_track: Arc::new(RwLock::new(vec![])),
        });

        state_engine_service.load_oracles_and_banks().await?;
        state_engine_service.load_token_accounts().await?;

        Ok(state_engine_service)
    }

    async fn start_geyser(self: &Arc<Self>) -> Result<()> {
        let aself = self.clone();

        Ok(())
    }

    /// Store pubkeys of oracles and token accounts to track
    fn cache_accounts_to_track(self: &Arc<Self>) -> Result<()> {
        let mut accounts = vec![];

        self.oracles.iter().for_each(|e| {
            accounts.push(*e.key());
        });

        self.token_accounts.iter().for_each(|e| {
            accounts.push(e.value().read().unwrap().address);
        });

        let aatt = self.accounts_to_track.clone();
        let mut att = aatt.write().unwrap();

        att.clear();
        att.append(&mut accounts);

        Ok(())
    }

    pub fn get_accounts_to_track(&self) -> Vec<Pubkey> {
        self.accounts_to_track.try_read().unwrap().clone()
    }

    async fn load_oracles_and_banks(self: &Arc<Self>) -> anyhow::Result<()> {
        let program: Program<Rc<Keypair>> = self.anchor_client.program(marginfi::id())?;
        let banks =
            program.accounts::<Bank>(vec![RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                BANK_GROUP_PK_OFFSET,
                self.config.marginfi_group_address.as_ref(),
            ))])?;

        let oracle_keys = banks
            .iter()
            .map(|(_, bank)| bank.config.oracle_keys[0])
            .collect::<Vec<_>>();

        let mut oracle_accounts = batch_get_mutliple_accounts(
            self.nb_rpc_client.clone(),
            &oracle_keys,
            BatchLoadingConfig::DEFAULT,
        )
        .await?;

        let mut oracles_with_addresses = oracle_keys
            .iter()
            .zip(oracle_accounts.iter_mut())
            .collect::<Vec<_>>();

        for ((bank_address, bank), (oracle_address, maybe_oracle_account)) in
            banks.iter().zip(oracles_with_addresses.iter_mut())
        {
            let oracle_ai =
                (*oracle_address, maybe_oracle_account.as_mut().unwrap()).into_account_info();
            let oracle_ai_c = oracle_ai.clone();

            // Insert an oracle or update the existing one
            self.oracles
                .entry(**oracle_address)
                .and_modify(move |oracle| {
                    if let Ok(mut oracle) = oracle.write() {
                        oracle.price_adapter = OraclePriceFeedAdapter::try_from_bank_config(
                            &bank.config,
                            &[oracle_ai],
                            i64::MAX,
                            u64::MAX,
                        )
                        .unwrap();
                    } else {
                        error!("Failed to acquire write lock on oracle");
                    }
                })
                .or_insert_with(|| {
                    let oracle_price_feed_adapter = OraclePriceFeedAdapter::try_from_bank_config(
                        &bank.config,
                        &[oracle_ai_c],
                        i64::MAX,
                        u64::MAX,
                    )
                    .unwrap();
                    Arc::new(RwLock::new(OracleWrapper::new(
                        **oracle_address,
                        oracle_price_feed_adapter,
                    )))
                });

            // Insert a bank or update the existing one
            self.banks
                .entry(*bank_address)
                .and_modify(|bank_entry| match bank_entry.try_write() {
                    Ok(mut bank_wg) => {
                        bank_wg.bank = bank.clone();
                    }
                    Err(e) => {
                        error!("Failed to acquire write lock on bank: {}", e);
                    }
                })
                .or_insert_with(|| {
                    Arc::new(RwLock::new(BankWrapper::new(
                        *bank_address,
                        bank.clone(),
                        self.oracles.get(oracle_address).unwrap().clone(),
                    )))
                });
        }

        Ok(())
    }

    async fn load_token_accounts(self: &Arc<Self>) -> anyhow::Result<()> {
        let banks = self.banks.clone();
        let bank_mints = banks
            .into_iter()
            .map(|(_, bank)| bank.read().unwrap().bank.mint)
            .collect::<Vec<_>>();

        let mut token_account_addresses = vec![];

        for mint in bank_mints.iter() {
            let ata = spl_associated_token_account::get_associated_token_address(
                &self.config.signer_pubkey,
                &mint,
            );
            token_account_addresses.push(ata);
        }

        let accounst = batch_get_mutliple_accounts(
            self.nb_rpc_client.clone(),
            &token_account_addresses,
            BatchLoadingConfig::DEFAULT,
        )
        .await?;

        let mut token_accounts_with_addresses_and_mints = token_account_addresses
            .iter()
            .zip(bank_mints.iter())
            .zip(accounst)
            .collect::<Vec<_>>();

        for ((token_account_address, mint), maybe_token_account) in
            token_accounts_with_addresses_and_mints.iter()
        {
            let balance = maybe_token_account
                .as_ref()
                .map(|a| accessor::amount(&a.data))
                .unwrap_or(0);

            let token_accounts = self.token_accounts.clone();

            token_accounts
                .entry(**mint)
                .and_modify(|token_account| {
                    if let Ok(mut token_account) = token_account.write() {
                        token_account.balance = balance;
                    } else {
                        error!("Failed to acquire write lock on token account");
                    }
                })
                .or_insert_with(|| {
                    Arc::new(RwLock::new(TokenAccountWrapper {
                        address: **token_account_address,
                        mint: **mint,
                        balance,
                        mint_decimals: 0,
                    }))
                });
        }

        Ok(())
    }

    pub fn get_group_id(&self) -> Pubkey {
        self.config.marginfi_group_address
    }

    pub fn get_marginfi_program_id(&self) -> Pubkey {
        self.config.marginfi_program_id
    }
}
