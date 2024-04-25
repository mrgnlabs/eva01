use std::{
    error::Error,
    sync::{Arc, RwLock},
};

use dashmap::DashMap;
use fixed::types::I80F48;
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

    pub fn get_liabilites(&self) -> Result<Vec<(I80F48, Pubkey)>, Box<dyn Error>> {
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

    pub fn get_deposits(
        &self,
        mints_to_exclude: &[Pubkey],
    ) -> Result<Vec<(I80F48, Pubkey)>, Box<dyn Error>> {
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
        let mut ordered_active_banks = self
            .account
            .lending_account
            .balances
            .iter()
            .filter(|b| b.active && !banks_to_exclude.contains(&b.bank_pk))
            .map(|b| b.bank_pk)
            .collect::<Vec<_>>();

        ordered_active_banks.extend(banks_to_include.iter());

        let bank_accounts_and_oracles = ordered_active_banks
            .iter()
            .filter(|b| banks_to_include.contains(b))
            .map(|b| {
                let bank_ref = self.banks.get(b).expect("Bank not found");
                let bank_wrapper = bank_ref.value().read().expect("RwLock error");

                vec![
                    bank_wrapper.bank.liquidity_vault,
                    bank_wrapper.bank.config.oracle_keys[0],
                ]
            })
            .flatten()
            .collect::<Vec<Pubkey>>();

        bank_accounts_and_oracles
    }
}
