use super::bank::BankWrapper;
use fixed::types::I80F48;
use marginfi::state::marginfi_account::{BalanceSide, MarginfiAccount};
use solana_program::pubkey::Pubkey;
use std::collections::HashMap;

#[derive(Clone)]
pub struct TxConfig {
    pub compute_unit_price_micro_lamports: Option<u64>,
}

#[derive(Clone)]
pub struct MarginfiAccountWrapper {
    pub address: Pubkey,
    pub account: MarginfiAccount,
}

impl MarginfiAccountWrapper {
    pub fn new(address: Pubkey, account: MarginfiAccount) -> Self {
        MarginfiAccountWrapper { address, account }
    }

    pub fn has_liabs(&self) -> bool {
        self.account
            .lending_account
            .balances
            .iter()
            .any(|a| a.active && matches!(a.get_side(), Some(BalanceSide::Liabilities)))
    }

    pub fn get_liabilities_shares(&self) -> Vec<(I80F48, Pubkey)> {
        self.account
            .lending_account
            .balances
            .iter()
            .filter(|b| matches!(b.get_side(), Some(BalanceSide::Liabilities)) && b.active)
            .map(|b| (b.liability_shares.into(), b.bank_pk))
            .collect::<Vec<_>>()
    }

    pub fn get_deposits(
        &self,
        mints_to_exclude: &[Pubkey],
        banks: &HashMap<Pubkey, BankWrapper>,
    ) -> anyhow::Result<Vec<(I80F48, Pubkey)>> {
        let mut deposits = vec![];

        for deposit_balance in self
            .account
            .lending_account
            .balances
            .iter()
            .filter(|b| matches!(b.get_side(), Some(BalanceSide::Assets)))
        {
            let bank = banks.get(&deposit_balance.bank_pk).unwrap();

            if mints_to_exclude.contains(&bank.bank.mint) {
                continue;
            }

            deposits.push((
                bank.bank
                    .get_asset_amount(deposit_balance.asset_shares.into())?,
                deposit_balance.bank_pk,
            ));
        }

        Ok(deposits)
    }

    pub fn get_balance_for_bank(
        &self,
        bank_pk: &Pubkey,
        bank: &BankWrapper,
    ) -> anyhow::Result<Option<(I80F48, BalanceSide)>> {
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
            .unwrap_or(None);

        Ok(balance)
    }

    pub fn get_deposits_shares(&self) -> Vec<(I80F48, Pubkey)> {
        self.account
            .lending_account
            .balances
            .iter()
            .filter(|b| matches!(b.get_side(), Some(BalanceSide::Assets)) & b.active)
            .map(|b| (b.asset_shares.into(), b.bank_pk))
            .collect::<Vec<_>>()
    }

    pub fn get_deposits_and_liabilities_shares(
        &self,
    ) -> (Vec<(I80F48, Pubkey)>, Vec<(I80F48, Pubkey)>) {
        let mut liabilities = Vec::new();
        let mut deposits = Vec::new();

        for balance in &self.account.lending_account.balances {
            if balance.active {
                match balance.get_side() {
                    Some(BalanceSide::Liabilities) => {
                        liabilities.push((balance.liability_shares.into(), balance.bank_pk))
                    }
                    Some(BalanceSide::Assets) => {
                        deposits.push((balance.asset_shares.into(), balance.bank_pk))
                    }
                    _ => {}
                }
            }
        }

        (deposits, liabilities)
    }

    pub fn get_active_banks(&self) -> Vec<Pubkey> {
        self.account
            .lending_account
            .balances
            .clone()
            .iter()
            .filter(|b| b.active)
            .map(|b| b.bank_pk)
            .collect::<Vec<_>>()
    }

    pub fn get_observation_accounts(
        &self,
        banks_to_include: &[Pubkey],
        banks_to_exclude: &[Pubkey],
        banks: &HashMap<Pubkey, BankWrapper>,
    ) -> Vec<Pubkey> {
        let mut ordered_active_banks = self
            .account
            .lending_account
            .balances
            .iter()
            .filter(|b| b.active && !banks_to_exclude.contains(&b.bank_pk))
            .map(|b| b.bank_pk)
            .collect::<Vec<_>>();

        for bank_pk in banks_to_include {
            if !ordered_active_banks.contains(bank_pk) {
                ordered_active_banks.push(*bank_pk);
            }
        }

        let bank_accounts_and_oracles = ordered_active_banks
            .iter()
            .flat_map(|b| {
                let bank = banks.get(b).unwrap();

                if bank.bank.config.oracle_keys[1] != Pubkey::default()
                    && bank.bank.config.oracle_keys[2] != Pubkey::default()
                {
                    vec![
                        bank.address,
                        bank.oracle_adapter.address,
                        bank.bank.config.oracle_keys[1],
                        bank.bank.config.oracle_keys[2],
                    ]
                } else {
                    vec![bank.address, bank.oracle_adapter.address]
                }
            })
            .collect::<Vec<_>>();

        bank_accounts_and_oracles
    }
}
