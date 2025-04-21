use crate::thread_debug;

use super::{bank::BankWrapperT, oracle::OracleWrapperTrait};
use fixed::types::I80F48;
use log::debug;
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

    pub fn get_deposits<T: OracleWrapperTrait>(
        &self,
        mints_to_exclude: &[Pubkey],
        banks: &HashMap<Pubkey, BankWrapperT<T>>,
    ) -> Vec<Pubkey> {
        self.lending_account
            .balances
            .iter()
            .filter_map(|b| {
                if b.is_active()
                    && matches!(b.get_side(), Some(BalanceSide::Assets))
                    && !mints_to_exclude.contains(&banks.get(&b.bank_pk).unwrap().bank.mint)
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

    // TODO: Create Unit test
    pub fn get_active_banks(lending_account: &LendingAccount) -> Vec<Pubkey> {
        lending_account
            .balances
            .iter()
            .filter_map(|balance| balance.is_active().then_some(balance.bank_pk))
            .collect::<Vec<_>>()
    }

    // TODO: add more unit tests
    pub fn get_observation_accounts<T: OracleWrapperTrait>(
        account_wrapper: &MarginfiAccountWrapper,
        include_banks: &[Pubkey],
        exclude_banks: &[Pubkey],
        banks: &HashMap<Pubkey, BankWrapperT<T>>,
    ) -> Vec<Pubkey> {
        thread_debug!("Collecting observation accounts for the account: {:?} with include_banks {:?} and exclude_banks {:?}", 
            account_wrapper.address, include_banks, exclude_banks);
        // This is a temporary fix to ensure the proper order of the remaining accounts.
        // It will NOT be necessary once this PR is deployed: https://github.com/mrgnlabs/marginfi-v2/pull/320
        let lending_account = &account_wrapper.lending_account;
        let active_bank_pks = MarginfiAccountWrapper::get_active_banks(lending_account);

        let mut bank_pks: Vec<Pubkey> = vec![];
        let mut bank_to_include_index = 0;

        for balance in lending_account.balances.iter() {
            if balance.is_active() {
                bank_pks.push(balance.bank_pk);
            } else if include_banks.len() > bank_to_include_index {
                for bank_pk in &include_banks[bank_to_include_index..] {
                    bank_to_include_index += 1;
                    if !active_bank_pks.contains(bank_pk) {
                        bank_pks.push(*bank_pk);
                        break;
                    }
                }
            }
        }
        // The end of the complex logic-to-be-removed

        bank_pks.retain(|bank_pk| !exclude_banks.contains(bank_pk));

        // Add bank oracles
        bank_pks
            .iter()
            .flat_map(|bank_pk| {
                let bank = banks.get(bank_pk).unwrap();
                debug!(
                    "Observation account Bank: {:?}, asset tag type: {:?}.",
                    bank.address, bank.bank.config.asset_tag
                );

                if bank.bank.config.oracle_keys[1] != Pubkey::default()
                    && bank.bank.config.oracle_keys[2] != Pubkey::default()
                {
                    debug!(
                        "Observation accounts for the bank {:?} will contain Oracle keys!",
                        bank.address
                    );
                    vec![
                        bank.address,
                        bank.oracle_adapter.get_address(),
                        bank.bank.config.oracle_keys[1],
                        bank.bank.config.oracle_keys[2],
                    ]
                } else {
                    vec![bank.address, bank.oracle_adapter.get_address()]
                }
            })
            .collect::<Vec<_>>()
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
            asset_bank: BankWrapperT<TestOracleWrapper>,
            liability_bank: BankWrapperT<TestOracleWrapper>,
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

    #[test]
    fn test_marginfi_account() {
        let sol_bank = TestBankWrapper::test_sol();
        let usdc_bank = TestBankWrapper::test_usdc();
        let mut banks = HashMap::new();
        banks.insert(sol_bank.address, sol_bank.clone());
        banks.insert(usdc_bank.address, usdc_bank.clone());

        let healthy = MarginfiAccountWrapper::test_healthy(sol_bank.clone(), usdc_bank.clone());
        assert!(healthy.has_liabs());
        assert_eq!(
            healthy.get_liabilities_shares(),
            vec![(I80F48::from_num(100), usdc_bank.address)]
        );
        assert_eq!(healthy.get_deposits(&[], &banks), vec![(sol_bank.address)]);
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

        let unhealthy = MarginfiAccountWrapper::test_unhealthy();
        assert!(unhealthy.has_liabs());
        assert_eq!(
            unhealthy.get_liabilities_shares(),
            vec![(I80F48::from_num(100), sol_bank.address)]
        );
        assert_eq!(
            unhealthy.get_deposits(&[], &banks),
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
    }

    #[test]
    fn test_get_healthy_observation_accounts() {
        let sol_bank = TestBankWrapper::test_sol();
        let usdc_bank = TestBankWrapper::test_usdc();
        let healthy_wrapper =
            MarginfiAccountWrapper::test_healthy(sol_bank.clone(), usdc_bank.clone());
        let mut banks = HashMap::new();
        banks.insert(sol_bank.address, sol_bank.clone());
        banks.insert(usdc_bank.address, usdc_bank.clone());
        assert_eq!(
            MarginfiAccountWrapper::get_observation_accounts(&healthy_wrapper, &[], &[], &banks),
            vec![
                sol_bank.address,
                sol_bank.oracle_adapter.get_address(),
                usdc_bank.address,
                usdc_bank.oracle_adapter.get_address()
            ]
        );
    }

    #[test]
    #[ignore]
    fn test_get_unhealthy_observation_accounts() {
        let unhealthy_wrapper = MarginfiAccountWrapper::test_unhealthy();
        let sol_bank = TestBankWrapper::test_sol();
        let usdc_bank = TestBankWrapper::test_usdc();
        let mut banks = HashMap::new();
        banks.insert(sol_bank.address.clone(), sol_bank.clone());
        banks.insert(usdc_bank.address.clone(), usdc_bank.clone());
        //        let banks_to_exclude = vec![sol_bank.address.clone(), usdc_bank.address.clone()];
        assert_eq!(
            MarginfiAccountWrapper::get_observation_accounts(&unhealthy_wrapper, &[], &[], &banks),
            vec![]
        );
    }

    #[test]
    fn test_get_observation_accounts_with_banks_to_include_banks() {
        let sol_bank = TestBankWrapper::test_sol();
        let usdc_bank = TestBankWrapper::test_usdc();
        let bonk_bank = TestBankWrapper::test_bonk();
        let healthy_wrapper =
            MarginfiAccountWrapper::test_healthy(sol_bank.clone(), usdc_bank.clone());
        let mut banks = HashMap::new();
        banks.insert(bonk_bank.address, bonk_bank.clone());
        banks.insert(sol_bank.address, sol_bank.clone());
        banks.insert(usdc_bank.address, usdc_bank.clone());
        let banks_to_include = vec![bonk_bank.address];
        let banks_to_exclude = vec![];
        assert_eq!(
            MarginfiAccountWrapper::get_observation_accounts(
                &healthy_wrapper,
                &banks_to_include,
                &banks_to_exclude,
                &banks
            ),
            vec![
                sol_bank.address,
                sol_bank.oracle_adapter.get_address(),
                usdc_bank.address,
                usdc_bank.oracle_adapter.get_address(),
                bonk_bank.address,
                bonk_bank.oracle_adapter.get_address()
            ]
        );
    }
}
