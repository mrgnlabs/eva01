use crate::{
    clock_manager::{self},
    config::{GeneralConfig, LiquidatorCfg},
    crossbar::CrossbarMaintainer,
    geyser::{AccountType, GeyserUpdate},
    metrics::{ERROR_COUNT, FAILED_LIQUIDATIONS, LIQUIDATION_ATTEMPTS, LIQUIDATION_LATENCY},
    thread_debug,
    transaction_manager::TransactionData,
    utils::{
        batch_get_multiple_accounts, find_oracle_keys, load_swb_pull_account_from_bytes,
        BankAccountWithPriceFeedEva, BatchLoadingConfig,
    },
    ward,
    wrappers::{
        bank::BankWrapper,
        liquidator_account::LiquidatorAccount,
        marginfi_account::MarginfiAccountWrapper,
        oracle::{OracleWrapper, OracleWrapperTrait},
    },
};
use anchor_client::Program;
use anchor_lang::Discriminator;
use core::panic;
use crossbeam::channel::{Receiver, Sender};
use fixed::types::I80F48;
use fixed_macro::types::I80F48;
use log::{debug, error, info};
use marginfi::{
    constants::{BANKRUPT_THRESHOLD, EXP_10_I80F48},
    state::{
        marginfi_account::{BalanceSide, MarginfiAccount, RequirementType},
        marginfi_group::{Bank, BankOperationalState, RiskTier},
        price::{
            OraclePriceFeedAdapter, OraclePriceType, OracleSetup, PriceBias,
            SwitchboardPullPriceFeed, SwitchboardV2PriceFeed,
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
    collections::{HashMap, HashSet},
    sync::{atomic::AtomicBool, Arc, Mutex, RwLock},
    time::Instant,
};
use switchboard_on_demand::PullFeedAccountData;
use tokio::runtime::{Builder, Runtime};

/// Bank group private key offset
const BANK_GROUP_PK_OFFSET: usize = 32 + 1 + 8;

pub struct Liquidator {
    liquidator_account: LiquidatorAccount,
    general_config: GeneralConfig,
    config: LiquidatorCfg,
    geyser_receiver: Receiver<GeyserUpdate>,
    marginfi_accounts: HashMap<Pubkey, MarginfiAccountWrapper>,
    banks: HashMap<Pubkey, BankWrapper>,
    oracle_to_bank: HashMap<Pubkey, Pubkey>,
    stop_liquidator: Arc<AtomicBool>,
    crossbar_client: CrossbarMaintainer,
    cache_oracle_needed_accounts: HashMap<Pubkey, Account>,
    clock: Arc<Mutex<Clock>>,
    tokio_rt: Runtime,
}

pub struct PreparedLiquidatableAccount {
    liquidatee_account: MarginfiAccountWrapper,
    asset_bank: BankWrapper,
    liab_bank: BankWrapper,
    asset_amount: u64,
    banks: HashMap<Pubkey, BankWrapper>,
    profit: u64,
}

impl Liquidator {
    /// Creates a new instance of the liquidator
    pub fn new(
        general_config: GeneralConfig,
        liquidator_config: LiquidatorCfg,
        geyser_receiver: Receiver<GeyserUpdate>,
        transaction_sender: Sender<TransactionData>,
        pending_liquidations: Arc<RwLock<HashSet<Pubkey>>>,
        stop_liquidator: Arc<AtomicBool>,
        clock: Arc<Mutex<Clock>>,
    ) -> anyhow::Result<Self> {
        let liquidator_account =
            LiquidatorAccount::new(transaction_sender, &general_config, pending_liquidations)?;

        let tokio_rt = Builder::new_multi_thread()
            .thread_name("liquidator")
            .worker_threads(2)
            .enable_all()
            .build()?;

        Ok(Liquidator {
            general_config,
            config: liquidator_config,
            geyser_receiver,
            marginfi_accounts: HashMap::new(),
            banks: HashMap::new(),
            liquidator_account,
            oracle_to_bank: HashMap::new(),
            stop_liquidator,
            crossbar_client: CrossbarMaintainer::new(),
            cache_oracle_needed_accounts: HashMap::new(),
            clock,
            tokio_rt,
        })
    }

    /// Loads necessary data to the liquidator
    pub fn load_data(&mut self) -> anyhow::Result<()> {
        info!("Loading Liquidator data.");

        let rpc_client = &RpcClient::new(self.general_config.rpc_url.clone());

        self.load_marginfi_accounts(rpc_client)?;
        info!("Loading banks...");
        self.load_oracles_and_banks(rpc_client)?;
        info!("Loading initial data for liquidator...");
        self.liquidator_account
            .load_initial_data(rpc_client, self.get_all_mints())?;
        info!("Loading staked banks...");

        let all_keys = self
            .banks
            .values()
            .filter(|b| b.bank.config.oracle_setup == OracleSetup::StakedWithPythPush)
            .flat_map(|bank| {
                vec![
                    bank.bank.config.oracle_keys[1],
                    bank.bank.config.oracle_keys[2],
                ]
            })
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();

        let all_accounts =
            batch_get_multiple_accounts(rpc_client, &all_keys, BatchLoadingConfig::DEFAULT)?;

        self.cache_oracle_needed_accounts = all_keys
            .into_iter()
            .zip(all_accounts)
            .map(|(key, acc)| (key, acc.unwrap()))
            .collect();
        Ok(())
    }

    /// Liquidator starts receiving messages and processing them
    pub fn start(&mut self) -> anyhow::Result<()> {
        info!("Staring the Liquidator loop.");
        let mut liquidator_updated = true;
        while let Ok(mut msg) = self.geyser_receiver.recv() {
            thread_debug!("Liquidator received geyser update: {:?}", msg);

            match msg.account_type {
                AccountType::Oracle => {
                    if let Some(bank_to_update_pk) = self.oracle_to_bank.get(&msg.address) {
                        let bank_to_update: &mut BankWrapper =
                            self.banks.get_mut(bank_to_update_pk).unwrap();

                        let oracle_price_adapter = match bank_to_update.bank.config.oracle_setup {
                            OracleSetup::SwitchboardPull => {
                                let mut offsets_data =
                                    [0u8; std::mem::size_of::<PullFeedAccountData>()];
                                offsets_data.copy_from_slice(
                                    &msg.account.data
                                        [8..std::mem::size_of::<PullFeedAccountData>() + 8],
                                );
                                let swb_feed =
                                    load_swb_pull_account_from_bytes(&offsets_data).unwrap();

                                let feed_hash = hex::encode(swb_feed.feed_hash);
                                bank_to_update.oracle_adapter.swb_feed_hash = Some(feed_hash);

                                OraclePriceFeedAdapter::SwitchboardPull(SwitchboardPullPriceFeed {
                                    feed: Box::new((&swb_feed).into()),
                                })
                            }
                            OracleSetup::StakedWithPythPush => {
                                debug!(
                                    "Getting update for STAKED oracle: {:?} for bank: {:?}",
                                    msg.address, bank_to_update_pk
                                );
                                let clock =
                                    ward!(clock_manager::get_clock(&self.clock).ok(), continue);
                                let keys = &bank_to_update.bank.config.oracle_keys[1..3];

                                let mut accounts_info =
                                    vec![(&msg.address, &mut msg.account).into_account_info()];

                                let mut owned_accounts: Vec<_> = keys
                                    .iter()
                                    .map(|key| {
                                        self.cache_oracle_needed_accounts
                                            .iter()
                                            .find(|(k, _)| *k == key)
                                            .unwrap()
                                            .1
                                            .clone()
                                    })
                                    .collect();

                                accounts_info.extend(
                                    keys.iter()
                                        .zip(owned_accounts.iter_mut())
                                        .map(|(key, account)| (key, account).into_account_info()),
                                );

                                OraclePriceFeedAdapter::try_from_bank_config(
                                    &bank_to_update.bank.config,
                                    &accounts_info,
                                    &clock,
                                )
                                .unwrap()
                            }
                            _ => {
                                let clock =
                                    ward!(clock_manager::get_clock(&self.clock).ok(), continue);
                                // info!(
                                //     "Getting update for oracle: {:?} for bank: {:?}",
                                //     msg.address, bank_to_update_pk
                                // );
                                let oracle_account_info =
                                    (&msg.address, &mut msg.account).into_account_info();
                                ward!(
                                    OraclePriceFeedAdapter::try_from_bank_config(
                                        &bank_to_update.bank.config,
                                        &[oracle_account_info],
                                        &clock,
                                    )
                                    // .map_err(|_| {
                                    //     //debug!("Error creating oracle price feed adapter: {:?}", e);
                                    // })
                                    .ok(),
                                    continue
                                )
                            }
                        };
                        bank_to_update.oracle_adapter.price_adapter = oracle_price_adapter;
                    }
                }
                AccountType::Marginfi => {
                    let marginfi_account =
                        bytemuck::from_bytes::<MarginfiAccount>(&msg.account.data[8..]);
                    if msg.address == self.general_config.liquidator_account {
                        thread_debug!("Received Liquidator account {:?} update.", msg.address);
                        let marginfi_account =
                            bytemuck::from_bytes::<MarginfiAccount>(&msg.account.data[8..]);

                        self.liquidator_account.account_wrapper.lending_account =
                            marginfi_account.lending_account;

                        liquidator_updated = true;
                    } else {
                        thread_debug!("Received Marginfi account {:?} update", msg.address);
                        self.marginfi_accounts
                            .entry(msg.address)
                            .and_modify(|mrgn_account| {
                                mrgn_account.lending_account = marginfi_account.lending_account;
                            })
                            .or_insert_with(|| {
                                MarginfiAccountWrapper::new(
                                    msg.address,
                                    marginfi_account.lending_account,
                                )
                            });
                    }
                }
                _ => {}
            };

            if self
                .stop_liquidator
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                break;
            }
            if !liquidator_updated {
                debug!("Liquidator account not updated, skipping liquidation...");
                continue;
            }
            if let Ok(mut accounts) = self.process_all_accounts() {
                // Accounts are sorted from the highest profit to the lowest
                accounts.sort_by(|a, b| a.profit.cmp(&b.profit));
                accounts.reverse();
                for account in accounts {
                    LIQUIDATION_ATTEMPTS.inc();
                    let start = Instant::now();
                    if let Err(e) = self.liquidator_account.liquidate(
                        &account.liquidatee_account,
                        &account.asset_bank,
                        &account.liab_bank,
                        account.asset_amount,
                        &account.banks,
                    ) {
                        error!(
                            "Failed to liquidate account {:?}, error: {:?}",
                            account.liquidatee_account.address, e
                        );
                        FAILED_LIQUIDATIONS.inc();
                        ERROR_COUNT.inc();
                    }
                    let duration = start.elapsed().as_secs_f64();
                    LIQUIDATION_LATENCY.observe(duration);

                    // We currently allow only one liquidation at a time because the next one can have a non-actual state of the liquidator account
                    liquidator_updated = false;
                    if !liquidator_updated {
                        break;
                    }
                }
            }
        }

        info!("The Liquidator loop is stopped.");
        Ok(())
    }

    /// Checks if liquidation is needed, for each account one by one
    fn process_all_accounts(&mut self) -> anyhow::Result<Vec<PreparedLiquidatableAccount>> {
        // Update switchboard pull prices with crossbar
        let swb_feed_hashes = self
            .banks
            .values()
            .filter_map(|bank| {
                bank.oracle_adapter
                    .swb_feed_hash
                    .as_ref()
                    .map(|feed_hash| (bank.address, feed_hash.clone()))
            })
            .collect::<Vec<_>>();

        let simulated_prices = self
            .tokio_rt
            .block_on(self.crossbar_client.simulate(swb_feed_hashes));

        for (bank_pk, price) in simulated_prices {
            let bank = self.banks.get_mut(&bank_pk).unwrap();
            bank.oracle_adapter.simulated_price = Some(price);
        }

        let accounts = self
            .marginfi_accounts
            .par_iter()
            .filter_map(|(_, account)| self.process_account(account))
            .collect::<Vec<_>>();

        Ok(accounts)
    }

    fn process_account(
        &self,
        account: &MarginfiAccountWrapper,
    ) -> Option<PreparedLiquidatableAccount> {
        if self
            .liquidator_account
            .pending_liquidations
            .read()
            .unwrap()
            .contains(&account.address)
        {
            return None;
        }

        let (deposit_shares, liabs_shares) = account.get_deposits_and_liabilities_shares();

        let deposit_values = self
            .get_value_of_shares(
                deposit_shares,
                &BalanceSide::Assets,
                RequirementType::Maintenance,
            )
            .ok()?;

        let liab_values = self
            .get_value_of_shares(
                liabs_shares,
                &BalanceSide::Liabilities,
                RequirementType::Maintenance,
            )
            .ok()?;

        let (asset_bank_pk, liab_bank_pk) = self
            .find_liquidation_bank_candidates(deposit_values, liab_values)
            .inspect_err(|e| {
                error!("Error finding liquidation bank candidates: {:?}", e);
                ERROR_COUNT.inc();
            })
            .ok()
            .flatten()?;

        let (max_liquidatable_amount, profit) = self
            .compute_max_liquidatable_asset_amount_with_banks(
                account,
                &asset_bank_pk,
                &liab_bank_pk,
            )
            .ok()?;

        if max_liquidatable_amount.is_zero() {
            return None;
        }

        let max_liab_coverage_value = self.get_max_borrow_for_bank(&liab_bank_pk).unwrap();

        let liab_bank = self.banks.get(&liab_bank_pk).unwrap();
        let asset_bank = self.banks.get(&asset_bank_pk).unwrap();

        let liquidation_asset_amount_capacity = asset_bank
            .calc_amount(
                max_liab_coverage_value,
                BalanceSide::Assets,
                RequirementType::Initial,
            )
            .ok()?;

        let asset_amount_to_liquidate =
            min(max_liquidatable_amount, liquidation_asset_amount_capacity);

        let slippage_adjusted_asset_amount = asset_amount_to_liquidate * I80F48!(0.90);

        debug!(
                "Liquidation asset amount capacity: {:?}, asset_amount_to_liquidate: {:?}, slippage_adjusted_asset_amount: {:?}",
                liquidation_asset_amount_capacity, asset_amount_to_liquidate, slippage_adjusted_asset_amount
            );

        debug!(
            "Account {:?} health = {:?}",
            account.address,
            self.calc_health(account, RequirementType::Maintenance, true)
                .unwrap()
        );

        Some(PreparedLiquidatableAccount {
            liquidatee_account: account.clone(),
            asset_bank: asset_bank.clone(),
            liab_bank: liab_bank.clone(),
            asset_amount: slippage_adjusted_asset_amount.to_num(),
            banks: self.banks.clone(),
            profit: profit.to_num(),
        })
    }

    fn get_max_borrow_for_bank(&self, bank_pk: &Pubkey) -> anyhow::Result<I80F48> {
        let free_collateral = self.get_free_collateral();

        let bank = self.banks.get(bank_pk).unwrap();

        let (asset_amount, _) =
            self.get_balance_for_bank(&self.liquidator_account.account_wrapper, bank_pk)?;
        debug!(
            "Liquidator Asset amount: {:?}, free collateral: {:?}",
            asset_amount, free_collateral
        );

        let untied_collateral_for_bank = min(
            free_collateral,
            bank.calc_value(asset_amount, BalanceSide::Assets, RequirementType::Initial)?,
        );
        debug!(
            "Liquidator Untied collateral for bank: {:?}",
            untied_collateral_for_bank
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

        let max_borrow_amount = if asset_weight == I80F48::ZERO {
            let max_additional_borrow_ui =
                (free_collateral - untied_collateral_for_bank) / (higher_price * liab_weight);

            let max_additional = max_additional_borrow_ui * EXP_10_I80F48[token_decimals];

            max_additional + asset_amount
        } else {
            let ui_amount = untied_collateral_for_bank / (lower_price * asset_weight)
                + (free_collateral - untied_collateral_for_bank) / (higher_price * liab_weight);

            ui_amount * EXP_10_I80F48[token_decimals]
        };
        let max_borrow_value = bank.calc_value(
            max_borrow_amount,
            BalanceSide::Liabilities,
            RequirementType::Initial,
        )?;

        debug!(
            "Liquidator asset_weight: {:?}, max borrow amount: {:?}, max borrow value: {:?}",
            asset_weight, max_borrow_amount, max_borrow_value
        );

        Ok(max_borrow_value)
    }

    fn get_free_collateral(&self) -> I80F48 {
        let collateral = self
            .calc_health(
                &self.liquidator_account.account_wrapper,
                RequirementType::Initial,
                false,
            )
            .unwrap();

        if collateral > I80F48::ZERO {
            collateral
        } else {
            I80F48::ZERO
        }
    }

    fn find_liquidation_bank_candidates(
        &self,
        deposit_values: Vec<(I80F48, Pubkey)>,
        liab_values: Vec<(I80F48, Pubkey)>,
    ) -> anyhow::Result<Option<(Pubkey, Pubkey)>> {
        if deposit_values.is_empty() || liab_values.is_empty() {
            return Ok(None);
        }

        if deposit_values
            .iter()
            .map(|(v, _)| v.to_num::<f64>())
            .sum::<f64>()
            < BANKRUPT_THRESHOLD
        {
            return Ok(None);
        }

        let (_, asset_bank) = deposit_values
            .iter()
            .max_by(|a, b| {
                //debug!("Asset Bank {:?} value: {:?}", a.1, a.0);
                a.0.cmp(&b.0)
            })
            .ok_or_else(|| anyhow::anyhow!("No asset bank found"))?;

        let (_, liab_bank) = liab_values
            .iter()
            .max_by(|a, b| {
                //debug!("Liab Bank {:?} value: {:?}", a.1, a.0);

                a.0.cmp(&b.0)
            })
            .ok_or_else(|| anyhow::anyhow!("No liability bank found"))?;

        Ok(Some((*asset_bank, *liab_bank)))
    }

    fn compute_max_liquidatable_asset_amount_with_banks(
        &self,
        account: &MarginfiAccountWrapper,
        asset_bank_pk: &Pubkey,
        liab_bank_pk: &Pubkey,
    ) -> anyhow::Result<(I80F48, I80F48)> {
        let maintenance_health = self.calc_health(account, RequirementType::Maintenance, false)?;
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
        let liab_weight_maint: I80F48 = liab_bank.bank.config.liability_weight_maint.into();

        let liquidation_discount = fixed_macro::types::I80F48!(0.95);

        let all = asset_weight_maint - liab_weight_maint * liquidation_discount;

        if all >= I80F48::ZERO {
            error!("Account {:?} has no liquidatable amount: {:?}, asset_weight_maint: {:?}, liab_weight_maint: {:?}", account.address, all, asset_weight_maint, liab_weight_maint);
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

        if liquidator_profit <= self.config.min_profit {
            return Ok((I80F48::ZERO, I80F48::ZERO));
        }

        let max_liquidatable_asset_amount = asset_bank.calc_amount(
            max_liquidatable_value,
            BalanceSide::Assets,
            RequirementType::Maintenance,
        )?;

        if liquidator_profit > self.config.min_profit {
            debug!("Account {:?}\nAsset Bank {:?}\nAsset maint weight: {:?}\nAsset Value (USD) {:?}\nLiab Bank {:?}\nLiab maint weight: {:?}\nLiab Value (USD) {:?}\nMax Liquidatable Value {:?}\nMax Liquidatable Asset Amount {:?}\nLiquidator profit (USD) {:?}", account.address, asset_bank.address, asset_bank.bank.config.asset_weight_maint, asset_value, liab_bank.address, liab_bank.bank.config.liability_weight_maint, liab_value, max_liquidatable_value,
                max_liquidatable_asset_amount, liquidator_profit);
        }

        Ok((max_liquidatable_asset_amount, liquidator_profit))
    }

    fn calc_health(
        &self,
        account: &MarginfiAccountWrapper,
        requirement_type: RequirementType,
        print_logs: bool,
    ) -> anyhow::Result<I80F48> {
        let baws = BankAccountWithPriceFeedEva::load(&account.lending_account, self.banks.clone())?;

        let (total_weighted_assets, total_weighted_liabilities) = baws.iter().fold(
            (I80F48::ZERO, I80F48::ZERO),
            |(total_assets, total_liabs), baw| {
                let (assets, liabs) = baw
                    .calc_weighted_assets_and_liabilities_values(requirement_type, print_logs)
                    .unwrap();
                if print_logs {
                    debug!(
                        "Account {:?}, Bank: {:?}\nAssets: {:?}\nLiabilities: {:?}",
                        account.address, baw.bank.address, assets, liabs
                    );
                }
                (total_assets + assets, total_liabs + liabs)
            },
        );

        Ok(total_weighted_assets - total_weighted_liabilities)
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
            .lending_account
            .balances
            .iter()
            .find(|b| b.bank_pk == *bank_pk && b.is_active())
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

    /// Loading marginfi accounts into the liquidator itself
    /// makes it easier and better, than holding it in a shared
    /// state engine, as it shouldn't be blocked by other threads
    pub fn load_marginfi_accounts(&mut self, rpc_client: &RpcClient) -> anyhow::Result<()> {
        info!("Loading marginfi accounts, this may take a few minutes, please wait!");
        let start = std::time::Instant::now();
        let marginfi_accounts_pubkeys = self.load_marginfi_account_addresses(rpc_client)?;

        let mut marginfi_accounts = batch_get_multiple_accounts(
            rpc_client,
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
                lending_account: marginfi_account.lending_account,
            };
            self.marginfi_accounts.insert(*address, maw);
        }

        info!("Loaded pubkeys in {:?}", start.elapsed());

        Ok(())
    }

    fn load_marginfi_account_addresses(
        &self,
        rpc_client: &RpcClient,
    ) -> anyhow::Result<Vec<Pubkey>> {
        info!("Loading marginfi account addresses...");
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

                info!(
                    "Loaded {} marginfi account addresses.",
                    marginfi_account_pubkeys.len()
                );
                Ok(marginfi_account_pubkeys)
            }
        }
    }

    /// Loads Oracles and banks into the Liquidator
    fn load_oracles_and_banks(&mut self, rpc_client: &RpcClient) -> anyhow::Result<()> {
        let anchor_client = anchor_client::Client::new(
            anchor_client::Cluster::Custom(self.general_config.rpc_url.clone(), String::from("")),
            Arc::new(Keypair::new()),
        );

        let program: Program<Arc<Keypair>> =
            anchor_client.program(self.general_config.marginfi_program_id)?;

        info!("Loading banks internally...");
        let banks =
            self.tokio_rt
                .block_on(program.accounts::<Bank>(vec![RpcFilterType::Memcmp(
                    Memcmp::new_base58_encoded(
                        BANK_GROUP_PK_OFFSET,
                        self.general_config.marginfi_group_address.as_ref(),
                    ),
                )]))?;

        info!("Fetched {} banks", banks.len());

        let oracle_keys = banks
            .iter()
            .flat_map(|(_, bank)| find_oracle_keys(&bank.config))
            .collect::<Vec<_>>();

        info!("Loading oracles...");
        let mut oracle_accounts =
            batch_get_multiple_accounts(rpc_client, &oracle_keys, BatchLoadingConfig::DEFAULT)?;

        let oracle_map: HashMap<Pubkey, Option<Account>> = oracle_keys
            .iter()
            .zip(oracle_accounts.iter_mut())
            .map(|(pk, account)| (*pk, account.take()))
            .collect();

        debug!("Filling the cache...");
        for (bank_address, bank) in banks.iter() {
            if bank.config.oracle_setup == OracleSetup::StakedWithPythPush {
                debug!("Loading STAKED bank: {:?}", bank_address);
            }

            let oracle_keys_excluded_default = bank
                .config
                .oracle_keys
                .iter()
                .filter(|key| *key != &Pubkey::default())
                .collect::<Vec<_>>();

            if oracle_keys_excluded_default.len() > 1
                && bank.config.oracle_setup != OracleSetup::StakedWithPythPush
            {
                error!(
                    "Bank {:?} has more than one oracle key, which is not supported",
                    bank_address
                );
                continue;
            }

            let (oracle_address, mut oracle_account) = {
                let oracle_addresses = find_oracle_keys(&bank.config);
                let mut oracle_account = None;
                let mut oracle_address = None;

                for address in oracle_addresses.iter() {
                    if let Some(Some(account)) = oracle_map.get(address) {
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
                    let swb_feed = load_swb_pull_account_from_bytes(&offsets_data).unwrap();

                    OraclePriceFeedAdapter::SwitchboardPull(SwitchboardPullPriceFeed {
                        feed: Box::new((&swb_feed).into()),
                    })
                }
                OracleSetup::SwitchboardV2 => {
                    let oracle_account_info =
                        (&oracle_address, &mut oracle_account).into_account_info();

                    OraclePriceFeedAdapter::SwitchboardV2(
                        SwitchboardV2PriceFeed::load_checked(&oracle_account_info, i64::MAX, 0)
                            .unwrap(),
                    )
                }
                OracleSetup::PythPushOracle => {
                    let oracle_account_info =
                        (&oracle_address, &mut oracle_account).into_account_info();
                    ward!(
                        OraclePriceFeedAdapter::try_from_bank_config(
                            &bank.config,
                            &[oracle_account_info],
                            &clock_manager::get_clock(&self.clock)?,
                        )
                        .ok(),
                        continue
                    )
                }
                OracleSetup::StakedWithPythPush => {
                    let keys = find_oracle_keys(&bank.config);

                    let mut oracle_account = oracle_map
                        .get(keys.first().unwrap())
                        .unwrap()
                        .clone()
                        .unwrap();
                    let mut oracle_account_infos =
                        vec![(keys.first().unwrap(), &mut oracle_account).into_account_info()];

                    // Remove the first key from the list
                    let mut keys = keys.clone();
                    keys.remove(0);

                    let accounts_map = oracle_map
                        .iter()
                        .filter(|(key, _)| keys.contains(key))
                        .map(|(key, address)| (*key, address.clone()))
                        .collect::<Vec<_>>();

                    let mut accounts: Vec<_> = accounts_map
                        .iter()
                        .map(|(_, account)| account.clone().unwrap())
                        .collect();

                    let remaining_account_infos: Vec<_> = accounts_map
                        .iter()
                        .zip(accounts.iter_mut())
                        .map(|((address, _), account)| (address, account).into_account_info())
                        .collect();

                    oracle_account_infos.extend(remaining_account_infos);

                    oracle_account_infos.sort_by_key(|info| {
                        let address = info.key;
                        keys.iter().position(|key| key == address)
                    });

                    ward!(
                        OraclePriceFeedAdapter::try_from_bank_config(
                            &bank.config,
                            &oracle_account_infos,
                            &clock_manager::get_clock(&self.clock)?,
                        )
                        .ok(),
                        continue
                    )
                }
                _ => {
                    panic!("Unknown oracle setup {:?}", bank.config.oracle_setup);
                }
            };

            if bank.config.oracle_setup == OracleSetup::StakedWithPythPush {
                debug!(
                    "Loaded STAKED bank: {:?} with oracle: {:?}",
                    bank_address, oracle_address
                );
            }
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
            tracked_accounts.insert(bank.oracle_adapter.address, AccountType::Oracle);
        }

        tracked_accounts
    }

    pub fn get_banks_and_map(&self) -> (HashMap<Pubkey, BankWrapper>, HashMap<Pubkey, Pubkey>) {
        (self.banks.clone(), self.oracle_to_bank.clone())
    }

    fn get_value_of_shares(
        &self,
        shares: Vec<(I80F48, Pubkey)>,
        balance_side: &BalanceSide,
        requirement_type: RequirementType,
    ) -> anyhow::Result<Vec<(I80F48, Pubkey)>> {
        let mut values: Vec<(I80F48, Pubkey)> = Vec::new();

        for (shares_amount, bank_pk) in shares {
            let bank = match self.banks.get(&bank_pk) {
                Some(bank) => bank,
                None => {
                    ERROR_COUNT.inc();
                    return Err(anyhow::anyhow!("Bank with pubkey {} not found", bank_pk));
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
                BalanceSide::Liabilities => {
                    let liabilities =
                        bank.bank.get_liability_amount(shares_amount).map_err(|e| {
                            anyhow::anyhow!("Couldn't calculate liability amount for: {}", e)
                        })?;
                    bank.calc_value(liabilities, BalanceSide::Liabilities, requirement_type)
                        .unwrap()
                }
                BalanceSide::Assets => {
                    let assets = bank.bank.get_asset_amount(shares_amount).map_err(|e| {
                        anyhow::anyhow!("Couldn't calculate asset amount for: {}", e)
                    })?;
                    bank.calc_value(assets, BalanceSide::Assets, requirement_type)
                        .unwrap()
                }
            };

            values.push((value, bank_pk));
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
