use crate::{
    cache::Cache,
    config::{Eva01Config, TokenThresholds},
    metrics::{
        record_liquidation_failure, ACCOUNTS_SCANNED_TOTAL, ACCOUNT_SCAN_DURATION_SECONDS,
        ERROR_COUNT, FAILURE_REASON_INTERNAL, FAILURE_REASON_NOT_ENOUGH_FUNDS,
        FAILURE_REASON_RPC_ERROR, FAILURE_REASON_STALE_ORACLES, LIQUIDATABLE_ACCOUNTS_FOUND,
        LIQUIDATABLE_ACCOUNTS_FOUND_TOTAL, LIQUIDATION_SCAN_IN_PROGRESS,
    },
    rebalancer::Rebalancer,
    utils::{calc_total_weighted_assets_liabs, swb_cranker::SwbCranker},
    wrappers::{
        bank::BankWrapper,
        liquidator_account::{LiquidationError, LiquidatorAccount, PreparedLiquidatableAccount},
        marginfi_account::MarginfiAccountWrapper,
        oracle::{OracleWrapper, OracleWrapperTrait},
    },
};
use anyhow::{anyhow, Result};
use fixed::types::I80F48;
use fixed_macro::types::I80F48;
use log::{debug, error, info, warn};
use marginfi::state::{
    bank::BankImpl,
    marginfi_account::RequirementType,
    price::{OraclePriceType, PriceAdapter},
};
use marginfi_type_crate::{
    constants::BANKRUPT_THRESHOLD,
    types::{BalanceSide, BankOperationalState, RiskTier},
};
use solana_client::client_error::ClientError;
use solana_program::pubkey::Pubkey;
use std::{
    cmp::min,
    collections::{HashMap, HashSet},
    sync::{atomic::AtomicBool, Arc},
    time::{Duration, Instant},
};
use std::{sync::atomic::Ordering, thread};

#[cfg(feature = "publish_to_db")]
use crate::utils::supabase::SupabasePublisher;

const DECLARED_VALUE_RANGE: f64 = 0.2;

pub struct Liquidator {
    liquidator_account: Arc<LiquidatorAccount>,
    rebalancer: Rebalancer,
    min_profit: f64,
    run_liquidation: Arc<AtomicBool>,
    stop_liquidator: Arc<AtomicBool>,
    cache: Arc<Cache>,
    swb_cranker: SwbCranker,
    token_thresholds: HashMap<Pubkey, TokenThresholds>,
    token_dust_threshold: I80F48,
}

impl Liquidator {
    pub fn new(
        config: Eva01Config,
        liquidator_account: Arc<LiquidatorAccount>,
        run_liquidation: Arc<AtomicBool>,
        stop_liquidator: Arc<AtomicBool>,
        cache: Arc<Cache>,
    ) -> Result<Self> {
        let swb_cranker = SwbCranker::new(&config)?;

        let rebalancer =
            Rebalancer::new(config.clone(), liquidator_account.clone(), cache.clone())?;

        Ok(Liquidator {
            liquidator_account,
            rebalancer,
            min_profit: config.min_profit,
            run_liquidation,
            stop_liquidator,
            cache,
            swb_cranker,
            token_thresholds: config.token_thresholds,
            token_dust_threshold: config.token_dust_threshold,
        })
    }

    pub fn start(&mut self) -> Result<()> {
        // Fund the liquidator account, if needed
        if !self.liquidator_account.has_funds()? {
            warn!("Liquidator has no funds.");
        }

        self.rebalancer.run(HashMap::new())?;

        #[cfg(feature = "publish_to_db")]
        let mut supabase = SupabasePublisher::from_env()?;

        let mut liquidation_rounds = 0;

        info!("Staring the Liquidator loop.");
        while !self.stop_liquidator.load(Ordering::Relaxed) {
            debug!("Waiting for any data change...");
            if !self.run_liquidation.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_secs(1));
                continue;
            }

            liquidation_rounds += 1;

            info!("Running the Liquidation process...");
            self.run_liquidation.store(false, Ordering::Relaxed);

            // TODO: come up with a better heuristics here
            if liquidation_rounds % 5 == 0 {
                if let Err(e) = self.liquidator_account.refresh_integrations() {
                    error!("Integrations failed to refresh: {}", e);
                }
            }

            let mut missing_tokens: HashMap<Pubkey, I80F48> = HashMap::new();
            if let Ok(mut accounts) = self.evaluate_all_accounts() {
                // Accounts are sorted from the highest profit to the lowest
                accounts.sort_by(|a, b| a.profit.cmp(&b.profit));
                accounts.reverse();

                let mut stale_swb_oracles: HashSet<Pubkey> = HashSet::new();
                let mut tokens_in_shortage: HashSet<Pubkey> = HashSet::new();
                for acc in accounts {
                    // Get the liability mint for metrics
                    let liab_mint = self
                        .cache
                        .banks
                        .try_get_bank(&acc.liab_bank)
                        .ok()
                        .map(|bank| bank.bank.mint);

                    if let Err(e) = self.liquidator_account.liquidate(
                        &acc,
                        &stale_swb_oracles,
                        &mut tokens_in_shortage,
                    ) {
                        match e {
                            LiquidationError::Anyhow(e) => {
                                error!(
                                    "Failed to liquidate account {:?}: {:?}",
                                    acc.liquidatee_account.address, e
                                );
                                let reason = if e.downcast_ref::<ClientError>().is_some() {
                                    FAILURE_REASON_RPC_ERROR
                                } else {
                                    FAILURE_REASON_INTERNAL
                                };
                                record_liquidation_failure(reason, liab_mint, None);
                                ERROR_COUNT.inc();
                            }
                            LiquidationError::StaleOracles(swb_oracles) => {
                                stale_swb_oracles.extend(&swb_oracles);
                                // Recording the first one is simpler and still useful for debugging
                                let oracle = swb_oracles.first().copied();
                                record_liquidation_failure(
                                    FAILURE_REASON_STALE_ORACLES,
                                    None,
                                    oracle,
                                );
                            }
                            LiquidationError::NotEnoughFunds => {
                                missing_tokens
                                    .entry(acc.liab_bank)
                                    .and_modify(|m| *m += acc.liab_amount)
                                    .or_insert(acc.liab_amount);
                                record_liquidation_failure(
                                    FAILURE_REASON_NOT_ENOUGH_FUNDS,
                                    liab_mint,
                                    None,
                                );
                            }
                        }
                    }
                }
                if !stale_swb_oracles.is_empty() {
                    info!("Cranking Swb Oracles {:#?}", stale_swb_oracles);
                    if let Err(err) = self
                        .swb_cranker
                        .crank_oracles(stale_swb_oracles.into_iter().collect())
                    {
                        error!("Failed to crank Swb Oracles: {}", err)
                    }
                    info!("Completed cranking Swb Oracles.");
                };
            }

            info!("The Liquidation process is complete.");

            if let Err(error) = self.rebalancer.run(missing_tokens) {
                error!("Rebalancing failed: {:?}", error);
                ERROR_COUNT.inc();
            }

            #[cfg(feature = "publish_to_db")]
            if liquidation_rounds % 10000 == 0 {
                if let Err(e) = self.publish_stats(&mut supabase) {
                    error!("Failed to publish stats: {}", e);
                }
            }
        }
        info!("The Liquidator loop is stopped.");

        Ok(())
    }

    /// Checks if liquidation is needed, for each account one by one
    fn evaluate_all_accounts(&mut self) -> Result<Vec<PreparedLiquidatableAccount>> {
        LIQUIDATION_SCAN_IN_PROGRESS.set(1);
        let scan_started = Instant::now();
        let mut total_scanned: u64 = 0;

        let evaluation_result = {
            let mut index: usize = 0;
            let mut result: Vec<PreparedLiquidatableAccount> = vec![];
            while index < self.cache.marginfi_accounts.len()? {
                total_scanned += 1;
                match self.cache.marginfi_accounts.try_get_account_by_index(index) {
                    Ok(account) => {
                        if account.address == self.liquidator_account.liquidator_address {
                            index += 1;
                            continue;
                        }
                        match self.process_account(&account) {
                            Ok(acc_opt) => {
                                if let Some(acc) = acc_opt {
                                    result.push(acc);
                                }
                            }
                            Err(e) => {
                                debug!("Failed to process account {:?}: {:?}", account.address, e);
                                ERROR_COUNT.inc();
                            }
                        }
                    }
                    Err(err) => {
                        error!(
                            "Failed to get Marginfi account by index {}: {:?}",
                            index, err
                        );
                        ERROR_COUNT.inc();
                    }
                }
                index += 1;
            }

            Ok(result)
        };

        LIQUIDATION_SCAN_IN_PROGRESS.set(0);
        ACCOUNT_SCAN_DURATION_SECONDS.observe(scan_started.elapsed().as_secs_f64());
        ACCOUNTS_SCANNED_TOTAL.inc_by(total_scanned);

        match &evaluation_result {
            Ok(accounts) => {
                LIQUIDATABLE_ACCOUNTS_FOUND.set(accounts.len() as i64);
                LIQUIDATABLE_ACCOUNTS_FOUND_TOTAL.inc_by(accounts.len() as u64);
            }
            Err(_) => {
                LIQUIDATABLE_ACCOUNTS_FOUND.set(0);
            }
        }

        evaluation_result
    }

    #[cfg(feature = "publish_to_db")]
    fn publish_stats(&self, _supabase: &mut SupabasePublisher) -> Result<()> {
        // supabase.publish_health(
        //     account.address,
        //     total_weighted_assets.to_num::<f64>(),
        //     total_weighted_liabilities.to_num::<f64>(),
        //     maintenance_health.to_num::<f64>(),
        //     percentage_health,
        //     schedule as i64,
        // )?;

        Ok(())
    }

    fn process_account(
        &self,
        account: &MarginfiAccountWrapper,
    ) -> Result<Option<PreparedLiquidatableAccount>> {
        let (deposit_shares, liab_shares) = account.get_deposits_and_liabilities_shares();
        if liab_shares.is_empty() {
            return Ok(None);
        }

        let deposit_values = self.get_value_of_shares(
            deposit_shares,
            &BalanceSide::Assets,
            RequirementType::Maintenance,
        )?;

        let liab_values = self.get_value_of_shares(
            liab_shares,
            &BalanceSide::Liabilities,
            RequirementType::Maintenance,
        )?;

        let (asset_bank_pk, liab_bank_pk) =
            match self.find_liquidation_bank_candidates(deposit_values, liab_values)? {
                Some(banks) => banks,
                None => return Ok(None),
            };

        let asset_bank_wrapper = self.cache.banks.try_get_bank(&asset_bank_pk)?;
        let liab_bank_wrapper = self.cache.banks.try_get_bank(&liab_bank_pk)?;

        // Calculated max liquidatable amount is the defining factor for liquidation.
        let (
            max_liquidatable_asset_amount,
            max_liquidatable_liab_amount,
            profit,
            dust_liab_threshold,
        ) = self.compute_max_liquidatable_amounts_with_banks(
            account,
            &asset_bank_wrapper,
            &liab_bank_wrapper,
        )?;

        if max_liquidatable_asset_amount.is_zero() {
            return Ok(None);
        }

        let slippage_adjusted_asset_amount = max_liquidatable_asset_amount * I80F48!(0.90);
        let slippage_adjusted_liab_amount = max_liquidatable_liab_amount * I80F48!(0.90);

        debug!(
                "asset_amount_to_liquidate: {:?}, slippage_adjusted_asset_amount: {:?}, slippage_adjusted_liab_amount: {:?}",
                max_liquidatable_asset_amount, slippage_adjusted_asset_amount, slippage_adjusted_liab_amount
            );

        Ok(Some(PreparedLiquidatableAccount {
            liquidatee_account: account.clone(),
            asset_bank: asset_bank_pk,
            liab_bank: liab_bank_pk,
            asset_amount: slippage_adjusted_asset_amount,
            liab_amount: slippage_adjusted_liab_amount,
            profit: profit.to_num(),
            dust_liab_threshold,
        }))
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

    fn compute_max_liquidatable_amounts_with_banks(
        &self,
        account: &MarginfiAccountWrapper,
        asset_bank_wrapper: &BankWrapper,
        liab_bank_wrapper: &BankWrapper,
    ) -> Result<(I80F48, I80F48, I80F48, I80F48)> {
        let (total_weighted_assets, total_weighted_liabilities) = calc_total_weighted_assets_liabs(
            &self.cache,
            &account.lending_account,
            RequirementType::Maintenance,
        )?;
        let maintenance_health = total_weighted_assets - total_weighted_liabilities;
        debug!(
            "Account {} maintenance_health = {:?}",
            account.address, maintenance_health
        );
        if maintenance_health >= I80F48::ZERO {
            // TODO: revisit this crazy return type
            return Ok((I80F48::ZERO, I80F48::ZERO, I80F48::ZERO, I80F48::ZERO));
        }

        let asset_oracle_wrapper = OracleWrapper::build(&self.cache, &asset_bank_wrapper.address)?;
        let asset_price = asset_oracle_wrapper
            .price_adapter
            .get_price_of_type_ignore_conf(OraclePriceType::RealTime, None)?
            .to_num::<f64>();
        if let Some(thresholds) = self.token_thresholds.get(&asset_bank_wrapper.bank.mint) {
            let min_asset_price = thresholds.declared_value * (1.0 - DECLARED_VALUE_RANGE);
            if asset_price < min_asset_price {
                warn!(
                    "Asset ({}) price is lower than the declared range: {} < {}",
                    asset_bank_wrapper.bank.mint, asset_price, min_asset_price
                );
                return Ok((I80F48::ZERO, I80F48::ZERO, I80F48::ZERO, I80F48::ZERO));
            }
            let max_asset_price = thresholds.declared_value * (1.0 + DECLARED_VALUE_RANGE);
            if asset_price > max_asset_price {
                warn!(
                    "Asset ({}) price is higher than the declared range: {} > {}",
                    asset_bank_wrapper.bank.mint, asset_price, max_asset_price
                );
                return Ok((I80F48::ZERO, I80F48::ZERO, I80F48::ZERO, I80F48::ZERO));
            }
        }

        let liab_oracle_wrapper = OracleWrapper::build(&self.cache, &liab_bank_wrapper.address)?;
        let liab_price = liab_oracle_wrapper
            .price_adapter
            .get_price_of_type_ignore_conf(OraclePriceType::RealTime, None)?
            .to_num::<f64>();
        if let Some(thresholds) = self.token_thresholds.get(&liab_bank_wrapper.bank.mint) {
            let min_liab_price = thresholds.declared_value * (1.0 - DECLARED_VALUE_RANGE);
            if liab_price < min_liab_price {
                warn!(
                    "Liability ({}) price is lower than the declared range: {} < {}",
                    liab_bank_wrapper.bank.mint, liab_price, min_liab_price
                );
                return Ok((I80F48::ZERO, I80F48::ZERO, I80F48::ZERO, I80F48::ZERO));
            }
            let max_liab_price = thresholds.declared_value * (1.0 + DECLARED_VALUE_RANGE);
            if liab_price > max_liab_price {
                warn!(
                    "Liability ({}) price is higher than the declared range: {} > {}",
                    liab_bank_wrapper.bank.mint, liab_price, max_liab_price
                );
                return Ok((I80F48::ZERO, I80F48::ZERO, I80F48::ZERO, I80F48::ZERO));
            }
        }

        let asset_weight_maint: I80F48 = asset_bank_wrapper.bank.config.asset_weight_maint.into();
        let liab_weight_maint: I80F48 = liab_bank_wrapper.bank.config.liability_weight_maint.into();

        let liquidation_discount = fixed_macro::types::I80F48!(0.95);

        let all = asset_weight_maint - liab_weight_maint * liquidation_discount;

        if all >= I80F48::ZERO {
            debug!("Account {:?} has no liquidatable amount: {:?}, asset_weight_maint: {:?}, liab_weight_maint: {:?}", account.address, all, asset_weight_maint, liab_weight_maint);
            return Ok((I80F48::ZERO, I80F48::ZERO, I80F48::ZERO, I80F48::ZERO));
        }

        let underwater_maint_value =
            maintenance_health / (asset_weight_maint - liab_weight_maint * liquidation_discount);

        let (asset_amount, _) = self.get_balance_for_bank(account, asset_bank_wrapper)?;
        let (_, liab_amount) = self.get_balance_for_bank(account, liab_bank_wrapper)?;

        let asset_value = asset_bank_wrapper.calc_value(
            &asset_oracle_wrapper,
            asset_amount,
            BalanceSide::Assets,
            RequirementType::Maintenance,
        )?;

        let liab_value = liab_bank_wrapper.calc_value(
            &liab_oracle_wrapper,
            liab_amount,
            BalanceSide::Liabilities,
            RequirementType::Maintenance,
        )?;

        let max_liquidatable_value = min(min(asset_value, liab_value), underwater_maint_value);
        let liquidator_profit = max_liquidatable_value * fixed_macro::types::I80F48!(0.025);

        if liquidator_profit <= self.min_profit {
            return Ok((I80F48::ZERO, I80F48::ZERO, I80F48::ZERO, I80F48::ZERO));
        }

        let max_liquidatable_asset_amount = asset_bank_wrapper.calc_amount(
            &asset_oracle_wrapper,
            max_liquidatable_value,
            BalanceSide::Assets,
            RequirementType::Maintenance,
        )?;

        let max_liquidatable_liab_amount = liab_bank_wrapper.calc_amount(
            &liab_oracle_wrapper,
            max_liquidatable_value,
            BalanceSide::Liabilities,
            RequirementType::Maintenance,
        )?;

        let dust_liab_threshold = liab_bank_wrapper.calc_amount(
            &liab_oracle_wrapper,
            self.token_dust_threshold, // in USD
            BalanceSide::Liabilities,
            RequirementType::Equity,
        )?;

        debug!("Account {:?} liquidability evaluation:\nTotal weighted Assets {:?}\nTotal weighted Liabilities {:?}\nMaintenance health {:?}\n\
            Asset Bank {:?}\nAsset maint weight: {:?}\nAsset Amount {:?}\nAsset Value (USD) {:?}\n\
            Liab Bank {:?}\nLiab maint weight: {:?}\nLiab Amount {:?}\nLiab Value (USD) {:?}\n\
            Max Liquidatable Value {:?}\nMax Liquidatable Asset Amount {:?}\nMax Liquidatable Liab Amount {:?}\nLiquidator profit (USD) {:?}", 
            account.address, total_weighted_assets, total_weighted_liabilities, maintenance_health,
            asset_bank_wrapper.address, asset_bank_wrapper.bank.config.asset_weight_maint, asset_amount, asset_value,
            liab_bank_wrapper.address, liab_bank_wrapper.bank.config.liability_weight_maint, liab_amount, liab_value,
            max_liquidatable_value, max_liquidatable_asset_amount, max_liquidatable_liab_amount, liquidator_profit);

        Ok((
            max_liquidatable_asset_amount,
            max_liquidatable_liab_amount,
            liquidator_profit,
            dust_liab_threshold,
        ))
    }

    /// Gets the balance for a given [`MarginfiAccount`] and [`Bank`]
    // TODO: merge with `get_balance_for_bank` in `MarginfiAccountWrapper`
    fn get_balance_for_bank(
        &self,
        account: &MarginfiAccountWrapper,
        bank_wrapper: &BankWrapper,
    ) -> Result<(I80F48, I80F48)> {
        let balance = account
            .lending_account
            .balances
            .iter()
            .find(|b| b.bank_pk == bank_wrapper.address && b.is_active())
            .map(|b| match b.get_side()? {
                BalanceSide::Assets => {
                    let amount = bank_wrapper
                        .bank
                        .get_asset_amount(b.asset_shares.into())
                        .ok()?;
                    Some((amount, I80F48::ZERO))
                }
                BalanceSide::Liabilities => {
                    let amount = bank_wrapper
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

    fn get_value_of_shares(
        &self,
        shares: Vec<(I80F48, Pubkey)>,
        balance_side: &BalanceSide,
        requirement_type: RequirementType,
    ) -> Result<Vec<(I80F48, Pubkey)>> {
        let mut values: Vec<(I80F48, Pubkey)> = Vec::new();

        for (shares_amount, bank_pk) in shares {
            let bank_wrapper = self.cache.banks.try_get_bank(&bank_pk)?;
            let oracle_wrapper = OracleWrapper::build(&self.cache, &bank_pk)?;

            // TODO: add support for isolated or deprecate completely?
            if matches!(bank_wrapper.bank.config.risk_tier, RiskTier::Isolated) {
                continue;
            }

            if !matches!(
                bank_wrapper.bank.config.operational_state,
                BankOperationalState::Operational
            ) {
                continue;
            }

            // TODO: add Banks to Geyser!!!
            if bank_wrapper.bank.check_utilization_ratio().is_err() {
                debug!("Skipping bankrupt bank from evaluation: {}", bank_pk);
                continue;
            }

            let value = match balance_side {
                BalanceSide::Liabilities => {
                    let liabilities = bank_wrapper
                        .bank
                        .get_liability_amount(shares_amount)
                        .map_err(|e| anyhow!("Couldn't calculate liability amount for: {}", e))?;
                    let oracle_wrapper = OracleWrapper::build(&self.cache, &bank_pk)?;
                    bank_wrapper.calc_value(
                        &oracle_wrapper,
                        liabilities,
                        BalanceSide::Liabilities,
                        requirement_type,
                    )?
                }
                BalanceSide::Assets => {
                    let assets = bank_wrapper
                        .bank
                        .get_asset_amount(shares_amount)
                        .map_err(|e| anyhow!("Couldn't calculate asset amount for: {}", e))?;
                    bank_wrapper.calc_value(
                        &oracle_wrapper,
                        assets,
                        BalanceSide::Assets,
                        requirement_type,
                    )?
                }
            };

            values.push((value, bank_pk));
        }

        Ok(values)
    }
}
