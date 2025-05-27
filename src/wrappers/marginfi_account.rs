use crate::cache::CacheT;

use super::{bank::BankWrapperT, oracle::OracleWrapperTrait};
use anyhow::{Error, Result};
use fixed::types::I80F48;
use log::debug;
use marginfi::state::{
    marginfi_account::{BalanceSide, LendingAccount},
    price::OracleSetup,
};
use solana_program::pubkey::Pubkey;
use std::{collections::HashSet, sync::Arc};

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
            .any(|b| b.is_active() && matches!(b.get_side(), Some(BalanceSide::Liabilities)))
    }

    pub fn get_liabilities_shares(&self) -> Vec<(I80F48, Pubkey)> {
        self.lending_account
            .balances
            .iter()
            .filter(|b| matches!(b.get_side(), Some(BalanceSide::Liabilities)) && b.is_active())
            .map(|b| (b.liability_shares.into(), b.bank_pk))
            .collect::<Vec<_>>()
    }

    pub fn get_deposits<T: OracleWrapperTrait + Clone>(
        &self,
        mints_to_exclude: &[Pubkey],
        cache: Arc<CacheT<T>>,
    ) -> Vec<Pubkey> {
        self.lending_account
            .balances
            .iter()
            .filter_map(|b| {
                if b.is_active()
                    && matches!(b.get_side(), Some(BalanceSide::Assets))
                    && !mints_to_exclude
                        .contains(&cache.banks.get_bank(&b.bank_pk).map(|bank| bank.mint)?)
                {
                    Some(b.bank_pk)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
    }

    pub fn get_balance_for_bank<T: OracleWrapperTrait>(
        &self,
        bank: &BankWrapperT<T>,
    ) -> Option<(I80F48, BalanceSide)> {
        self.lending_account
            .balances
            .iter()
            .find(|b| b.bank_pk == bank.address)
            .and_then(|b| match b.get_side()? {
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
    }

    pub fn get_deposits_and_liabilities_shares(&self) -> (Shares, Shares) {
        let mut liabilities = Vec::new();
        let mut deposits = Vec::new();

        for balance in &self.lending_account.balances {
            if balance.is_active() {
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

    pub fn get_active_banks(lending_account: &LendingAccount) -> Vec<Pubkey> {
        lending_account
            .balances
            .iter()
            .filter_map(|balance| balance.is_active().then_some(balance.bank_pk))
            .collect::<Vec<_>>()
    }

    pub fn get_observation_accounts<T: OracleWrapperTrait + Clone>(
        lending_account: &LendingAccount,
        include_banks: &[Pubkey],
        exclude_banks: &[Pubkey],
        cache: Arc<CacheT<T>>,
    ) -> Result<(Vec<Pubkey>, Vec<Pubkey>)> {
        let mut bank_pks: HashSet<Pubkey> =
            MarginfiAccountWrapper::get_active_banks(lending_account)
                .into_iter()
                .collect();

        bank_pks.extend(include_banks.iter());

        let mut bank_pks = bank_pks
            .into_iter()
            .filter(|bank_pk| !exclude_banks.contains(bank_pk))
            .collect::<Vec<_>>();

        // Sort all bank_pks in descending order
        bank_pks.sort_by(|a, b| b.cmp(a));

        let mut swb_oracles = vec![];
        // Add bank oracles
        let observation_accounts = bank_pks.iter().flat_map(|bank_pk| {
            let bank = cache.banks.try_get_bank(bank_pk)?;
            let bank_oracle_wrapper = cache.oracles.try_get_wrapper_from_bank(bank_pk)?;
            debug!(
                "Observation account Bank: {:?}, asset tag type: {:?}.",
                bank_pk, bank.config.asset_tag
            );
            if matches!(bank.config.oracle_setup, OracleSetup::SwitchboardPull) {
                swb_oracles.push(bank_oracle_wrapper.get_address());
            }

            if bank.config.oracle_keys[1] != Pubkey::default()
                && bank.config.oracle_keys[2] != Pubkey::default()
            {
                debug!(
                    "Observation accounts for the bank {:?} will contain Oracle keys!",
                    bank_pk
                );
                Ok::<Vec<Pubkey>, Error>(vec![
                    *bank_pk,
                    bank_oracle_wrapper.get_address(),
                    bank.config.oracle_keys[1],
                    bank.config.oracle_keys[2],
                ])
            } else {
                Ok(vec![*bank_pk, bank_oracle_wrapper.get_address()])
            }
        });

        Ok((
            observation_accounts.into_iter().flatten().collect(),
            swb_oracles,
        ))
    }
}

#[cfg(test)]
pub mod test_utils {
    use std::array;

    use marginfi::{
        constants::ASSET_TAG_DEFAULT,
        state::{marginfi_account::Balance, marginfi_group::WrappedI80F48},
    };

    use crate::wrappers::{
        bank::test_utils::TestBankWrapper, oracle::test_utils::TestOracleWrapper,
    };

    use super::*;

    impl MarginfiAccountWrapper {
        pub fn test_healthy(
            asset_bank: &BankWrapperT<TestOracleWrapper>,
            liability_bank: &BankWrapperT<TestOracleWrapper>,
        ) -> Self {
            let balances: [Balance; 16] = array::from_fn(|i| match i {
                0 => Balance {
                    active: 1,
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
                    active: 1,
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
                    active: 1,
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
                    active: 1,
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

    use crate::cache::test_utils::create_test_cache;

    #[test]
    fn test_marginfi_account() {
        let sol_bank = TestBankWrapper::test_sol();
        let usdc_bank = TestBankWrapper::test_usdc();

        let mut cache = create_test_cache(&Vec::new());
        cache.banks.insert(sol_bank.address, sol_bank.bank);
        cache.banks.insert(usdc_bank.address, usdc_bank.bank);

        let cache = Arc::new(cache);

        let healthy = MarginfiAccountWrapper::test_healthy(&sol_bank, &usdc_bank);
        assert!(healthy.has_liabs());
        assert_eq!(
            healthy.get_liabilities_shares(),
            vec![(I80F48::from_num(100), usdc_bank.address)]
        );
        assert_eq!(
            healthy.get_deposits(&[], cache.clone()),
            vec![(sol_bank.address)]
        );
        let (balance, side) = healthy.get_balance_for_bank(&sol_bank).unwrap();
        assert_eq!(balance, I80F48::from_num(100));
        match side {
            BalanceSide::Assets => assert!(true, "Side is Assets"),
            BalanceSide::Liabilities => assert!(false, "Side is Liabilities"),
        }
        let (balance, side) = healthy.get_balance_for_bank(&usdc_bank).unwrap();
        assert_eq!(balance, I80F48::from_num(100));
        match side {
            BalanceSide::Assets => assert!(false, "Side is Assets"),
            BalanceSide::Liabilities => assert!(true, "Side is Liabilities"),
        }
        assert_eq!(
            healthy.get_deposits_and_liabilities_shares(),
            (
                vec![(I80F48::from_num(100), sol_bank.address)],
                vec![(I80F48::from_num(100), usdc_bank.address)]
            )
        );
        assert_eq!(
            MarginfiAccountWrapper::get_active_banks(&healthy.lending_account),
            vec![sol_bank.address, usdc_bank.address]
        );

        let mut unhealthy = MarginfiAccountWrapper::test_unhealthy();
        assert!(unhealthy.has_liabs());
        assert_eq!(
            unhealthy.get_liabilities_shares(),
            vec![(I80F48::from_num(100), sol_bank.address)]
        );
        assert_eq!(
            unhealthy.get_deposits(&[], cache),
            vec![(usdc_bank.address)]
        );
        let (balance, side) = unhealthy.get_balance_for_bank(&sol_bank).unwrap();
        assert_eq!(balance, I80F48::from_num(100));
        match side {
            BalanceSide::Assets => assert!(false, "Side is Assets"),
            BalanceSide::Liabilities => assert!(true, "Side is Liabilities"),
        }
        let (balance, side) = unhealthy.get_balance_for_bank(&usdc_bank).unwrap();
        assert_eq!(balance, I80F48::from_num(100));
        match side {
            BalanceSide::Assets => assert!(true, "Side is Assets"),
            BalanceSide::Liabilities => assert!(false, "Side is Liabilities"),
        }
        assert_eq!(
            unhealthy.get_deposits_and_liabilities_shares(),
            (
                vec![(I80F48::from_num(100), usdc_bank.address)],
                vec![(I80F48::from_num(100), sol_bank.address)]
            )
        );
        assert_eq!(
            MarginfiAccountWrapper::get_active_banks(&unhealthy.lending_account),
            vec![usdc_bank.address, sol_bank.address]
        );

        unhealthy.lending_account.balances.swap(1, 2); // swap the elements to create a "gap" at index 1
        unhealthy.lending_account.balances.swap(2, 3); // swap the elements to create a "gap" at index 2 as well

        // Check that the gaps are handled correctly -> get_active_banks returns the same result as before
        assert_eq!(
            MarginfiAccountWrapper::get_active_banks(&unhealthy.lending_account),
            vec![usdc_bank.address, sol_bank.address]
        );

        // Now swap two active banks' positions and verify that the new order is respected
        unhealthy.lending_account.balances.swap(0, 3);
        assert_eq!(
            MarginfiAccountWrapper::get_active_banks(&unhealthy.lending_account),
            vec![sol_bank.address, usdc_bank.address]
        );

        // Finally "turn off" the first active bank and check that only the second one is returned
        unhealthy.lending_account.balances[0].active = 0;
        assert_eq!(
            MarginfiAccountWrapper::get_active_banks(&unhealthy.lending_account),
            vec![usdc_bank.address]
        );
    }

    #[test]
    fn test_get_healthy_observation_accounts() {
        let sol_bank = TestBankWrapper::test_sol();
        let sol_oracle_address = sol_bank.oracle_adapter.get_address();
        let usdc_bank = TestBankWrapper::test_usdc();
        let healthy_wrapper = MarginfiAccountWrapper::test_healthy(&sol_bank, &usdc_bank);

        let cache = create_test_cache(&vec![sol_bank.clone(), usdc_bank.clone()]);
        let cache = Arc::new(cache);

        assert_eq!(
            MarginfiAccountWrapper::get_observation_accounts(
                &healthy_wrapper.lending_account,
                &[],
                &[],
                cache.clone()
            )
            .unwrap(),
            (
                vec![
                    sol_bank.address,
                    sol_oracle_address,
                    usdc_bank.address,
                    usdc_bank.oracle_adapter.address
                ],
                vec![]
            )
        );
    }

    #[test]
    fn test_get_unhealthy_observation_accounts() {
        let unhealthy_wrapper = MarginfiAccountWrapper::test_unhealthy();
        let cache = create_test_cache(&Vec::new());
        let cache = Arc::new(cache);

        assert_eq!(
            MarginfiAccountWrapper::get_observation_accounts(
                &unhealthy_wrapper.lending_account,
                &[],
                &[],
                cache
            )
            .unwrap(),
            (vec![], vec![])
        );
    }

    #[test]
    fn test_get_observation_accounts_with_banks_to_include() {
        let sol_bank_wrapper = TestBankWrapper::test_sol();
        let usdc_bank_wrapper = TestBankWrapper::test_usdc();
        let bonk_bank_wrapper = TestBankWrapper::test_bonk();
        let cache = create_test_cache(&vec![
            sol_bank_wrapper.clone(),
            usdc_bank_wrapper.clone(),
            bonk_bank_wrapper.clone(),
        ]);
        let cache = Arc::new(cache);

        let healthy_wrapper =
            MarginfiAccountWrapper::test_healthy(&sol_bank_wrapper, &usdc_bank_wrapper);

        let banks_to_include = vec![bonk_bank_wrapper.address, sol_bank_wrapper.address];
        let banks_to_exclude = vec![];
        assert_eq!(
            MarginfiAccountWrapper::get_observation_accounts(
                &healthy_wrapper.lending_account,
                &banks_to_include,
                &banks_to_exclude,
                cache
            )
            .unwrap(),
            (
                vec![
                    sol_bank_wrapper.address,
                    sol_bank_wrapper.oracle_adapter.address,
                    usdc_bank_wrapper.address,
                    usdc_bank_wrapper.oracle_adapter.address,
                    bonk_bank_wrapper.address,
                    bonk_bank_wrapper.oracle_adapter.address
                ],
                vec![bonk_bank_wrapper.oracle_adapter.address] // Bonk oracle is the only switchboard oracle
            )
        );
    }

    #[test]
    fn test_get_observation_accounts_with_banks_to_exclude_and_gaps() {
        let sol_bank_wrapper = TestBankWrapper::test_sol();
        let usdc_bank_wrapper = TestBankWrapper::test_usdc();
        let bonk_bank_wrapper = TestBankWrapper::test_bonk();
        let cache = create_test_cache(&vec![
            sol_bank_wrapper.clone(),
            usdc_bank_wrapper.clone(),
            bonk_bank_wrapper.clone(),
        ]);
        let cache = Arc::new(cache);

        let mut healthy_wrapper =
            MarginfiAccountWrapper::test_healthy(&sol_bank_wrapper, &usdc_bank_wrapper);
        healthy_wrapper.lending_account.balances.swap(1, 2); // swap the elements to create a "gap" at index 1

        let banks_to_include = vec![bonk_bank_wrapper.address];
        let banks_to_exclude = vec![sol_bank_wrapper.address];
        assert_eq!(
            MarginfiAccountWrapper::get_observation_accounts(
                &healthy_wrapper.lending_account,
                &banks_to_include,
                &banks_to_exclude,
                cache
            )
            .unwrap(),
            (
                vec![
                    // SOL bank was excluded
                    // sol_bank_wrapper.address,
                    // sol_bank_wrapper.oracle_adapter.address,
                    bonk_bank_wrapper.address, // bonk bank took the place of a "gap"
                    bonk_bank_wrapper.oracle_adapter.address,
                    usdc_bank_wrapper.address,
                    usdc_bank_wrapper.oracle_adapter.address,
                ],
                vec![bonk_bank_wrapper.oracle_adapter.address] // Bonk oracle is the only switchboard oracle
            )
        );
    }
}
