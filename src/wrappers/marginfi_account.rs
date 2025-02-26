use super::{
    bank::{BankWrapper, BankWrapperT},
    oracle::OracleWrapperTrait,
};
use fixed::types::I80F48;
use marginfi::state::marginfi_account::{BalanceSide, LendingAccount};
use solana_program::pubkey::Pubkey;
use std::collections::HashMap;

#[derive(Clone)]
pub struct MarginfiAccountWrapper {
    pub address: Pubkey,
    pub lending_account: LendingAccount,
}

type Shares = Vec<(I80F48, Pubkey)>;

impl MarginfiAccountWrapper {
    pub fn new(address: Pubkey, lending_account: LendingAccount) -> Self {
        MarginfiAccountWrapper {
            address,
            lending_account,
        }
    }

    pub fn has_liabs(&self) -> bool {
        self.lending_account
            .balances
            .iter()
            .any(|a| a.active && matches!(a.get_side(), Some(BalanceSide::Liabilities)))
    }

    pub fn get_liabilities_shares(&self) -> Vec<(I80F48, Pubkey)> {
        self.lending_account
            .balances
            .iter()
            .filter(|b| matches!(b.get_side(), Some(BalanceSide::Liabilities)) && b.active)
            .map(|b| (b.liability_shares.into(), b.bank_pk))
            .collect::<Vec<_>>()
    }

    pub fn get_deposits<T: OracleWrapperTrait>(
        &self,
        mints_to_exclude: &[Pubkey],
        banks: &HashMap<Pubkey, BankWrapperT<T>>,
    ) -> anyhow::Result<Vec<(I80F48, Pubkey)>> {
        let mut deposits = vec![];

        for deposit_balance in self
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

    pub fn get_deposits_and_liabilities_shares(&self) -> (Shares, Shares) {
        let mut liabilities = Vec::new();
        let mut deposits = Vec::new();

        for balance in &self.lending_account.balances {
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
        self.lending_account
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

#[cfg(test)]
pub mod test_utils {
    use std::array;

    use marginfi::{
        constants::ASSET_TAG_DEFAULT,
        state::{marginfi_account::Balance, marginfi_group::WrappedI80F48},
    };

    use crate::wrappers::bank::test_utils::TestBankWrapper;

    use super::*;

    impl MarginfiAccountWrapper {
        pub fn test_healthy() -> Self {
            let asset_bank = TestBankWrapper::test_sol();
            let liability_bank = TestBankWrapper::test_usdc();
            let balances: [Balance; 16] = array::from_fn(|i| match i {
                0 => Balance {
                    active: true,
                    bank_pk: asset_bank.address,
                    bank_asset_tag: ASSET_TAG_DEFAULT,
                    _pad0: [0; 6],
                    asset_shares: WrappedI80F48::from(I80F48::from_num(100)),
                    liability_shares: WrappedI80F48::from(I80F48::ZERO),
                    emissions_outstanding: WrappedI80F48::from(I80F48::ZERO),
                    last_update: 0,
                    _padding: [0; 1],
                },
                1 => Balance {
                    active: true,
                    bank_pk: liability_bank.address,
                    bank_asset_tag: ASSET_TAG_DEFAULT,
                    _pad0: [0; 6],
                    asset_shares: WrappedI80F48::from(I80F48::ZERO),
                    liability_shares: WrappedI80F48::from(I80F48::from_num(100)),
                    emissions_outstanding: WrappedI80F48::from(I80F48::ZERO),
                    last_update: 0,
                    _padding: [0; 1],
                },

                _ => Balance::empty_deactivated(),
            });

            let lending_account = LendingAccount {
                balances,
                _padding: [0u64; 8],
            };
            Self {
                address: Pubkey::new_unique(),
                lending_account,
            }
        }

        pub fn test_unhealthy() -> Self {
            let asset_bank = TestBankWrapper::test_usdc();
            let liability_bank = TestBankWrapper::test_sol();
            let balances: [Balance; 16] = array::from_fn(|i| match i {
                0 => Balance {
                    active: true,
                    bank_pk: asset_bank.address,
                    bank_asset_tag: ASSET_TAG_DEFAULT,
                    _pad0: [0; 6],
                    asset_shares: WrappedI80F48::from(I80F48::from_num(100)),
                    liability_shares: WrappedI80F48::from(I80F48::ZERO),
                    emissions_outstanding: WrappedI80F48::from(I80F48::ZERO),
                    last_update: 0,
                    _padding: [0; 1],
                },
                1 => Balance {
                    active: true,
                    bank_pk: liability_bank.address,
                    bank_asset_tag: ASSET_TAG_DEFAULT,
                    _pad0: [0; 6],
                    asset_shares: WrappedI80F48::from(I80F48::ZERO),
                    liability_shares: WrappedI80F48::from(I80F48::from_num(100)),
                    emissions_outstanding: WrappedI80F48::from(I80F48::ZERO),
                    last_update: 0,
                    _padding: [0; 1],
                },

                _ => Balance::empty_deactivated(),
            });

            let lending_account = LendingAccount {
                balances,
                _padding: [0u64; 8],
            };
            Self {
                address: Pubkey::new_unique(),
                lending_account,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::wrappers::bank::test_utils::TestBankWrapper;

    use super::*;

    #[test]
    fn test_marginfi_account() {
        let sol_bank = TestBankWrapper::test_sol();
        let usdc_bank = TestBankWrapper::test_usdc();
        let mut banks = HashMap::new();
        banks.insert(sol_bank.address, sol_bank.clone());
        banks.insert(usdc_bank.address, usdc_bank.clone());

        let healthy = MarginfiAccountWrapper::test_healthy();
        assert!(healthy.has_liabs());
        assert_eq!(
            healthy.get_liabilities_shares(),
            vec![(I80F48::from_num(100), usdc_bank.address)]
        );
        assert_eq!(
            healthy.get_deposits(&[], &banks).unwrap(),
            vec![(I80F48::from_num(100), sol_bank.address)]
        );

        let unhealthy = MarginfiAccountWrapper::test_unhealthy();
        assert!(unhealthy.has_liabs());
        assert_eq!(
            unhealthy.get_liabilities_shares(),
            vec![(I80F48::from_num(100), sol_bank.address)]
        );
        assert_eq!(
            unhealthy.get_deposits(&[], &banks).unwrap(),
            vec![(I80F48::from_num(100), usdc_bank.address)]
        );
    }
}
