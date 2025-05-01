use crate::{
    cache::Cache,
    config::{GeneralConfig, LiquidatorCfg},
    metrics::{ERROR_COUNT, FAILED_LIQUIDATIONS, LIQUIDATION_ATTEMPTS, LIQUIDATION_LATENCY},
    thread_debug,
    transaction_manager::TransactionData,
    utils::{swb_cranker::SwbCranker, BankAccountWithPriceFeedEva},
    wrappers::{
        bank::BankWrapper, liquidator_account::LiquidatorAccount,
        marginfi_account::MarginfiAccountWrapper, oracle::OracleWrapperTrait,
    },
};
use anyhow::{anyhow, Result};
use crossbeam::channel::Sender;
use fixed::types::I80F48;
use fixed_macro::types::I80F48;
use log::{debug, error, info};
use marginfi::{
    constants::{BANKRUPT_THRESHOLD, EXP_10_I80F48},
    state::{
        marginfi_account::{BalanceSide, RequirementType},
        marginfi_group::{BankOperationalState, RiskTier},
        price::{OraclePriceType, PriceBias},
    },
};
use solana_program::pubkey::Pubkey;
use std::{
    cmp::min,
    collections::HashSet,
    sync::{atomic::AtomicBool, Arc, RwLock},
    time::{Duration, Instant},
};
use std::{sync::atomic::Ordering, thread};

pub struct Liquidator {
    liquidator_account: LiquidatorAccount,
    config: LiquidatorCfg,
    run_liquidation: Arc<AtomicBool>,
    stop_liquidator: Arc<AtomicBool>,
    cache: Arc<Cache>,
    swb_price_simulator: Arc<SwbCranker>,
}

pub struct PreparedLiquidatableAccount {
    liquidatee_account: MarginfiAccountWrapper,
    asset_bank: BankWrapper,
    liab_bank: BankWrapper,
    asset_amount: u64,
    profit: u64,
}

impl Liquidator {
    pub fn new(
        general_config: GeneralConfig,
        liquidator_config: LiquidatorCfg,
        run_liquidation: Arc<AtomicBool>,
        transaction_sender: Sender<TransactionData>,
        pending_liquidations: Arc<RwLock<HashSet<Pubkey>>>,
        stop_liquidator: Arc<AtomicBool>,
        cache: Arc<Cache>,
        swb_price_simulator: Arc<SwbCranker>,
    ) -> Result<Self> {
        let liquidator_account = LiquidatorAccount::new(
            transaction_sender,
            &general_config,
            pending_liquidations,
            cache.clone(),
        )?;

        Ok(Liquidator {
            config: liquidator_config,
            run_liquidation,
            liquidator_account,
            stop_liquidator,
            cache,
            swb_price_simulator,
        })
    }

    pub fn start(&mut self) -> Result<()> {
        info!("Staring the Liquidator loop.");
        while !self.stop_liquidator.load(Ordering::Relaxed) {
            if self.run_liquidation.load(Ordering::Relaxed) {
                thread_debug!("Running the Liquidation process...");
                self.run_liquidation.store(false, Ordering::Relaxed);

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
                    }
                }

                thread_debug!("The Liquidation process is complete.");
            } else {
                thread::sleep(Duration::from_millis(1000))
            }
        }
        info!("The Liquidator loop is stopped.");

        Ok(())
    }

    /// Checks if liquidation is needed, for each account one by one
    fn process_all_accounts(&mut self) -> Result<Vec<PreparedLiquidatableAccount>> {
        self.swb_price_simulator.simulate_swb_prices()?;

        let mut index: usize = 0;
        let mut accounts: Vec<PreparedLiquidatableAccount> = vec![];
        while index < self.cache.marginfi_accounts.len()? {
            if let Some(account) = self.cache.marginfi_accounts.get_account_by_index(index) {
                self.process_account(&account)
                    .into_iter()
                    .for_each(|acc| accounts.push(acc));
            }
            index += 1;
        }

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

        // Liability
        let max_liab_coverage_value = self.get_max_borrow_for_bank(&liab_bank_pk).unwrap();
        let liab_bank_wrapper = self
            .cache
            .try_get_bank_wrapper(&liab_bank_pk)
            .map_err(|err| {
                error!("Failed to create the Liability Bank Wrapper! {}", err);
            })
            .ok()?;

        // Asset
        let asset_bank_wrapper = self
            .cache
            .try_get_bank_wrapper(&asset_bank_pk)
            .map_err(|err| {
                error!("Failed to create the Asset Bank Wrapper! {}", err);
            })
            .ok()?;

        let liquidation_asset_amount_capacity = asset_bank_wrapper
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
            asset_bank: asset_bank_wrapper,
            liab_bank: liab_bank_wrapper,
            asset_amount: slippage_adjusted_asset_amount.to_num(),
            profit: profit.to_num(),
        })
    }

    fn get_max_borrow_for_bank(&self, bank_pk: &Pubkey) -> Result<I80F48> {
        let free_collateral = self.get_free_collateral()?;

        let bank = self.cache.try_get_bank_wrapper(bank_pk)?;

        let (asset_amount, _) = self.get_balance_for_bank(
            &self
                .cache
                .marginfi_accounts
                .try_get_account(&self.liquidator_account.liquidator_address)?,
            bank_pk,
        )?;
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

    fn get_free_collateral(&self) -> Result<I80F48> {
        let collateral = self.calc_health(
            &self
                .cache
                .marginfi_accounts
                .try_get_account(&self.liquidator_account.liquidator_address)?,
            RequirementType::Initial,
            false,
        )?;

        if collateral > I80F48::ZERO {
            Ok(collateral)
        } else {
            Ok(I80F48::ZERO)
        }
    }

    fn find_liquidation_bank_candidates(
        &self,
        deposit_values: Vec<(I80F48, Pubkey)>,
        liab_values: Vec<(I80F48, Pubkey)>,
    ) -> Result<Option<(Pubkey, Pubkey)>> {
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
            .ok_or_else(|| anyhow!("No asset bank found"))?;

        let (_, liab_bank) = liab_values
            .iter()
            .max_by(|a, b| {
                //debug!("Liab Bank {:?} value: {:?}", a.1, a.0);

                a.0.cmp(&b.0)
            })
            .ok_or_else(|| anyhow!("No liability bank found"))?;

        Ok(Some((*asset_bank, *liab_bank)))
    }

    fn compute_max_liquidatable_asset_amount_with_banks(
        &self,
        account: &MarginfiAccountWrapper,
        asset_bank_pk: &Pubkey,
        liab_bank_pk: &Pubkey,
    ) -> Result<(I80F48, I80F48)> {
        let maintenance_health = self.calc_health(account, RequirementType::Maintenance, false)?;
        if maintenance_health >= I80F48::ZERO {
            return Ok((I80F48::ZERO, I80F48::ZERO));
        }

        let asset_bank = self.cache.try_get_bank_wrapper(asset_bank_pk)?;
        let liab_bank = self.cache.try_get_bank_wrapper(liab_bank_pk)?;

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
    ) -> Result<I80F48> {
        let baws = BankAccountWithPriceFeedEva::load(&account.lending_account, self.cache.clone())?;

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
    ) -> Result<(I80F48, I80F48)> {
        let bank = self
            .cache
            .banks
            .get_account(bank_pk)
            .ok_or_else(|| anyhow!("Bank {} not bound", bank_pk))?;

        let balance = account
            .lending_account
            .balances
            .iter()
            .find(|b| b.bank_pk == *bank_pk && b.is_active())
            .map(|b| match b.get_side()? {
                BalanceSide::Assets => {
                    let amount = bank.get_asset_amount(b.asset_shares.into()).ok()?;
                    Some((amount, I80F48::ZERO))
                }
                BalanceSide::Liabilities => {
                    let amount = bank.get_liability_amount(b.liability_shares.into()).ok()?;
                    Some((I80F48::ZERO, amount))
                }
            })
            .map(|e| e.unwrap_or_default())
            .unwrap_or_default();

        Ok(balance)
    }

    fn get_value_of_shares(
        &self,
        shares: Vec<(I80F48, Pubkey)>,
        balance_side: &BalanceSide,
        requirement_type: RequirementType,
    ) -> Result<Vec<(I80F48, Pubkey)>> {
        let mut values: Vec<(I80F48, Pubkey)> = Vec::new();

        for (shares_amount, bank_pk) in shares {
            let bank = self.cache.try_get_bank_wrapper(&bank_pk)?;

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
                    let liabilities = bank
                        .bank
                        .get_liability_amount(shares_amount)
                        .map_err(|e| anyhow!("Couldn't calculate liability amount for: {}", e))?;
                    bank.calc_value(liabilities, BalanceSide::Liabilities, requirement_type)
                        .unwrap()
                }
                BalanceSide::Assets => {
                    let assets = bank
                        .bank
                        .get_asset_amount(shares_amount)
                        .map_err(|e| anyhow!("Couldn't calculate asset amount for: {}", e))?;
                    bank.calc_value(assets, BalanceSide::Assets, requirement_type)
                        .unwrap()
                }
            };

            values.push((value, bank_pk));
        }

        Ok(values)
    }
}
