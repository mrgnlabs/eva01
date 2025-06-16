use crate::{
    cache::Cache,
    config::{GeneralConfig, LiquidatorCfg},
    metrics::{ERROR_COUNT, FAILED_LIQUIDATIONS, LIQUIDATION_LATENCY},
    thread_debug, thread_error, thread_info, thread_trace,
    utils::{calc_total_weighted_assets_liabs, get_free_collateral, swb_cranker::SwbCranker},
    wrappers::{
        liquidator_account::LiquidatorAccount,
        marginfi_account::MarginfiAccountWrapper,
        oracle::{OracleWrapper, OracleWrapperTrait},
    },
};
use anyhow::{anyhow, Result};
use fixed::types::I80F48;
use fixed_macro::types::I80F48;
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
    sync::{atomic::AtomicBool, Arc},
    time::{Duration, Instant},
};
use std::{sync::atomic::Ordering, thread};

#[allow(dead_code)]
pub struct Liquidator {
    liquidator_account: LiquidatorAccount,
    config: LiquidatorCfg,
    min_profit: f64,
    run_liquidation: Arc<AtomicBool>,
    stop_liquidator: Arc<AtomicBool>,
    cache: Arc<Cache>,
    swb_cranker: SwbCranker,
}

pub struct PreparedLiquidatableAccount {
    liquidatee_account: MarginfiAccountWrapper,
    asset_bank: Pubkey,
    liab_bank: Pubkey,
    asset_amount: u64,
    profit: u64,
}

impl Liquidator {
    pub fn new(
        general_config: GeneralConfig,
        liquidator_config: LiquidatorCfg,
        liquidator_account: LiquidatorAccount,
        run_liquidation: Arc<AtomicBool>,
        stop_liquidator: Arc<AtomicBool>,
        cache: Arc<Cache>,
    ) -> Result<Self> {
        let swb_cranker = SwbCranker::new(&general_config)?;

        Ok(Liquidator {
            config: liquidator_config,
            min_profit: general_config.min_profit,
            run_liquidation,
            liquidator_account,
            stop_liquidator,
            cache,
            swb_cranker,
        })
    }

    pub fn start(&mut self) -> Result<()> {
        thread_info!("Staring the Liquidator loop.");
        while !self.stop_liquidator.load(Ordering::Relaxed) {
            if self.run_liquidation.load(Ordering::Relaxed) {
                thread_debug!("Running the Liquidation process...");
                self.run_liquidation.store(false, Ordering::Relaxed);

                if let Ok(mut accounts) = self.evaluate_all_accounts() {
                    // Accounts are sorted from the highest profit to the lowest
                    accounts.sort_by(|a, b| a.profit.cmp(&b.profit));
                    accounts.reverse();

                    let mut stale_swb_oracles: HashSet<Pubkey> = HashSet::new();
                    for candidate in accounts {
                        match self.process_account(&candidate.liquidatee_account) {
                            Ok(acc_opt) => {
                                if let Some(acc) = acc_opt {
                                    let start = Instant::now();
                                    if let Err(e) = self.liquidator_account.liquidate(
                                        &acc.liquidatee_account,
                                        &acc.asset_bank,
                                        &acc.liab_bank,
                                        acc.asset_amount,
                                        &stale_swb_oracles,
                                    ) {
                                        thread_error!(
                                            "Failed to liquidate account {:?}, error: {:?}",
                                            candidate.liquidatee_account.address,
                                            e.error
                                        );
                                        FAILED_LIQUIDATIONS.inc();
                                        ERROR_COUNT.inc();
                                        stale_swb_oracles.extend(e.keys);
                                    }
                                    let duration = start.elapsed().as_secs_f64();
                                    LIQUIDATION_LATENCY.observe(duration);
                                }
                            }
                            Err(e) => {
                                thread_trace!(
                                    "The account {:?} has failed the liquidation evaluation: {:?}",
                                    candidate.liquidatee_account.address,
                                    e
                                );
                                ERROR_COUNT.inc();
                            }
                        }
                    }
                    if !stale_swb_oracles.is_empty() {
                        thread_debug!("Cranking Swb Oracles {:#?}", stale_swb_oracles);
                        if let Err(err) = self
                            .swb_cranker
                            .crank_oracles(stale_swb_oracles.into_iter().collect())
                        {
                            thread_error!("Failed to crank Swb Oracles: {}", err)
                        }
                    };
                }

                thread_debug!("The Liquidation process is complete.");
            } else {
                thread::sleep(Duration::from_secs(1))
            }
        }
        thread_info!("The Liquidator loop is stopped.");

        Ok(())
    }

    /// Checks if liquidation is needed, for each account one by one
    fn evaluate_all_accounts(&mut self) -> Result<Vec<PreparedLiquidatableAccount>> {
        //        self.swb_price_simulator.simulate_swb_prices()?;

        let mut index: usize = 0;
        let mut result: Vec<PreparedLiquidatableAccount> = vec![];
        while index < self.cache.marginfi_accounts.len()? {
            match self.cache.marginfi_accounts.try_get_account_by_index(index) {
                Ok(account) => match self.process_account(&account) {
                    Ok(acc_opt) => {
                        if let Some(acc) = acc_opt {
                            result.push(acc);
                        }
                    }
                    Err(e) => {
                        thread_trace!("Failed to process account {:?}: {:?}", account.address, e);
                        ERROR_COUNT.inc();
                    }
                },
                Err(err) => {
                    thread_error!(
                        "Failed to get Marginfi account by index {}: {:?}",
                        index,
                        err
                    );
                    ERROR_COUNT.inc();
                }
            }
            index += 1;
        }

        Ok(result)
    }

    fn process_account(
        &self,
        account: &MarginfiAccountWrapper,
    ) -> Result<Option<PreparedLiquidatableAccount>> {
        let (deposit_shares, liabs_shares) = account.get_deposits_and_liabilities_shares();
        if liabs_shares.is_empty() {
            return Ok(None);
        }

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

        let (asset_bank_pk, liab_bank_pk) =
            match self.find_liquidation_bank_candidates(deposit_values, liab_values)? {
                Some(banks) => banks,
                None => return Ok(None),
            };

        // Calculated max liquidatable amount is the defining factor for liquidation.
        let (max_liquidatable_amount, profit) = self
            .compute_max_liquidatable_asset_amount_with_banks(
                account,
                &asset_bank_pk,
                &liab_bank_pk,
            )?;

        if max_liquidatable_amount.is_zero() {
            return Ok(None);
        }

        let max_liab_coverage_value = self.get_max_borrow_for_bank(&liab_bank_pk)?;

        // Asset
        let asset_bank_wrapper = self
            .cache
            .try_get_bank_wrapper::<OracleWrapper>(&asset_bank_pk)?;

        let liquidation_asset_amount_capacity = asset_bank_wrapper.calc_amount(
            max_liab_coverage_value,
            BalanceSide::Assets,
            RequirementType::Initial,
        )?;

        let asset_amount_to_liquidate =
            min(max_liquidatable_amount, liquidation_asset_amount_capacity);

        let slippage_adjusted_asset_amount = asset_amount_to_liquidate * I80F48!(0.90);

        thread_debug!(
                "Liquidation asset amount capacity: {:?}, asset_amount_to_liquidate: {:?}, slippage_adjusted_asset_amount: {:?}",
                liquidation_asset_amount_capacity, asset_amount_to_liquidate, slippage_adjusted_asset_amount
            );

        Ok(Some(PreparedLiquidatableAccount {
            liquidatee_account: account.clone(),
            asset_bank: asset_bank_pk,
            liab_bank: liab_bank_pk,
            asset_amount: slippage_adjusted_asset_amount.to_num(),
            profit: profit.to_num(),
        }))
    }

    fn get_max_borrow_for_bank(&self, bank_pk: &Pubkey) -> Result<I80F48> {
        let lq_account = &self
            .cache
            .marginfi_accounts
            .try_get_account(&self.liquidator_account.liquidator_address)?;

        let free_collateral = get_free_collateral(&self.cache, lq_account)?;

        let bank = self.cache.try_get_bank_wrapper::<OracleWrapper>(bank_pk)?;

        let (asset_amount, _) = self.get_balance_for_bank(lq_account, bank_pk)?;
        thread_debug!(
            "Liquidator Asset amount: {:?}, free collateral: {:?}",
            asset_amount,
            free_collateral
        );

        let untied_collateral_for_bank = min(
            free_collateral,
            bank.calc_value(asset_amount, BalanceSide::Assets, RequirementType::Initial)?,
        );
        thread_debug!(
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

        thread_debug!(
            "Liquidator asset_weight: {:?}, max borrow amount: {:?}, max borrow value: {:?}",
            asset_weight,
            max_borrow_amount,
            max_borrow_value
        );

        Ok(max_borrow_value)
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
        let (total_weighted_assets, total_weighted_liabilities) =
            calc_total_weighted_assets_liabs(&self.cache, account, RequirementType::Maintenance)?;
        let maintenance_health = total_weighted_assets - total_weighted_liabilities;
        thread_trace!(
            "Account {} maintenance_health = {:?}",
            account.address,
            maintenance_health
        );
        if maintenance_health >= I80F48::ZERO {
            return Ok((I80F48::ZERO, I80F48::ZERO));
        }

        let asset_bank = self
            .cache
            .try_get_bank_wrapper::<OracleWrapper>(asset_bank_pk)?;
        let liab_bank = self
            .cache
            .try_get_bank_wrapper::<OracleWrapper>(liab_bank_pk)?;

        let asset_weight_maint: I80F48 = asset_bank.bank.config.asset_weight_maint.into();
        let liab_weight_maint: I80F48 = liab_bank.bank.config.liability_weight_maint.into();

        let liquidation_discount = fixed_macro::types::I80F48!(0.95);

        let all = asset_weight_maint - liab_weight_maint * liquidation_discount;

        if all >= I80F48::ZERO {
            thread_error!("Account {:?} has no liquidatable amount: {:?}, asset_weight_maint: {:?}, liab_weight_maint: {:?}", account.address, all, asset_weight_maint, liab_weight_maint);
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

        if liquidator_profit <= self.min_profit {
            return Ok((I80F48::ZERO, I80F48::ZERO));
        }

        let max_liquidatable_asset_amount = asset_bank.calc_amount(
            max_liquidatable_value,
            BalanceSide::Assets,
            RequirementType::Maintenance,
        )?;

        thread_debug!("Account {:?} liquidability evaluation:\nTotal weighted Assets {:?}\nTotal weighted Liabilities {:?}\nMaintenance health {:?}\n\
            Asset Bank {:?}\nAsset maint weight: {:?}\nAsset Amount {:?}\nAsset Value (USD) {:?}\n\
            Liab Bank {:?}\nLiab maint weight: {:?}\nLiab Amount {:?}\nLiab Value (USD) {:?}\n\
            Max Liquidatable Value {:?}\nMax Liquidatable Asset Amount {:?}\nLiquidator profit (USD) {:?}", 
            account.address, total_weighted_assets, total_weighted_liabilities, maintenance_health,
            asset_bank.address, asset_bank.bank.config.asset_weight_maint, asset_amount, asset_value,
            liab_bank.address, liab_bank.bank.config.liability_weight_maint, liab_amount, liab_value,
            max_liquidatable_value,max_liquidatable_asset_amount, liquidator_profit);

        Ok((max_liquidatable_asset_amount, liquidator_profit))
    }

    /// Gets the balance for a given [`MarginfiAccount`] and [`Bank`]
    // TODO: merge with `get_balance_for_bank` in `MarginfiAccountWrapper`
    fn get_balance_for_bank(
        &self,
        account: &MarginfiAccountWrapper,
        bank_pk: &Pubkey,
    ) -> Result<(I80F48, I80F48)> {
        let bank = self
            .cache
            .banks
            .get_bank(bank_pk)
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
            let bank = self.cache.try_get_bank_wrapper::<OracleWrapper>(&bank_pk)?;

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

            if bank.bank.check_utilization_ratio().is_err() {
                thread_debug!("Skipping bankrupt bank from evaluation: {}", bank_pk);
                continue;
            }

            let value = match balance_side {
                BalanceSide::Liabilities => {
                    let liabilities = bank
                        .bank
                        .get_liability_amount(shares_amount)
                        .map_err(|e| anyhow!("Couldn't calculate liability amount for: {}", e))?;
                    bank.calc_value(liabilities, BalanceSide::Liabilities, requirement_type)?
                }
                BalanceSide::Assets => {
                    let assets = bank
                        .bank
                        .get_asset_amount(shares_amount)
                        .map_err(|e| anyhow!("Couldn't calculate asset amount for: {}", e))?;
                    bank.calc_value(assets, BalanceSide::Assets, requirement_type)?
                }
            };

            values.push((value, bank_pk));
        }

        Ok(values)
    }
}
