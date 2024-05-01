use crossbeam::channel::Receiver;
use crossbeam::channel::Sender;
use fixed::types::I80F48;
use log::info;
use log::trace;
use marginfi::constants::EXP_10_I80F48;
use marginfi::state::marginfi_account::calc_amount;
use marginfi::state::marginfi_account::calc_value;
use marginfi::state::marginfi_account::BalanceSide;
use marginfi::state::marginfi_account::RequirementType;
use marginfi::state::price::OraclePriceType;
use marginfi::state::price::PriceAdapter;
use marginfi::state::price::PriceBias;
use solana_account_decoder::UiAccountEncoding;
use solana_account_decoder::UiDataSliceConfig;
use solana_sdk::bs58;
use solana_sdk::pubkey;
use std::sync::Arc;
use std::sync::RwLock;

use anchor_client::anchor_lang::Discriminator;
use anchor_client::Program;
use dashmap::{DashMap, DashSet};
use log::{debug, error, warn};
use marginfi::state::{
    marginfi_account::MarginfiAccount, marginfi_group::Bank, price::OraclePriceFeedAdapter,
};
use solana_client::{
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType},
};
use solana_program::{account_info::IntoAccountInfo, program_pack::Pack, pubkey::Pubkey};
use solana_sdk::{account::Account, signature::Keypair};

use crate::state_engine::geyser::GeyserService;
use crate::token_account_manager::TokenAccountManager;
use crate::utils::{accessor, batch_get_multiple_accounts, from_pubkey_string, BatchLoadingConfig};

use super::geyser::GeyserServiceConfig;
use super::marginfi_account::MarginfiAccountWrapper;

const BANK_GROUP_PK_OFFSET: usize = 32 + 1 + 8;

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

    fn get_pricing_params(
        &self,
        side: BalanceSide,
        requirement_type: RequirementType,
    ) -> (I80F48, Option<PriceBias>, OraclePriceType) {
        match (side, requirement_type) {
            (BalanceSide::Assets, RequirementType::Initial) => (
                self.bank.config.asset_weight_init.into(),
                Some(PriceBias::Low),
                OraclePriceType::TimeWeighted,
            ),
            (BalanceSide::Assets, RequirementType::Maintenance) => (
                self.bank.config.asset_weight_maint.into(),
                Some(PriceBias::Low),
                OraclePriceType::RealTime,
            ),
            (BalanceSide::Liabilities, RequirementType::Initial) => (
                self.bank.config.liability_weight_init.into(),
                Some(PriceBias::High),
                OraclePriceType::TimeWeighted,
            ),
            (BalanceSide::Liabilities, RequirementType::Maintenance) => (
                self.bank.config.liability_weight_maint.into(),
                Some(PriceBias::High),
                OraclePriceType::RealTime,
            ),
            _ => (I80F48::ONE, None, OraclePriceType::RealTime),
        }
    }

    pub fn calc_amount(
        &self,
        value: I80F48,
        side: BalanceSide,
        requirement_type: RequirementType,
    ) -> anyhow::Result<I80F48> {
        let (_, price_bias, oracle_type) = self.get_pricing_params(side, requirement_type);

        let price = self
            .oracle_adapter
            .price_adapter
            .get_price_of_type(oracle_type, price_bias)
            .unwrap();

        Ok(calc_amount(value, price, self.bank.mint_decimals)?)
    }

    pub fn calc_value(
        &self,
        amount: I80F48,
        side: BalanceSide,
        requirement_type: RequirementType,
    ) -> anyhow::Result<I80F48> {
        let (_, price_bias, oracle_type) = self.get_pricing_params(side, requirement_type);

        let price = self
            .oracle_adapter
            .price_adapter
            .get_price_of_type(oracle_type, price_bias)
            .unwrap();

        Ok(calc_value(amount, price, self.bank.mint_decimals, None)?)
    }

    pub fn calc_weighted_value(
        &self,
        amount: I80F48,
        side: BalanceSide,
        requirement_type: RequirementType,
    ) -> anyhow::Result<I80F48> {
        let (weight, price_bias, oracle_type) = self.get_pricing_params(side, requirement_type);

        let price = self
            .oracle_adapter
            .price_adapter
            .get_price_of_type(oracle_type, price_bias)
            .unwrap();

        Ok(calc_value(
            amount,
            price,
            self.bank.mint_decimals,
            Some(weight),
        )?)
    }
}

pub struct TokenAccountWrapper {
    pub address: Pubkey,
    pub mint: Pubkey,
    pub balance: u64,
    pub mint_decimals: u8,
    pub bank: Arc<RwLock<BankWrapper>>,
}

impl TokenAccountWrapper {
    pub fn get_value(&self) -> Result<I80F48, Box<dyn std::error::Error>> {
        let ui_amount = {
            let amount = I80F48::from_num(self.balance);
            let decimal_scale = EXP_10_I80F48[self.mint_decimals as usize];

            amount
                .checked_div(decimal_scale)
                .ok_or("Failed to divide")?
        };
        let price = self
            .bank
            .read()
            .unwrap()
            .oracle_adapter
            .price_adapter
            .get_price_of_type(marginfi::state::price::OraclePriceType::RealTime, None)?;

        trace!(
            "Token account {} (mint: {}) balance: {} @ ${:.5} - ${:.5}",
            self.address,
            self.mint,
            ui_amount,
            price,
            ui_amount * price
        );

        Ok(ui_amount * price)
    }

    pub fn get_amount(&self) -> I80F48 {
        I80F48::from_num(self.balance)
    }
}

#[derive(Debug, serde::Deserialize, serde::Serialize, Clone)]
pub struct StateEngineConfig {
    pub rpc_url: String,
    pub yellowstone_endpoint: String,
    pub yellowstone_x_token: Option<String>,

    #[serde(
        default = "StateEngineConfig::default_marginfi_program_id",
        deserialize_with = "from_pubkey_string"
    )]
    pub marginfi_program_id: Pubkey,
    #[serde(
        default = "StateEngineConfig::default_marginfi_group_address",
        deserialize_with = "from_pubkey_string"
    )]
    pub marginfi_group_address: Pubkey,
    #[serde(deserialize_with = "from_pubkey_string")]
    pub signer_pubkey: Pubkey,
    #[serde(default = "StateEngineConfig::default_skip_account_loading")]
    /// Skip loading of marginfi accounts on startup
    pub skip_account_loading: bool,
}

impl StateEngineConfig {
    pub fn get_geyser_service_config(&self) -> GeyserServiceConfig {
        GeyserServiceConfig {
            endpoint: self.yellowstone_endpoint.clone(),
            x_token: self.yellowstone_x_token.clone(),
        }
    }

    pub fn default_marginfi_program_id() -> Pubkey {
        marginfi::id()
    }

    pub fn default_marginfi_group_address() -> Pubkey {
        pubkey!("4qp6Fx6tnZkY5Wropq9wUYgtFxXKwE6viZxFHg3rdAG8")
    }

    pub fn default_skip_account_loading() -> bool {
        false
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StateEngineError {
    #[error("Failed to load from RPC")]
    RpcError,
    #[error("Bank not found")]
    NotFound,
}

pub struct StateEngineService {
    nb_rpc_client: Arc<solana_client::nonblocking::rpc_client::RpcClient>,
    pub rpc_client: Arc<solana_client::rpc_client::RpcClient>,
    anchor_client: anchor_client::Client<Arc<Keypair>>,
    pub marginfi_accounts: Arc<DashMap<Pubkey, Arc<RwLock<MarginfiAccountWrapper>>>>,
    pub banks: Arc<DashMap<Pubkey, Arc<RwLock<BankWrapper>>>>,
    pub token_accounts: Arc<DashMap<Pubkey, Arc<RwLock<TokenAccountWrapper>>>>,
    pub sol_accounts: DashMap<Pubkey, Account>,
    pub token_account_manager: TokenAccountManager,
    config: StateEngineConfig,
    accounts_to_track: Arc<RwLock<Vec<Pubkey>>>,
    oracle_to_bank_map: DashMap<Pubkey, Vec<Arc<RwLock<BankWrapper>>>>,
    pub mint_to_bank_map: DashMap<Pubkey, Vec<Arc<RwLock<BankWrapper>>>>,
    tracked_oracle_accounts: DashSet<Pubkey>,
    tracked_token_accounts: DashSet<Pubkey>,
    update_tx: Sender<()>,
}

impl StateEngineService {
    pub fn new(config: StateEngineConfig) -> anyhow::Result<(Arc<Self>, Receiver<()>)> {
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

        let (update_tx, update_rx) = crossbeam::channel::bounded(1000);

        let token_account_manager = TokenAccountManager::new(rpc_client.clone())?;

        let state_engine_service = Arc::new(Self {
            marginfi_accounts: Arc::new(DashMap::new()),
            banks: Arc::new(DashMap::new()),
            token_accounts: Arc::new(DashMap::new()),
            sol_accounts: DashMap::new(),
            anchor_client,
            config: config.clone(),
            nb_rpc_client,
            rpc_client,
            accounts_to_track: Arc::new(RwLock::new(Vec::new())),
            oracle_to_bank_map: DashMap::new(),
            mint_to_bank_map: DashMap::new(),
            tracked_oracle_accounts: DashSet::new(),
            tracked_token_accounts: DashSet::new(),
            update_tx,
            token_account_manager,
        });

        Ok((state_engine_service, update_rx))
    }

    pub fn get_bank(&self, bank_pk: &Pubkey) -> Option<Arc<RwLock<BankWrapper>>> {
        self.banks.get(bank_pk).map(|bank| bank.value().clone())
    }

    /// TODO: Enable a liquidator to specify a preferred bank
    pub fn get_bank_for_mint(&self, mint: &Pubkey) -> Option<Arc<RwLock<BankWrapper>>> {
        self.mint_to_bank_map
            .get(mint)
            .map(|banks| banks.value().first().unwrap().clone())
    }

    pub async fn load(&self, liquidator_account: Pubkey) -> anyhow::Result<()> {
        debug!("StateEngineService::load");
        info!("Loading state engine service");

        self.load_oracles_and_banks().await?;
        self.load_token_accounts()?;
        self.load_sol_accounts()?;
        self.load_liquidator_account(liquidator_account)?;

        if !self.config.skip_account_loading {
            self.load_marginfi_accounts().await?;
        }

        Ok(())
    }

    pub fn get_accounts_to_track(&self) -> Vec<Pubkey> {
        let mut taracked_accounts = self
            .tracked_oracle_accounts
            .iter()
            .chain(self.tracked_token_accounts.iter())
            .map(|e| *e)
            .collect::<Vec<_>>();

        taracked_accounts.push(self.config.signer_pubkey);

        debug!("Getting {} accounts to track", taracked_accounts.len());

        taracked_accounts
    }

    async fn load_oracles_and_banks(&self) -> anyhow::Result<()> {
        let program: Program<Arc<Keypair>> = self
            .anchor_client
            .program(self.config.marginfi_program_id)?;
        let banks = program
            .accounts::<Bank>(vec![RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                BANK_GROUP_PK_OFFSET,
                self.config.marginfi_group_address.as_ref(),
            ))])
            .await?;

        debug!("Found {} banks", banks.len());

        let oracle_keys = banks
            .iter()
            .map(|(_, bank)| bank.config.oracle_keys[0])
            .collect::<Vec<_>>();

        let mut oracle_accounts = batch_get_multiple_accounts(
            self.rpc_client.clone(),
            &oracle_keys,
            BatchLoadingConfig::DEFAULT,
        )?;

        debug!("Found {} oracle accounts", oracle_accounts.len());

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
                            OraclePriceFeedAdapter::try_from_bank_config_with_max_age(
                                &bank.config,
                                &[oracle_ai_c],
                                0,
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

            self.mint_to_bank_map
                .entry(bank.mint)
                .and_modify(|vec| vec.push(bank_ref.clone()))
                .or_insert_with(|| vec![bank_ref.clone()]);

            self.tracked_oracle_accounts.insert(**oracle_address);
        }

        debug!("Done loading oracles and banks");

        Ok(())
    }

    pub fn load_sol_accounts(&self) -> anyhow::Result<()> {
        self.rpc_client
            .get_account(&self.config.signer_pubkey)
            .map(|account| {
                self.sol_accounts.insert(self.config.signer_pubkey, account);
            })?;

        Ok(())
    }

    pub fn load_liquidator_account(&self, liquidator_account: Pubkey) -> anyhow::Result<()> {
        let account = self.rpc_client.get_account(&liquidator_account)?;

        let marginfi_account = bytemuck::from_bytes::<MarginfiAccount>(&account.data[8..]);

        self.marginfi_accounts
            .entry(liquidator_account)
            .and_modify(|marginfi_account_ref| {
                let mut marginfi_account_guard = marginfi_account_ref.write().unwrap();
                marginfi_account_guard.account = marginfi_account.clone();
            })
            .or_insert_with(|| {
                Arc::new(RwLock::new(MarginfiAccountWrapper::new(
                    liquidator_account,
                    marginfi_account.clone(),
                    self.banks.clone(),
                )))
            });

        Ok(())
    }

    pub fn update_oracle(
        &self,
        oracle_address: &Pubkey,
        mut oracle_account: Account,
    ) -> anyhow::Result<()> {
        if let Some(banks_to_update) = self.oracle_to_bank_map.get(oracle_address) {
            let oracle_ai = (oracle_address, &mut oracle_account).into_account_info();

            debug!("Updating oracle {}", oracle_ai.key);

            for bank_to_update in banks_to_update.iter() {
                if let Ok(mut bank_to_update) = bank_to_update.try_write() {
                    bank_to_update.oracle_adapter.price_adapter =
                        OraclePriceFeedAdapter::try_from_bank_config_with_max_age(
                            &bank_to_update.bank.config,
                            &[oracle_ai.clone()],
                            0,
                            u64::MAX,
                        )?;
                } else {
                    warn!("Failed to acquire write lock on bank, oracle update skipped");
                }
            }
        } else {
            warn!("Received update for unknown oracle {}", oracle_address);
        }

        debug!("Done updating oracle {}", oracle_address);

        Ok(())
    }

    pub fn update_bank(&self, bank_address: &Pubkey, bank: Account) -> anyhow::Result<bool> {
        debug!("Updating bank {}", bank_address);
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

                let bank_entry = Arc::new(RwLock::new(BankWrapper::new(
                    *bank_address,
                    bank.clone(),
                    OracleWrapper::new(
                        oracle_address,
                        OraclePriceFeedAdapter::try_from_bank_config_with_max_age(
                            &bank.config,
                            &[oracle_account_ai],
                            i64::MAX,
                            u64::MAX,
                        )
                        .unwrap(),
                    ),
                )));

                self.mint_to_bank_map
                    .entry(bank.mint)
                    .and_modify(|vec| vec.push(bank_entry.clone()))
                    .or_insert_with(|| vec![bank_entry.clone()]);

                bank_entry
            });

        debug!("Done updating bank {}", bank_address);

        Ok(new_bank)
    }

    fn load_token_accounts(&self) -> anyhow::Result<()> {
        debug!("Loading token accounts");

        {
            let banks = self.banks.clone();
            let mut bank_mints = Vec::new();
            for bank in banks.iter() {
                let bank_guard = bank.read().unwrap();
                bank_mints.push(bank_guard.bank.mint);
            }

            self.token_account_manager
                .add_mints(&bank_mints, self.config.signer_pubkey)
                .map_err(|e| anyhow::anyhow!("Failed to add mints: {:?}", e))?;
        }

        let (mints, token_account_addresses) = self
            .token_account_manager
            .get_mints_and_token_account_addresses();

        let accounts = batch_get_multiple_accounts(
            self.rpc_client.clone(),
            &token_account_addresses,
            BatchLoadingConfig::DEFAULT,
        )?;

        debug!("Found {} token accounts", accounts.len());

        let token_accounts_with_addresses_and_mints = token_account_addresses
            .iter()
            .zip(mints.iter())
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
                    let token_account = Arc::clone(token_account);
                    let mut token_account_guard = token_account.write().unwrap();
                    token_account_guard.balance = balance;
                })
                .or_insert_with(|| {
                    let bank = self
                        .mint_to_bank_map
                        .get(mint)
                        .unwrap()
                        .value()
                        .first()
                        .unwrap()
                        .clone();

                    let mint_decimals = bank.read().unwrap().bank.mint_decimals;

                    let taw = TokenAccountWrapper {
                        address: **token_account_address,
                        mint: **mint,
                        balance,
                        mint_decimals,
                        bank,
                    };

                    Arc::new(RwLock::new(taw))
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
            .and_modify(|token_account| {
                let token_account = Arc::clone(token_account);
                let mut token_account_guard = token_account.write().unwrap();
                token_account_guard.balance = balance;
            })
            .or_insert_with(|| {
                let mint_account = self.rpc_client.get_account(&mint).unwrap();
                let decimals = spl_token::state::Mint::unpack(&mint_account.data)
                    .map_err(|e| anyhow::anyhow!("Failed to unpack mint: {:?}", e))
                    .unwrap()
                    .decimals;

                let bank = self
                    .mint_to_bank_map
                    .get(&mint)
                    .unwrap()
                    .value()
                    .first()
                    .unwrap()
                    .clone();

                Arc::new(RwLock::new(TokenAccountWrapper {
                    address: *token_account_address,
                    mint,
                    balance,
                    mint_decimals: decimals,
                    bank,
                }))
            });

        Ok(())
    }

    pub fn update_sol_account(
        &self,
        account_address: Pubkey,
        account: Account,
    ) -> anyhow::Result<()> {
        self.sol_accounts.insert(account_address, account);
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

    pub fn is_tracked_sol_account(&self, address: &Pubkey) -> bool {
        self.config.signer_pubkey == *address
    }

    async fn load_marginfi_accounts(&self) -> anyhow::Result<()> {
        debug!("Loading marginfi accounts");
        let start = std::time::Instant::now();

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
                        #[allow(deprecated)]
                        RpcFilterType::Memcmp(Memcmp {
                            offset: 8,
                            #[allow(deprecated)]
                            bytes: MemcmpEncodedBytes::Base58(
                                self.config.marginfi_group_address.to_string(),
                            ),
                            #[allow(deprecated)]
                            encoding: None,
                        }),
                        #[allow(deprecated)]
                        RpcFilterType::Memcmp(Memcmp {
                            offset: 0,
                            #[allow(deprecated)]
                            bytes: MemcmpEncodedBytes::Base58(
                                bs58::encode(MarginfiAccount::DISCRIMINATOR).into_string(),
                            ),
                            #[allow(deprecated)]
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

        debug!(
            "Found {} marginfi accounts",
            marginfi_account_addresses.len()
        );

        let mut marginfi_accounts = batch_get_multiple_accounts(
            self.rpc_client.clone(),
            &marginfi_account_pubkeys,
            BatchLoadingConfig::DEFAULT,
        )?;

        debug!("Fetched {} marginfi accounts", marginfi_accounts.len());

        for (address, account) in marginfi_account_addresses
            .iter()
            .zip(marginfi_accounts.iter_mut())
        {
            let account = account.as_ref().unwrap();
            self.update_marginfi_account(&address.0, &account)?;
        }

        debug!("Done loading marginfi accounts, tool {:?}", start.elapsed());

        Ok(())
    }

    pub fn update_marginfi_account(
        &self,
        marginfi_account_address: &Pubkey,
        account: &Account,
    ) -> anyhow::Result<()> {
        let marginfi_account = bytemuck::from_bytes::<MarginfiAccount>(&account.data[8..]);
        let marginfi_accounts = self.marginfi_accounts.clone();

        marginfi_accounts
            .entry(*marginfi_account_address)
            .and_modify(|marginfi_account_ref| {
                let mut marginfi_account_guard = marginfi_account_ref.write().unwrap();
                marginfi_account_guard.account = marginfi_account.clone();
            })
            .or_insert_with(|| {
                Arc::new(RwLock::new(MarginfiAccountWrapper::new(
                    *marginfi_account_address,
                    marginfi_account.clone(),
                    self.banks.clone(),
                )))
            });

        Ok(())
    }

    pub fn trigger_update_signal(&self) {
        match self.update_tx.send(()) {
            Ok(_) => debug!("Sent update signal"),
            Err(e) => error!("Failed to send update signal: {}", e),
        }
    }

    pub async fn start(self: &Arc<Self>) -> anyhow::Result<()> {
        let geyser_handle =
            GeyserService::connect(self.config.get_geyser_service_config(), self.clone()).await?;

        info!("StateEngineService connected to geyser");

        geyser_handle.await??;

        Ok(())
    }
}
