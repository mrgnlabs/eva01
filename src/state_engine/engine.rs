use solana_account_decoder::UiAccountEncoding;
use solana_account_decoder::UiDataSliceConfig;
use solana_sdk::bs58;
use std::{
    rc::Rc,
    str::FromStr,
    sync::{Arc, RwLock},
};

use anchor_client::anchor_lang::AccountDeserialize;
use anchor_client::anchor_lang::Discriminator;
use anchor_client::Program;
use anyhow::Result;
use dashmap::{DashMap, DashSet};
use fixed::types::I80F48;
use log::{debug, error, warn};
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
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType},
};
use solana_program::{account_info::IntoAccountInfo, program_pack::Pack, pubkey::Pubkey};
use solana_sdk::{account::Account, signature::Keypair};

use crate::utils::{accessor, batch_get_multiple_accounts, BatchLoadingConfig};

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
    pub oracle_adapter: OracleWrapper,
}

impl BankWrapper {
    pub fn new(address: Pubkey, bank: Bank, oracle_adapter_wrapper: OracleWrapper) -> Self {
        Self {
            address,
            bank,
            oracle_adapter: oracle_adapter_wrapper,
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
    update_tasks: Arc<Mutex<HashMap<Pubkey, tokio::task::JoinHandle<anyhow::Result<()>>>>>,
}

pub struct StateEngineService {
    nb_rpc_client: Arc<solana_client::nonblocking::rpc_client::RpcClient>,
    rpc_client: Arc<solana_client::rpc_client::RpcClient>,
    anchor_client: anchor_client::Client<Arc<Keypair>>,
    marginfi_accounts: DashMap<Pubkey, Arc<RwLock<MarginfiAccountWrapper>>>,
    banks: DashMap<Pubkey, Arc<RwLock<BankWrapper>>>,
    token_accounts: DashMap<Pubkey, Arc<RwLock<TokenAccountWrapper>>>,
    config: StateEngineConfig,
    accounts_to_track: Arc<RwLock<Vec<Pubkey>>>,
    oracle_to_bank_map: DashMap<Pubkey, Vec<Arc<RwLock<BankWrapper>>>>,
    tracked_oracle_accounts: DashSet<Pubkey>,
    tracked_token_accounts: DashSet<Pubkey>,
}

impl StateEngineService {
    pub async fn start(config: Option<StateEngineConfig>) -> anyhow::Result<Arc<Self>> {
        let config = match config {
            Some(cfg) => cfg,
            None => StateEngineConfig {
                rpc_url: std::env::var("RPC_URL").expect("Expected RPC_URL to be set in env"),
                yellowstone_endpoint: std::env::var("YELLOWSTONE_ENDPOINT")
                    .expect("Expected YELLOWSTONE_ENDPOINT to be set in env"),
                yellowstone_x_token: std::env::var("YELLOWSTONE_X_TOKEN").ok(),
                marginfi_program_id: Pubkey::from_str(
                    &std::env::var("MARGINFI_PROGRAM_ID")
                        .expect("Expected MARGINFI_PROGRAM_ID to be set in env"),
                )
                .unwrap(),
                marginfi_group_address: Pubkey::from_str(
                    &std::env::var("MARGINFI_GROUP_ADDRESS")
                        .expect("Expected MARGINFI_GROUP_ADDRESS to be set in env"),
                )
                .unwrap(),
                signer_pubkey: Pubkey::from_str(
                    &std::env::var("SIGNER_PUBKEY")
                        .expect("Expected SIGNER_PUBKEY to be set in env"),
                )
                .unwrap(),
            },
        };

        let anchor_client = anchor_client::Client::new(
            anchor_client::Cluster::Custom(config.rpc_url.clone(), "".to_string()),
            Arc::new(Keypair::new()),
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
            accounts_to_track: Arc::new(RwLock::new(Vec::new())),
            oracle_to_bank_map: DashMap::new(),
            tracked_oracle_accounts: DashSet::new(),
            tracked_token_accounts: DashSet::new(),
        });

        state_engine_service.load_oracles_and_banks().await?;
        state_engine_service.load_token_accounts().await?;

        Ok(state_engine_service)
    }

    async fn start_geyser(self: &Arc<Self>) -> Result<()> {
        let aself = self.clone();

        Ok(())
    }

    pub fn get_accounts_to_track(&self) -> Vec<Pubkey> {
        self.tracked_oracle_accounts
            .iter()
            .chain(self.tracked_token_accounts.iter())
            .map(|e| *e)
            .collect::<Vec<_>>()
    }

    async fn load_oracles_and_banks(self: &Arc<Self>) -> anyhow::Result<()> {
        let program: Program<Arc<Keypair>> = self.anchor_client.program(marginfi::id())?;
        let banks =
            program.accounts::<Bank>(vec![RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                BANK_GROUP_PK_OFFSET,
                self.config.marginfi_group_address.as_ref(),
            ))])?;

        let oracle_keys = banks
            .iter()
            .map(|(_, bank)| bank.config.oracle_keys[0])
            .collect::<Vec<_>>();

        let mut oracle_accounts = batch_get_multiple_accounts(
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

            let bank_ref = self
                .banks
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
                        OracleWrapper::new(
                            **oracle_address,
                            OraclePriceFeedAdapter::try_from_bank_config(
                                &bank.config,
                                &[oracle_ai_c],
                                i64::MAX,
                                u64::MAX,
                            )
                            .unwrap(),
                        ),
                    )))
                });

            self.oracle_to_bank_map
                .entry(**oracle_address)
                .and_modify(|vec| vec.push(bank_ref.clone()))
                .or_insert_with(|| vec![bank_ref.clone()]);

            self.tracked_oracle_accounts.insert(**oracle_address);
        }

        Ok(())
    }

    pub fn update_oracle(
        &self,
        oracle_address: &Pubkey,
        mut oracle_account: Account,
    ) -> anyhow::Result<()> {
        if let Some(banks_to_update) = self.oracle_to_bank_map.get(oracle_address) {
            let oracle_ai = (oracle_address, &mut oracle_account).into_account_info();
            for bank_to_update in banks_to_update.iter() {
                if let Ok(mut bank_to_update) = bank_to_update.try_write() {
                    bank_to_update.oracle_adapter.price_adapter =
                        OraclePriceFeedAdapter::try_from_bank_config(
                            &bank_to_update.bank.config,
                            &[oracle_ai.clone()],
                            i64::MAX,
                            u64::MAX,
                        )?;
                } else {
                    warn!("Failed to acquire write lock on bank, oracle update skipped");
                }
            }
        } else {
            warn!("Received update for unknown oracle {}", oracle_address);
        }

        Ok(())
    }

    pub fn update_bank(&self, bank_address: &Pubkey, bank: Account) -> anyhow::Result<bool> {
        let bank = bytemuck::from_bytes::<Bank>(&bank.data.as_slice()[8..]);

        let new_bank = self.banks.contains_key(bank_address);

        self.banks
            .entry(*bank_address)
            .and_modify(|bank_entry| {
                if let Ok(mut bank_entry) = bank_entry.try_write() {
                    bank_entry.bank = bank.clone();
                } else {
                    warn!("Failed to acquire write lock on bank, bank update skipped");
                }
            })
            .or_insert_with(|| {
                debug!("Received update for a new bank {}", bank_address);

                let oracle_address = bank.config.oracle_keys[0];
                let mut oracle_account = self.rpc_client.get_account(&oracle_address).unwrap();
                let oracle_account_ai = (&oracle_address, &mut oracle_account).into_account_info();

                self.tracked_oracle_accounts.insert(oracle_address);

                Arc::new(RwLock::new(BankWrapper::new(
                    *bank_address,
                    bank.clone(),
                    OracleWrapper::new(
                        oracle_address,
                        OraclePriceFeedAdapter::try_from_bank_config(
                            &bank.config,
                            &[oracle_account_ai],
                            i64::MAX,
                            u64::MAX,
                        )
                        .unwrap(),
                    ),
                )))
            });

        Ok(new_bank)
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

        let accounts = batch_get_multiple_accounts(
            self.nb_rpc_client.clone(),
            &token_account_addresses,
            BatchLoadingConfig::DEFAULT,
        )
        .await?;

        let token_accounts_with_addresses_and_mints = token_account_addresses
            .iter()
            .zip(bank_mints.iter())
            .zip(accounts)
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

            self.tracked_token_accounts.insert(**token_account_address);
        }

        Ok(())
    }

    pub fn update_token_account(
        &self,
        token_account_address: &Pubkey,
        token_account: Account,
    ) -> anyhow::Result<()> {
        let token_accounts = self.token_accounts.clone();
        let mint = accessor::mint(&token_account.data);
        let balance = accessor::amount(&token_account.data);

        token_accounts
            .entry(mint)
            .and_modify(|token_account_ref| {
                if let Ok(mut token_account_ref) = token_account_ref.write() {
                    token_account_ref.balance = balance;
                } else {
                    error!("Failed to acquire write lock on token account");
                }
            })
            .or_insert_with(|| {
                let mint_account = self.rpc_client.get_account(&mint).unwrap();
                let decimals = spl_token::state::Mint::unpack(&mint_account.data)
                    .map_err(|e| anyhow::anyhow!("Failed to unpack mint: {:?}", e))
                    .unwrap()
                    .decimals;

                Arc::new(RwLock::new(TokenAccountWrapper {
                    address: *token_account_address,
                    mint,
                    balance,
                    mint_decimals: decimals,
                }))
            });

        Ok(())
    }

    pub fn get_group_id(&self) -> Pubkey {
        self.config.marginfi_group_address
    }

    pub fn get_marginfi_program_id(&self) -> Pubkey {
        self.config.marginfi_program_id
    }

    pub fn is_tracked_oracle(&self, address: &Pubkey) -> bool {
        self.tracked_oracle_accounts.contains(address)
    }

    pub fn is_tracked_token_account(&self, address: &Pubkey) -> bool {
        self.tracked_token_accounts.contains(address)
    }

    async fn load_marginfi_accounts(self: &Arc<Self>) -> anyhow::Result<()> {
        let program: Program<Arc<Keypair>> = self.anchor_client.program(marginfi::id())?;
        let marginfi_account_addresses = self
            .nb_rpc_client
            .get_program_accounts_with_config(
                &self.config.marginfi_program_id,
                RpcProgramAccountsConfig {
                    account_config: RpcAccountInfoConfig {
                        encoding: Some(UiAccountEncoding::Base64),
                        data_slice: Some(UiDataSliceConfig {
                            offset: 0,
                            length: 0,
                        }),
                        ..Default::default()
                    },
                    filters: Some(vec![
                        RpcFilterType::Memcmp(Memcmp {
                            offset: 8,
                            bytes: MemcmpEncodedBytes::Binary(
                                self.config.marginfi_group_address.to_string(),
                            ),
                            encoding: None,
                        }),
                        RpcFilterType::Memcmp(Memcmp {
                            offset: 0,
                            bytes: MemcmpEncodedBytes::Binary(
                                bs58::encode(MarginfiAccount::DISCRIMINATOR).into_string(),
                            ),
                            encoding: None,
                        }),
                    ]),
                    with_context: Some(false),
                },
            )
            .await?;

        let marginfi_account_pubkeys: Vec<Pubkey> = marginfi_account_addresses
            .iter()
            .map(|(pubkey, _)| *pubkey)
            .collect();

        let mut marginfi_accounts = batch_get_multiple_accounts(
            self.nb_rpc_client.clone(),
            &marginfi_account_pubkeys,
            BatchLoadingConfig::DEFAULT,
        )
        .await?;

        for (address, account) in marginfi_account_addresses
            .iter()
            .zip(marginfi_accounts.iter_mut())
        {
            let account = account.as_mut().unwrap();
            let mut data_slice = account.data.as_slice();
            let marginfi_account = MarginfiAccount::try_deserialize(&mut data_slice).unwrap();
            self.update_marginfi_account(&address.0, marginfi_account)?;
        }

        Ok(())
    }

    pub fn update_marginfi_account(
        &self,
        marginfi_account_address: &Pubkey,
        marginfi_account: MarginfiAccount,
    ) -> anyhow::Result<()> {
        let marginfi_accounts = self.marginfi_accounts.clone();

        marginfi_accounts
            .entry(*marginfi_account_address)
            .and_modify(|marginfi_account_ref| {
                if let Ok(mut marginfi_account_ref) = marginfi_account_ref.write() {
                    marginfi_account_ref.account = marginfi_account.clone();
                } else {
                    error!("Failed to acquire write lock on marginfi account");
                }
            })
            .or_insert_with(|| {
                Arc::new(RwLock::new(MarginfiAccountWrapper::new(
                    *marginfi_account_address,
                    marginfi_account.clone(),
                    Vec::new(),
                )))
            });

        Ok(())
    }

    async fn update_all_accounts_helper<T, F, G>(
        &self,
        accounts: DashMap<Pubkey, Arc<RwLock<T>>>,
        get_account: F,
        update_account: G,
    ) -> anyhow::Result<()>
    where
        F: Fn(&T) -> Account + Send + Sync,
        G: Fn(Pubkey, Account) -> tokio::task::JoinHandle<anyhow::Result<()>> + Send + Sync,
    {
        loop {
            let cloned_accounts = accounts.clone();
            for (address, account) in cloned_accounts.iter() {
                let address = *address;
                let account = get_account(&account.read().unwrap());
                let update_future = update_account(address, account);

                let mut update_tasks = self.update_tasks.lock().await;
                if let Some(task) = update_tasks.get(&address) {
                    let _ = task.await;
                }
                update_tasks.insert(address, tokio::spawn(update_future));
            }
        }
    }

    async fn update_all_marginfi_accounts(&self) -> anyhow::Result<()> {
        self.update_all_accounts(
            self.marginfi_accounts.clone(),
            |account_wrapper| account_wrapper.account.clone(),
            |address, account| {
                let self_clone = self.clone();
                tokio::spawn(
                    async move { self_clone.update_marginfi_account(&address, account).await },
                )
            },
        )
        .await
    }

    async fn update_all_bank_accounts(&self) -> anyhow::Result<()> {
        self.update_all_accounts(
            self.bank_accounts.clone(),
            |account_wrapper| account_wrapper.account.clone(),
            |address, account| {
                let self_clone = self.clone();
                tokio::spawn(async move { self_clone.update_bank(&address, account).await })
            },
        )
        .await
    }

    async fn update_all_token_accounts(&self) -> anyhow::Result<()> {
        self.update_all_accounts(
            self.token_accounts.clone(),
            |account_wrapper| account_wrapper.account.clone(),
            |address, account| {
                let self_clone = self.clone();
                tokio::spawn(
                    async move { self_clone.update_token_account(&address, account).await },
                )
            },
        )
        .await
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        loop {
            self.update_all_bank_accounts().await?;
            self.update_all_token_accounts().await?;
            self.update_all_marginfi_accounts().await?;
        }
    }
}
