use std::{
    cmp::min,
    error::Error,
    sync::{Arc, RwLock},
};

use anchor_lang::prelude::borsh::de;
use dashmap::DashMap;
use fixed::types::I80F48;
use log::{debug, trace};
use marginfi::state::marginfi_account::{BalanceSide, MarginfiAccount, RequirementType};
use solana_sdk::pubkey::Pubkey;

use crate::utils::BankAccountWithPriceFeedEva;

use super::engine::BankWrapper;

#[derive(Debug, thiserror::Error)]
pub enum MarginfiAccountWrapperError {
    #[error("Bank not found")]
    BankNotFound,
    #[error("RwLock error")]
    RwLockError,
    #[error("Error {0}")]
    Error(&'static str),
}

pub struct MarginfiAccountWrapper {
    pub address: Pubkey,
    pub account: MarginfiAccount,
    pub banks: Arc<DashMap<Pubkey, Arc<RwLock<BankWrapper>>>>,
}

impl MarginfiAccountWrapper {
    pub fn new(
        address: Pubkey,
        account: MarginfiAccount,
        banks: Arc<DashMap<Pubkey, Arc<RwLock<BankWrapper>>>>,
    ) -> Self {
        Self {
            address,
            account,
            banks,
        }
    }

    pub fn has_liabs(&self) -> bool {
        self.account
            .lending_account
            .balances
            .iter()
            .any(|a| a.active && matches!(a.get_side(), Some(BalanceSide::Liabilities)))
    }

    pub fn get_liabilites(&self) -> anyhow::Result<Vec<(I80F48, Pubkey)>> {
        Ok(self
            .account
            .lending_account
            .balances
            .iter()
            .filter(|b| matches!(b.get_side(), Some(BalanceSide::Liabilities)) && b.active)
            .filter_map(|b| {
                Some((
                    self.banks
                        .get(&b.bank_pk)
                        .unwrap()
                        .value()
                        .read()
                        .map(|bank| {
                            bank.bank
                                .get_liability_amount(b.liability_shares.into())
                                .ok()
                        })
                        .ok()??,
                    b.bank_pk,
                ))
            })
            .collect::<Vec<_>>())
    }

    pub fn get_liabilities_value(
        &self,
        requirement_type: RequirementType,
    ) -> anyhow::Result<Vec<(I80F48, Pubkey)>> {
        Ok(self
            .account
            .lending_account
            .balances
            .iter()
            .filter(|b| matches!(b.get_side(), Some(BalanceSide::Liabilities)) && b.active)
            .filter_map(|b| -> Option<(I80F48, Pubkey)> {
                let value = self
                    .banks
                    .get(&b.bank_pk)
                    .unwrap()
                    .value()
                    .read()
                    .map(|bank| -> Option<I80F48> {
                        let amount = bank
                            .bank
                            .get_liability_amount(b.liability_shares.into())
                            .ok()?;

                        let value = bank
                            .calc_value(amount, BalanceSide::Liabilities, requirement_type)
                            .ok()?;

                        Some(value)
                    })
                    .ok()??;

                Some((value, b.bank_pk))
            })
            .collect::<Vec<_>>())
    }

    pub fn get_deposits(
        &self,
        mints_to_exclude: &[Pubkey],
    ) -> anyhow::Result<Vec<(I80F48, Pubkey)>> {
        let mut deposits = vec![];

        for deposit_balance in self
            .account
            .lending_account
            .balances
            .iter()
            .filter(|b| matches!(b.get_side(), Some(BalanceSide::Assets)))
        {
            let bank_ref = self
                .banks
                .get(&deposit_balance.bank_pk)
                .ok_or(MarginfiAccountWrapperError::BankNotFound)?;

            let bank_wrapper = bank_ref
                .value()
                .read()
                .map_err(|_| MarginfiAccountWrapperError::RwLockError)?;

            if mints_to_exclude.contains(&bank_wrapper.bank.mint) {
                continue;
            }

            deposits.push((
                bank_wrapper
                    .bank
                    .get_asset_amount(deposit_balance.asset_shares.into())
                    .map_err(|_| {
                        MarginfiAccountWrapperError::Error("Failed to get asset amount")
                    })?,
                deposit_balance.bank_pk,
            ));
        }

        Ok(deposits)
    }

    pub fn get_deposits_values(
        &self,
        requirement_type: RequirementType,
    ) -> anyhow::Result<Vec<(I80F48, Pubkey)>> {
        let mut deposits = vec![];

        for deposit_balance in self
            .account
            .lending_account
            .balances
            .iter()
            .filter(|b| matches!(b.get_side(), Some(BalanceSide::Assets)))
        {
            let bank_ref = self
                .banks
                .get(&deposit_balance.bank_pk)
                .ok_or(MarginfiAccountWrapperError::BankNotFound)?;

            let bank_wrapper = bank_ref
                .value()
                .read()
                .map_err(|_| MarginfiAccountWrapperError::RwLockError)?;

            let amount = bank_wrapper
                .bank
                .get_asset_amount(deposit_balance.asset_shares.into())
                .map_err(|_| MarginfiAccountWrapperError::Error("Failed to get asset amount"))?;

            let value = bank_wrapper
                .calc_value(amount, BalanceSide::Assets, requirement_type)
                .map_err(|_| MarginfiAccountWrapperError::Error("Failed to calc value"))?;

            deposits.push((value, deposit_balance.bank_pk));
        }

        Ok(deposits)
    }

    pub fn get_balance_for_bank(
        &self,
        bank_pk: &Pubkey,
    ) -> Result<Option<(I80F48, BalanceSide)>, MarginfiAccountWrapperError> {
        let bank_ref = self
            .banks
            .get(bank_pk)
            .ok_or(MarginfiAccountWrapperError::BankNotFound)?;
        let bank = bank_ref
            .value()
            .read()
            .map_err(|_| MarginfiAccountWrapperError::RwLockError)?;

        let balance = self
            .account
            .lending_account
            .balances
            .iter()
            .find(|b| b.bank_pk == *bank_pk)
            .map(|b| match b.get_side()? {
                BalanceSide::Assets => {
                    let amount = bank.bank.get_asset_amount(b.asset_shares.into()).ok()?;
                    Some((amount, BalanceSide::Assets))
                }
                BalanceSide::Liabilities => {
                    let amount = bank
                        .bank
                        .get_liability_amount(b.liability_shares.into())
                        .ok()?;
                    Some((amount, BalanceSide::Liabilities))
                }
            })
            .ok_or(MarginfiAccountWrapperError::BankNotFound)?;

        Ok(balance)
    }

    pub fn get_balance_for_bank_2(
        &self,
        bank_pk: &Pubkey,
    ) -> Result<(I80F48, I80F48), MarginfiAccountWrapperError> {
        let bank_ref = self
            .banks
            .get(bank_pk)
            .ok_or(MarginfiAccountWrapperError::BankNotFound)?;
        let bank = bank_ref
            .value()
            .read()
            .map_err(|_| MarginfiAccountWrapperError::RwLockError)?;

        let balance = self
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

    pub fn calc_health(&self, requirement_type: RequirementType) -> (I80F48, I80F48) {
        let baws =
            BankAccountWithPriceFeedEva::load(&self.account.lending_account, self.banks.clone())
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

    pub fn get_observation_accounts(
        &self,
        banks_to_include: &[Pubkey],
        banks_to_exclude: &[Pubkey],
    ) -> Vec<Pubkey> {
        trace!(
            "Getting observation accounts, include: {:?}, exclude: {:?}",
            banks_to_include,
            banks_to_exclude
        );

        let mut ordered_active_banks = self
            .account
            .lending_account
            .balances
            .iter()
            .filter(|b| b.active && !banks_to_exclude.contains(&b.bank_pk))
            .map(|b| b.bank_pk)
            .collect::<Vec<_>>();

        ordered_active_banks.extend(banks_to_include.iter());

        trace!("Ordered active banks: {:?}", ordered_active_banks);

        let bank_accounts_and_oracles = ordered_active_banks
            .iter()
            .map(|b| {
                let bank_ref = self.banks.get(b).expect("Bank not found");
                let bank_wrapper = bank_ref.value().read().expect("RwLock error");

                vec![
                    bank_wrapper.address,
                    bank_wrapper.bank.config.oracle_keys[0],
                ]
            })
            .flatten()
            .collect::<Vec<Pubkey>>();

        trace!("Bank accounts and oracles: {:?}", bank_accounts_and_oracles);

        bank_accounts_and_oracles
    }

    /// Find the banks that are candidates for liquidation
    /// Returns the asset bank and the liability bank with the highest value
    pub fn find_liquidaiton_bank_canididates(&self) -> anyhow::Result<(Pubkey, Pubkey)> {
        let deposits = self.get_deposits_values(RequirementType::Maintenance)?;
        let liabs = self.get_liabilities_value(RequirementType::Maintenance)?;

        let (asset_value, asset_bank) = deposits
            .iter()
            .max_by(|a, b| a.0.cmp(&b.0))
            .ok_or_else(|| anyhow::anyhow!("No asset bank found"))?;

        let (liab_value, liab_bank) = liabs
            .iter()
            .max_by(|a, b| a.0.cmp(&b.0))
            .ok_or_else(|| anyhow::anyhow!("No liability bank found"))?;

        debug!(
            "Asset Bank: {:?}, Asset Value: {:?}, Liability Bank: {:?}, Liability Value: {:?}",
            asset_bank, asset_value, liab_bank, liab_value
        );

        Ok((*asset_bank, *liab_bank))
    }

    pub fn compute_max_liquidatable_asset_amount(&self) -> anyhow::Result<(I80F48, I80F48)> {
        let (asset_bank_pk, liab_bank_pk) = self.find_liquidaiton_bank_canididates()?;

        self.compute_max_liquidatable_asset_amount_with_banks(
            self.banks.clone(),
            &asset_bank_pk,
            &liab_bank_pk,
        )
    }

    pub fn compute_max_liquidatable_asset_amount_with_banks(
        &self,
        banks: Arc<DashMap<Pubkey, Arc<RwLock<BankWrapper>>>>,
        asset_bank_pk: &Pubkey,
        liab_bank_pk: &Pubkey,
    ) -> anyhow::Result<(I80F48, I80F48)> {
        let (assets, liabs) = self.calc_health(RequirementType::Maintenance);

        let maintenence_health = assets - liabs;

        if maintenence_health >= I80F48::ZERO {
            return Ok((I80F48::ZERO, I80F48::ZERO));
        }

        let asset_bank = banks
            .get(asset_bank_pk)
            .ok_or_else(|| anyhow::anyhow!("Asset bank {} not found", asset_bank_pk))?
            .clone();

        let liab_bank = banks
            .get(liab_bank_pk)
            .ok_or_else(|| anyhow::anyhow!("Liability bank {} not found", liab_bank_pk))?
            .clone();

        let asset_maint_weight: I80F48 = asset_bank
            .read()
            .unwrap()
            .bank
            .config
            .asset_weight_maint
            .into();

        let liab_maint_weight: I80F48 = liab_bank
            .read()
            .unwrap()
            .bank
            .config
            .liability_weight_maint
            .into();

        let liquidation_discount = fixed_macro::types::I80F48!(0.95);

        let underwater_maint_value =
            maintenence_health / (asset_maint_weight - liab_maint_weight * liquidation_discount);

        let (asset_amount, _) = self.get_balance_for_bank_2(&asset_bank_pk)?;

        let (_, liab_amount) = self.get_balance_for_bank_2(&liab_bank_pk)?;

        let asset_value = asset_bank.read().unwrap().calc_value(
            asset_amount,
            BalanceSide::Assets,
            RequirementType::Maintenance,
        )?;

        let liab_value = liab_bank.read().unwrap().calc_value(
            liab_amount,
            BalanceSide::Liabilities,
            RequirementType::Maintenance,
        )?;

        let max_liquidatable_value = min(min(asset_value, liab_value), underwater_maint_value);

        let liquidator_profit = max_liquidatable_value * fixed_macro::types::I80F48!(0.025);

        let max_liquidatable_asset_amount = asset_bank.read().unwrap().calc_amount(
            max_liquidatable_value,
            BalanceSide::Assets,
            RequirementType::Maintenance,
        )?;

        trace!("Account: {}", self.address);
        trace!("Assets: {:?}", assets);
        trace!("Liabilities: {:?}", liabs);
        trace!("Asset Maintenance Weight: {:?}", asset_maint_weight);
        trace!("Liability Maintenance Weight: {:?}", liab_maint_weight);
        trace!("Liquidation Discount: {:?}", liquidation_discount);
        trace!("Maintenance Health: {:?}", maintenence_health);
        trace!("Underwater Maintenance Value: {:?}", underwater_maint_value);
        trace!("Asset Amount: {:?}", asset_amount);
        trace!("Liability Amount: {:?}", liab_amount);
        trace!("Asset Value: {:?}", asset_value);
        trace!("Liability Value: {:?}", liab_value);
        trace!("Max Liquidatable Value: {:?}", max_liquidatable_value);
        trace!("Liquidator Profit: {:?}", liquidator_profit);

        debug!(
            "Max Liquidatable Asset Amount: {:?}, liquidator profit: {:?}",
            max_liquidatable_asset_amount, liquidator_profit
        );

        Ok((max_liquidatable_asset_amount, liquidator_profit))
    }
}
