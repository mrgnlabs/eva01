use crate::{
    config::{GeneralConfig, LiquidatorCfg},
    crossbar::CrossbarMaintainer,
    geyser::{AccountType, GeyserUpdate},
    transaction_manager::BatchTransactions,
    utils::{
        batch_get_multiple_accounts, find_oracle_keys, BankAccountWithPriceFeedEva,
        BatchLoadingConfig,
    },
    wrappers::{
        bank::BankWrapper, liquidator_account::LiquidatorAccount,
        marginfi_account::MarginfiAccountWrapper, oracle::OracleWrapper,
    },
};
use anchor_client::Program;
use anchor_lang::Discriminator;
use crossbeam::channel::{Receiver, Sender};
use fixed::types::I80F48;
use fixed_macro::types::I80F48;
use log::{debug, error, info};
use marginfi::{
    constants::EXP_10_I80F48,
    state::{
        marginfi_account::{BalanceSide, MarginfiAccount, RequirementType},
        marginfi_group::{Bank, BankOperationalState, RiskTier},
        price::{
            OraclePriceFeedAdapter, OraclePriceType, OracleSetup, PriceBias,
            SwitchboardPullPriceFeed,
        },
    },
};
use rayon::prelude::*;
use solana_account_decoder::{UiAccountEncoding, UiDataSliceConfig};
use solana_client::{
    rpc_client::RpcClient,
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType},
};
use solana_program::pubkey::Pubkey;
use solana_sdk::{
    account::Account, account_info::IntoAccountInfo, bs58, clock::Clock, signature::Keypair,
};
use std::{
    cmp::min,
    collections::HashMap,
    sync::{atomic::AtomicBool, Arc},
};
use switchboard_on_demand::PullFeedAccountData;

/// Bank group private key offset
const BANK_GROUP_PK_OFFSET: usize = 32 + 1 + 8;

pub struct Liquidator {
    liquidator_account: LiquidatorAccount,
    general_config: GeneralConfig,
    config: LiquidatorCfg,
    geyser_receiver: Receiver<GeyserUpdate>,
    transaction_sender: Sender<BatchTransactions>,
    marginfi_accounts: HashMap<Pubkey, MarginfiAccountWrapper>,
    banks: HashMap<Pubkey, BankWrapper>,
    oracle_to_bank: HashMap<Pubkey, Pubkey>,
    stop_liquidation: Arc<AtomicBool>,
    crossbar_client: CrossbarMaintainer,
}

#[derive(Clone)]
pub struct LiquidatableAccount<'a> {
    account: &'a MarginfiAccountWrapper,
    asset_bank_pk: Pubkey,
    liab_bank_pk: Pubkey,
    max_liquidation_amount: I80F48,
    profit: I80F48,
}

impl<'a> LiquidatableAccount<'a> {
    pub fn new(
        account: &'a MarginfiAccountWrapper,
        asset_bank_pk: Pubkey,
        liab_bank_pk: Pubkey,
        max_liquidation_amount: I80F48,
        profit: I80F48,
    ) -> LiquidatableAccount {
        Self {
            account,
            asset_bank_pk,
            liab_bank_pk,
            max_liquidation_amount,
            profit,
        }
    }
}

pub struct PreparedLiquidatableAccount {
    liquidate_account: MarginfiAccountWrapper,
    asset_bank: BankWrapper,
    liab_bank: BankWrapper,
    asset_amount: u64,
    banks: HashMap<Pubkey, BankWrapper>,
    profit: u64,
}

impl Liquidator {
    /// Creates a new instance of the liquidator
    pub async fn new(
        general_config: GeneralConfig,
        liquidator_config: LiquidatorCfg,
        geyser_receiver: Receiver<GeyserUpdate>,
        transaction_sender: Sender<BatchTransactions>,
        stop_liquidation: Arc<AtomicBool>,
    ) -> Liquidator {
        let liquidator_account = LiquidatorAccount::new(
            RpcClient::new(general_config.rpc_url.clone()),
            general_config.liquidator_account,
            transaction_sender.clone(),
            general_config.clone(),
        )
        .await
        .unwrap();

        Liquidator {
            general_config,
            config: liquidator_config,
            geyser_receiver,
            transaction_sender,
            marginfi_accounts: HashMap::new(),
            banks: HashMap::new(),
            liquidator_account,
            oracle_to_bank: HashMap::new(),
            stop_liquidation,
            crossbar_client: CrossbarMaintainer::new(),
        }
    }

    /// Loads necessary data to the liquidator
    pub async fn load_data(&mut self) -> anyhow::Result<()> {
        let rpc_client = Arc::new(RpcClient::new(self.general_config.rpc_url.clone()));
        self.load_marginfi_accounts(rpc_client.clone()).await?;
        self.load_oracles_and_banks(rpc_client.clone()).await?;
        self.liquidator_account
            .load_initial_data(rpc_client.as_ref(), self.get_all_mints())
            .await?;
        Ok(())
    }

    /// Liquidator starts, receiving messages and process them,
    /// a "timeout" is awaiting for accounts to be evaluated
    pub async fn start(&mut self) -> anyhow::Result<()> {
        let max_duration = std::time::Duration::from_secs(5);
        loop {
            let start = std::time::Instant::now();
            while let Ok(mut msg) = self.geyser_receiver.recv() {
                debug!("Received message {:?}", msg);
                match msg.account_type {
                    AccountType::OracleAccount => {
                        if let Some(bank_to_update_pk) = self.oracle_to_bank.get(&msg.address) {
                            let bank_to_update: &mut BankWrapper =
                                self.banks.get_mut(bank_to_update_pk).unwrap();

                            let oracle_price_adapter = match bank_to_update.bank.config.oracle_setup
                            {
                                OracleSetup::SwitchboardPull => {
                                    let mut offsets_data =
                                        [0u8; std::mem::size_of::<PullFeedAccountData>()];
                                    offsets_data.copy_from_slice(
                                        &msg.account.data
                                            [8..std::mem::size_of::<PullFeedAccountData>() + 8],
                                    );
                                    let swb_feed = crate::utils::load_swb_pull_account_from_bytes(
                                        &offsets_data,
                                    )
                                    .unwrap();

                                    let feed_hash = hex::encode(swb_feed.feed_hash);
                                    bank_to_update.oracle_adapter.swb_feed_hash = Some(feed_hash);

                                    OraclePriceFeedAdapter::SwitchboardPull(
                                        SwitchboardPullPriceFeed {
                                            feed: Box::new((&swb_feed).into()),
                                        },
                                    )
                                }
                                _ => {
                                    let oracle_account_info =
                                        (&msg.address, &mut msg.account).into_account_info();
                                    OraclePriceFeedAdapter::try_from_bank_config_with_max_age(
                                        &bank_to_update.bank.config,
                                        &[oracle_account_info],
                                        &Clock::default(),
                                        i64::MAX as u64,
                                    )
                                    .unwrap()
                                }
                            };

                            bank_to_update.oracle_adapter.price_adapter = oracle_price_adapter;
                        }
                    }
                    AccountType::MarginfiAccount => {
                        let marginfi_account =
                            bytemuck::from_bytes::<MarginfiAccount>(&msg.account.data[8..]);
                        self.marginfi_accounts
                            .entry(msg.address)
                            .and_modify(|mrgn_account| {
                                mrgn_account.account = *marginfi_account;
                            })
                            .or_insert_with(|| {
                                MarginfiAccountWrapper::new(msg.address, *marginfi_account)
                            });
                    }
                    _ => {}
                };

                if start.elapsed() > max_duration {
                    if self
                        .stop_liquidation
                        .load(std::sync::atomic::Ordering::Relaxed)
                    {
                        break;
                    }
                    if let Ok(mut accounts) = self.process_all_accounts().await {
                        // Accounts are sorted from the highest profit to the lowest
                        accounts.sort_by(|a, b| a.profit.cmp(&b.profit));
                        accounts.reverse();
                        for account in accounts {
                            if let Err(e) = self
                                .liquidator_account
                                .liquidate(
                                    &account.liquidate_account,
                                    &account.asset_bank,
                                    &account.liab_bank,
                                    account.asset_amount,
                                    &account.banks,
                                )
                                .await
                            {
                                info!(
                                    "Failed to liquidate account {:?}, error: {:?}",
                                    account.liquidate_account.address, e
                                );
                            }
                        }
                    }
                    break;
                }
            }
        }
    }

    /// Starts processing/evaluate all account, checking
    /// if a liquidation is necessary/needed
    async fn process_all_accounts(&mut self) -> anyhow::Result<Vec<PreparedLiquidatableAccount>> {
        // Update switchboard pull prices with crossbar
        let swb_feed_hashes = self
            .banks
            .values()
            .filter_map(|bank| {
                if let Some(feed_hash) = &bank.oracle_adapter.swb_feed_hash {
                    Some((bank.address, feed_hash.clone()))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        let simulated_prices = self.crossbar_client.simulate(swb_feed_hashes).await;

        for (bank_pk, price) in simulated_prices {
            let bank = self.banks.get_mut(&bank_pk).unwrap();
            bank.oracle_adapter.simulated_price = Some(price);
        }

        let accounts = self
            .marginfi_accounts
            .par_iter()
            .filter_map(|(_, account)| {
                if !account.has_liabs() {
                    return None;
                }

                let (asset_bank_pk, liab_bank_pk) =
                    match self.find_liquidation_bank_candidates(account) {
                        Ok(Some((asset_bank_pk, liab_bank_pk))) => (asset_bank_pk, liab_bank_pk),
                        Ok(None) => return None,
                        Err(e) => {
                            error!("Error finding liquidation bank candidates: {:?}", e);
                            return None;
                        }
                    };

                let (max_liquidation_amount, profit) = self
                    .compute_max_liquidatble_asset_amount_with_banks(
                        account,
                        &asset_bank_pk,
                        &liab_bank_pk,
                    )
                    .map_err(|e| {
                        error!("Error computing max liquidatable asset amount: {:?}", e);
                    })
                    .ok()?;

                if max_liquidation_amount.is_zero() || profit < self.config.min_profit {
                    return None;
                }

                let max_liab_coverage_amount = self.get_max_borrow_for_bank(&liab_bank_pk).unwrap();

                let liab_bank = self.banks.get(&liab_bank_pk).unwrap();
                let asset_bank = self.banks.get(&asset_bank_pk).unwrap();

                let liquidation_asset_amount_capacity = asset_bank
                    .calc_amount(
                        max_liab_coverage_amount,
                        BalanceSide::Assets,
                        RequirementType::Initial,
                    )
                    .ok()?;

                let asset_amount_to_liquidate =
                    min(max_liquidation_amount, liquidation_asset_amount_capacity);

                let slippage_adjusted_asset_amount = asset_amount_to_liquidate * I80F48!(0.95);

                Some(PreparedLiquidatableAccount {
                    liquidate_account: account.clone(),
                    asset_bank: asset_bank.clone(),
                    liab_bank: liab_bank.clone(),
                    asset_amount: slippage_adjusted_asset_amount.to_num(),
                    banks: self.banks.clone(),
                    profit: profit.to_num(),
                })
            })
            .collect::<Vec<_>>();

        Ok(accounts)
    }

    fn get_max_borrow_for_bank(&self, bank_pk: &Pubkey) -> anyhow::Result<I80F48> {
        let free_collateral = self.get_free_collateral()?;

        let bank = self.banks.get(bank_pk).unwrap();

        let (asset_amount, _) =
            self.get_balance_for_bank(&self.liquidator_account.account_wrapper, bank_pk)?;

        let untied_collateral_for_bank = min(
            free_collateral,
            bank.calc_value(asset_amount, BalanceSide::Assets, RequirementType::Initial)?,
        );

        let asset_weight: I80F48 = bank.bank.config.asset_weight_init.into();
        let liab_weight: I80F48 = bank.bank.config.asset_weight_init.into();

        let lower_price = bank
            .oracle_adapter
            .get_price_of_type(OraclePriceType::TimeWeighted, Some(PriceBias::Low))?;

        let higher_price = bank
            .oracle_adapter
            .get_price_of_type(OraclePriceType::TimeWeighted, Some(PriceBias::High))?;

        let token_decimals = bank.bank.mint_decimals as usize;

        let max_borrow_ammount = if asset_weight == I80F48::ZERO {
            let max_additional_borrow_ui =
                (free_collateral - untied_collateral_for_bank) / (higher_price * liab_weight);

            let max_additional = max_additional_borrow_ui * EXP_10_I80F48[token_decimals];

            max_additional + asset_amount
        } else {
            let ui_amount = untied_collateral_for_bank / (lower_price * asset_weight)
                + (free_collateral - untied_collateral_for_bank) / (higher_price * liab_weight);

            ui_amount * EXP_10_I80F48[token_decimals]
        };

        Ok(max_borrow_ammount)
    }

    fn get_free_collateral(&self) -> anyhow::Result<I80F48> {
        let (assets, liabs) = self.calc_health(
            &self.liquidator_account.account_wrapper,
            RequirementType::Initial,
        );
        if assets > liabs {
            Ok(assets - liabs)
        } else {
            Ok(I80F48::ZERO)
        }
    }

    /// Finds banks that are candidates for liquidations
    fn find_liquidation_bank_candidates(
        &self,
        account: &MarginfiAccountWrapper,
    ) -> anyhow::Result<Option<(Pubkey, Pubkey)>> {
        let (deposit_shares, liabs_shares) = account.get_deposits_and_liabilities_shares();

        let deposit_values = self.get_value_of_shares(
            deposit_shares,
            &BalanceSide::Assets,
            RequirementType::Maintenance,
        )?;

        let liab_values = self.get_value_of_shares(
            liabs_shares,
            &BalanceSide::Liabilities,
            RequirementType::Maintenance,
        )?;

        if deposit_values.is_empty() || liab_values.is_empty() {
            return Ok(None);
        }

        let (_, asset_bank) = deposit_values
            .iter()
            .max_by(|a, b| a.0.cmp(&b.0))
            .ok_or_else(|| anyhow::anyhow!("No asset bank found"))?;

        let (_, liab_bank) = liab_values
            .iter()
            .max_by(|a, b| a.0.cmp(&b.0))
            .ok_or_else(|| anyhow::anyhow!("No liability bank found"))?;

        Ok(Some((*asset_bank, *liab_bank)))
    }

    /// Computes the max liquidatable asset amount
    fn compute_max_liquidatble_asset_amount_with_banks(
        &self,
        account: &MarginfiAccountWrapper,
        asset_bank_pk: &Pubkey,
        liab_bank_pk: &Pubkey,
    ) -> anyhow::Result<(I80F48, I80F48)> {
        let (assets, liabs) = self.calc_health(account, RequirementType::Maintenance);

        let maintenance_health = assets - liabs;

        if maintenance_health >= I80F48::ZERO {
            return Ok((I80F48::ZERO, I80F48::ZERO));
        }

        let asset_bank = self
            .banks
            .get(asset_bank_pk)
            .ok_or_else(|| anyhow::anyhow!("Asset bank {} not found", asset_bank_pk))?;

        let liab_bank = self
            .banks
            .get(liab_bank_pk)
            .ok_or_else(|| anyhow::anyhow!("Liab bank {} not found", liab_bank_pk))?;

        let asset_weight_maint: I80F48 = asset_bank.bank.config.asset_weight_maint.into();
        let liab_weight_maint: I80F48 = liab_bank.bank.config.asset_weight_maint.into();

        let liquidation_discount = fixed_macro::types::I80F48!(0.95);

        let all = asset_weight_maint - liab_weight_maint * liquidation_discount;

        if all == I80F48::ZERO {
            return Ok((I80F48::ZERO, I80F48::ZERO));
        }

        let underwater_maint_value =
            maintenance_health / (asset_weight_maint - liab_weight_maint * liquidation_discount);

        let (asset_amount, _) = self.get_balance_for_bank(account, asset_bank_pk)?;
        let (_, liab_amount) = self.get_balance_for_bank(account, liab_bank_pk)?;

        let asset_value = asset_bank.calc_value(
            asset_amount,
            BalanceSide::Assets,
            RequirementType::Maintenance,
        )?;

        let liab_value = liab_bank.calc_value(
            liab_amount,
            BalanceSide::Liabilities,
            RequirementType::Maintenance,
        )?;

        let max_liquidatable_value = min(min(asset_value, liab_value), underwater_maint_value);
        let liquidator_profit = max_liquidatable_value * fixed_macro::types::I80F48!(0.025);

        if liquidator_profit <= I80F48::ZERO {
            return Ok((I80F48::ZERO, I80F48::ZERO));
        }

        let max_liquidatable_asset_amount = asset_bank.calc_amount(
            max_liquidatable_value,
            BalanceSide::Assets,
            RequirementType::Maintenance,
        )?;

        if liquidator_profit > self.config.min_profit {
            debug!("Account {:?}", account.address);
            debug!("Health {:?}", maintenance_health);
            debug!("Liquidator profit {:?}", liquidator_profit);
        }

        Ok((max_liquidatable_asset_amount, liquidator_profit))
    }

    /// Calculates the health of a given account
    fn calc_health(
        &self,
        account: &MarginfiAccountWrapper,
        requirement_type: RequirementType,
    ) -> (I80F48, I80F48) {
        let baws =
            BankAccountWithPriceFeedEva::load(&account.account.lending_account, self.banks.clone())
                .unwrap();

        baws.iter().fold(
            (I80F48::ZERO, I80F48::ZERO),
            |(total_assets, total_liabs), baw| {
                let (assets, liabs) = baw
                    .calc_weighted_assets_and_liabilities_values(requirement_type)
                    .unwrap();
                (total_assets + assets, total_liabs + liabs)
            },
        )
    }

    /// Gets the balance for a given [`MarginfiAccount`] and [`Bank`]
    fn get_balance_for_bank(
        &self,
        account: &MarginfiAccountWrapper,
        bank_pk: &Pubkey,
    ) -> anyhow::Result<(I80F48, I80F48)> {
        let bank = self
            .banks
            .get(bank_pk)
            .ok_or_else(|| anyhow::anyhow!("Bank {} not bound", bank_pk))?;

        let balance = account
            .account
            .lending_account
            .balances
            .iter()
            .find(|b| b.bank_pk == *bank_pk)
            .map(|b| match b.get_side()? {
                BalanceSide::Assets => {
                    let amount = bank.bank.get_asset_amount(b.asset_shares.into()).ok()?;
                    Some((amount, I80F48::ZERO))
                }
                BalanceSide::Liabilities => {
                    let amount = bank
                        .bank
                        .get_liability_amount(b.liability_shares.into())
                        .ok()?;
                    Some((I80F48::ZERO, amount))
                }
            })
            .map(|e| e.unwrap_or_default())
            .unwrap_or_default();

        Ok(balance)
    }

    /// Will load marginfi accounts into the liquidator itself
    /// makes it easier and better, than holding it in a shared
    /// state engine, as it shouldn't be blocked by another threads
    pub async fn load_marginfi_accounts(
        &mut self,
        rpc_client: Arc<RpcClient>,
    ) -> anyhow::Result<()> {
        info!("Loading marginfi accounts, this may take a few minutes, please wait!");
        let start = std::time::Instant::now();
        let marginfi_accounts_pubkeys = self
            .load_marginfi_account_addresses(rpc_client.clone())
            .await?;

        let mut marginfi_accounts = batch_get_multiple_accounts(
            rpc_client.clone(),
            &marginfi_accounts_pubkeys,
            BatchLoadingConfig::DEFAULT,
        )?;

        info!("Fetched {} marginfi accounts", marginfi_accounts.len());

        for (address, account) in marginfi_accounts_pubkeys
            .iter()
            .zip(marginfi_accounts.iter_mut())
        {
            let account = account.as_ref().unwrap();
            let marginfi_account = bytemuck::from_bytes::<MarginfiAccount>(&account.data[8..]);
            let maw = MarginfiAccountWrapper {
                address: *address,
                account: *marginfi_account,
            };
            self.marginfi_accounts.insert(*address, maw);
        }

        info!("Loaded pubkeys in {:?}", start.elapsed());

        Ok(())
    }

    /// Loads all marginfi account address into a [`Vec`]
    async fn load_marginfi_account_addresses(
        &self,
        rpc_client: Arc<RpcClient>,
    ) -> anyhow::Result<Vec<Pubkey>> {
        match &self.general_config.account_whitelist {
            Some(account_list) => Ok(account_list.clone()),
            None => {
                let marginfi_account_addresses = rpc_client.get_program_accounts_with_config(
                    &self.general_config.marginfi_program_id,
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
                                    self.general_config.marginfi_group_address.to_string(),
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
                )?;

                let marginfi_account_pubkeys: Vec<Pubkey> = marginfi_account_addresses
                    .iter()
                    .map(|(pubkey, _)| *pubkey)
                    .collect();

                Ok(marginfi_account_pubkeys)
            }
        }
    }

    /// Loads Oracles and banks into the Liquidator
    async fn load_oracles_and_banks(&mut self, rpc_client: Arc<RpcClient>) -> anyhow::Result<()> {
        let anchor_client = anchor_client::Client::new(
            anchor_client::Cluster::Custom(self.general_config.rpc_url.clone(), String::from("")),
            Arc::new(Keypair::new()),
        );

        let program: Program<Arc<Keypair>> =
            anchor_client.program(self.general_config.marginfi_program_id)?;

        let banks = program
            .accounts::<Bank>(vec![RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                BANK_GROUP_PK_OFFSET,
                self.general_config.marginfi_group_address.as_ref(),
            ))])
            .await?;

        debug!("Found {} banks", banks.len());

        let oracle_keys = banks
            .iter()
            .map(|(_, bank)| find_oracle_keys(&bank.config))
            .flatten()
            .collect::<Vec<_>>();

        let mut oracle_accounts =
            batch_get_multiple_accounts(rpc_client, &oracle_keys, BatchLoadingConfig::DEFAULT)?;

        let oracle_map: HashMap<Pubkey, Option<Account>> = oracle_keys
            .iter()
            .zip(oracle_accounts.iter_mut())
            .map(|(pk, account)| (*pk, account.take()))
            .collect();

        info!("Found {:?} oracle accounts", oracle_accounts.len());

        for (bank_address, bank) in banks.iter() {
            let (oracle_address, mut oracle_account) = {
                let oracle_addresses = find_oracle_keys(&bank.config);
                let mut oracle_account = None;
                let mut oracle_address = None;

                for address in oracle_addresses.iter() {
                    if let Some(Some(account)) = oracle_map.get(&address) {
                        oracle_account = Some(account.clone());
                        oracle_address = Some(*address);
                        break;
                    }
                }

                (oracle_address.unwrap(), oracle_account.unwrap())
            };

            let price_adapter = match bank.config.oracle_setup {
                OracleSetup::SwitchboardPull => {
                    let mut offsets_data = [0u8; std::mem::size_of::<PullFeedAccountData>()];
                    offsets_data.copy_from_slice(
                        &oracle_account.data[8..std::mem::size_of::<PullFeedAccountData>() + 8],
                    );
                    let swb_feed =
                        crate::utils::load_swb_pull_account_from_bytes(&offsets_data).unwrap();

                    OraclePriceFeedAdapter::SwitchboardPull(SwitchboardPullPriceFeed {
                        feed: Box::new((&swb_feed).into()),
                    })
                }
                _ => {
                    let oracle_account_info =
                        (&oracle_address, &mut oracle_account).into_account_info();
                    OraclePriceFeedAdapter::try_from_bank_config_with_max_age(
                        &bank.config,
                        &[oracle_account_info],
                        &Clock::default(),
                        i64::MAX as u64,
                    )
                    .unwrap()
                }
            };

            self.banks.insert(
                *bank_address,
                BankWrapper::new(
                    *bank_address,
                    *bank,
                    OracleWrapper::new(oracle_address, price_adapter),
                ),
            );

            self.oracle_to_bank.insert(oracle_address, *bank_address);
        }

        Ok(())
    }

    pub fn get_accounts_to_track(&self) -> HashMap<Pubkey, AccountType> {
        let mut tracked_accounts: HashMap<Pubkey, AccountType> = HashMap::new();

        for bank in self.banks.values() {
            tracked_accounts.insert(bank.oracle_adapter.address, AccountType::OracleAccount);
        }

        tracked_accounts
    }

    pub fn get_banks_and_map(&self) -> (HashMap<Pubkey, BankWrapper>, HashMap<Pubkey, Pubkey>) {
        (self.banks.clone(), self.oracle_to_bank.clone())
    }

    fn get_value_of_shares(
        &self,
        tshares: Vec<(I80F48, Pubkey)>,
        balance_side: &BalanceSide,
        requirement_type: RequirementType,
    ) -> anyhow::Result<Vec<(I80F48, Pubkey)>> {
        let mut values: Vec<(I80F48, Pubkey)> = Vec::new();

        for share in tshares {
            let bank = match self.banks.get(&share.1) {
                Some(bank) => bank,
                None => {
                    return Err(anyhow::anyhow!("Bank with pubkey {} not found", share.1));
                }
            };

            if !self.config.isolated_banks
                && matches!(bank.bank.config.risk_tier, RiskTier::Isolated)
            {
                continue;
            }

            if !matches!(
                bank.bank.config.operational_state,
                BankOperationalState::Operational
            ) {
                continue;
            }

            let value = match balance_side {
                BalanceSide::Liabilities => bank
                    .calc_value(share.0, BalanceSide::Liabilities, requirement_type)
                    .unwrap(),
                BalanceSide::Assets => bank
                    .calc_value(share.0, BalanceSide::Assets, requirement_type)
                    .unwrap(),
            };

            values.push((value, share.1));
        }

        Ok(values)
    }

    fn get_all_mints(&self) -> Vec<Pubkey> {
        self.banks
            .values()
            .map(|bank| bank.bank.mint)
            .collect::<Vec<_>>()
    }
}
