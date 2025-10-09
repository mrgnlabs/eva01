use crate::cache::Cache;

use super::{bank::BankWrapper, oracle::OracleWrapperTrait};
use anyhow::{Error, Result};
use fixed::types::I80F48;
use log::debug;
use marginfi::state::bank::BankImpl;
use marginfi_type_crate::{
    constants::ASSET_TAG_KAMINO,
    types::{BalanceSide, LendingAccount, MarginfiAccount, OracleSetup},
};
use solana_program::pubkey::Pubkey;
use std::{collections::HashSet, sync::Arc};

#[derive(Clone)]
pub struct MarginfiAccountWrapper {
    pub address: Pubkey,
    pub liquidation_record: Pubkey,
    pub lending_account: LendingAccount,
}

type Shares = Vec<(I80F48, Pubkey)>;

impl MarginfiAccountWrapper {
    pub fn new(address: Pubkey, account: &MarginfiAccount) -> Self {
        MarginfiAccountWrapper {
            address,
            liquidation_record: account.liquidation_record,
            lending_account: account.lending_account,
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

    pub fn get_assets(&self, banks_to_exclude: &[Pubkey]) -> Vec<Pubkey> {
        self.lending_account
            .balances
            .iter()
            .filter_map(|b| {
                if b.is_active()
                    && matches!(b.get_side(), Some(BalanceSide::Assets))
                    && !banks_to_exclude.contains(&b.bank_pk)
                {
                    Some(b.bank_pk)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
    }

    pub fn get_balance_for_bank(&self, bank: &BankWrapper) -> Option<(I80F48, BalanceSide)> {
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

    pub fn contains_kamino_position(
        lending_account: &LendingAccount,
        cache: Arc<Cache>,
    ) -> Result<bool> {
        for balance in &lending_account.balances {
            if !balance.is_active() {
                continue;
            }
            let bank = cache.banks.try_get_bank(&balance.bank_pk)?;
            if bank.bank.config.asset_tag == ASSET_TAG_KAMINO {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn get_observation_accounts<T: OracleWrapperTrait>(
        lending_account: &LendingAccount,
        include_banks: &[Pubkey],
        exclude_banks: &[Pubkey],
        cache: Arc<Cache>,
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
        let mut observation_accounts: Vec<Pubkey> = vec![];
        for bank_pk in bank_pks.iter() {
            let bank_wrapper = cache.banks.try_get_bank(bank_pk)?;
            let oracle_wrapper = T::build(&cache, bank_pk)?;
            debug!(
                "Observation account Bank: {:?}, asset tag type: {:?}.",
                bank_pk, bank_wrapper.bank.config.asset_tag
            );
            let bank_and_oracles: Vec<Pubkey> = match bank_wrapper.bank.config.oracle_setup {
                OracleSetup::PythPushOracle => {
                    vec![*bank_pk, oracle_wrapper.get_address()]
                }
                OracleSetup::SwitchboardPull => {
                    swb_oracles.push(oracle_wrapper.get_address());
                    vec![*bank_pk, oracle_wrapper.get_address()]
                }
                OracleSetup::StakedWithPythPush => {
                    vec![
                        *bank_pk,
                        oracle_wrapper.get_address(),
                        bank_wrapper.bank.config.oracle_keys[1],
                        bank_wrapper.bank.config.oracle_keys[2],
                    ]
                }
                OracleSetup::KaminoPythPush => {
                    vec![
                        *bank_pk,
                        oracle_wrapper.get_address(),
                        bank_wrapper.bank.config.oracle_keys[1],
                    ]
                }
                OracleSetup::KaminoSwitchboardPull => {
                    swb_oracles.push(oracle_wrapper.get_address());
                    vec![
                        *bank_pk,
                        oracle_wrapper.get_address(),
                        bank_wrapper.bank.config.oracle_keys[1],
                    ]
                }
                _ => {
                    return Err(Error::msg("Unsupported Oracle setup"));
                }
            };
            observation_accounts.extend(bank_and_oracles);
        }

        let res = (observation_accounts, swb_oracles);

        Ok(res)
    }
}

#[cfg(test)]
pub mod test_utils {
    use std::array;

    use marginfi_type_crate::{
        constants::ASSET_TAG_DEFAULT,
        types::{Balance, WrappedI80F48},
    };

    use crate::wrappers::bank::test_utils::{test_sol, test_usdc};

    use super::*;

    impl MarginfiAccountWrapper {
        pub fn test_healthy(asset_bank: &BankWrapper, liability_bank: &BankWrapper) -> Self {
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
                liquidation_record: Pubkey::default(),
                lending_account,
            }
        }

        pub fn test_unhealthy() -> Self {
            let asset_bank = test_usdc();
            let liability_bank = test_sol();
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
                liquidation_record: Pubkey::default(),
                lending_account,
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use crate::wrappers::{
        bank::test_utils::{test_bonk, test_sol, test_usdc},
        oracle::test_utils::TestOracleWrapper,
    };

    use super::*;

    use crate::cache::test_utils::create_test_cache;

    #[test]
    fn test_marginfi_account() {
        let sol_bank = test_sol();
        let usdc_bank = test_usdc();

        let mut cache = create_test_cache(&Vec::new());
        cache.banks.insert(sol_bank.address, sol_bank.bank);
        cache.banks.insert(usdc_bank.address, usdc_bank.bank);

        let healthy = MarginfiAccountWrapper::test_healthy(&sol_bank, &usdc_bank);
        assert!(healthy.has_liabs());
        assert_eq!(
            healthy.get_liabilities_shares(),
            vec![(I80F48::from_num(100), usdc_bank.address)]
        );
        assert_eq!(healthy.get_assets(&[]), vec![sol_bank.address]);
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
        assert_eq!(unhealthy.get_assets(&[]), vec![usdc_bank.address]);
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
        let sol_bank = test_sol();
        let usdc_bank = test_usdc();
        let healthy_wrapper = MarginfiAccountWrapper::test_healthy(&sol_bank, &usdc_bank);

        let cache = create_test_cache(&vec![sol_bank.clone(), usdc_bank.clone()]);
        let cache = Arc::new(cache);

        assert_eq!(
            MarginfiAccountWrapper::get_observation_accounts::<TestOracleWrapper>(
                &healthy_wrapper.lending_account,
                &[],
                &[],
                cache.clone()
            )
            .unwrap(),
            (
                vec![
                    sol_bank.address,
                    sol_bank.bank.config.oracle_keys[0],
                    usdc_bank.address,
                    usdc_bank.bank.config.oracle_keys[0],
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
            MarginfiAccountWrapper::get_observation_accounts::<TestOracleWrapper>(
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
        let sol_bank_wrapper = test_sol();
        let usdc_bank_wrapper = test_usdc();
        let bonk_bank_wrapper = test_bonk();
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
            MarginfiAccountWrapper::get_observation_accounts::<TestOracleWrapper>(
                &healthy_wrapper.lending_account,
                &banks_to_include,
                &banks_to_exclude,
                cache
            )
            .unwrap(),
            (
                vec![
                    bonk_bank_wrapper.address,
                    bonk_bank_wrapper.bank.config.oracle_keys[0],
                    sol_bank_wrapper.address,
                    sol_bank_wrapper.bank.config.oracle_keys[0],
                    usdc_bank_wrapper.address,
                    usdc_bank_wrapper.bank.config.oracle_keys[0],
                ],
                vec![bonk_bank_wrapper.bank.config.oracle_keys[0]] // Bonk oracle is the only switchboard oracle
            )
        );
    }

    #[test]
    fn test_get_observation_accounts_with_banks_to_exclude_and_gaps() {
        let sol_bank_wrapper = test_sol();
        let usdc_bank_wrapper = test_usdc();
        let bonk_bank_wrapper = test_bonk();
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
            MarginfiAccountWrapper::get_observation_accounts::<TestOracleWrapper>(
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
                    bonk_bank_wrapper.bank.config.oracle_keys[0],
                    usdc_bank_wrapper.address,
                    usdc_bank_wrapper.bank.config.oracle_keys[0],
                ],
                vec![bonk_bank_wrapper.bank.config.oracle_keys[0]] // Bonk oracle is the only switchboard oracle
            )
        );
    }
}
