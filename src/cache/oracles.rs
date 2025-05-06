use std::{collections::HashMap, sync::RwLock};

use marginfi::state::price::OraclePriceFeedAdapter;
use solana_sdk::{account::Account, pubkey::Pubkey};

use crate::wrappers::oracle::OracleWrapperTrait;
use anyhow::{anyhow, Result};

pub struct OraclesCache<T: OracleWrapperTrait + Clone> {
    accounts: HashMap<Pubkey, Account>,
    account_wrappers: RwLock<HashMap<Pubkey, T>>,
    oracle_to_bank: HashMap<Pubkey, Pubkey>,
    bank_to_oracle: HashMap<Pubkey, Pubkey>,
}

impl<T: OracleWrapperTrait + Clone> OraclesCache<T> {
    pub fn new() -> Self {
        Self {
            accounts: HashMap::new(),
            account_wrappers: RwLock::new(HashMap::new()),
            oracle_to_bank: HashMap::new(),
            bank_to_oracle: HashMap::new(),
        }
    }

    pub fn try_insert(
        &mut self,
        account: Account,
        account_wrapper: T,
        bank_address: Pubkey,
    ) -> Result<()> {
        let oracle_address = account_wrapper.get_address();
        self.accounts.insert(oracle_address, account);
        self.account_wrappers
            .write()
            .map_err(|e| anyhow!("Failed to lock the account_wrappers map for insert! {}", e))?
            .insert(oracle_address, account_wrapper);
        self.oracle_to_bank.insert(oracle_address, bank_address);
        self.bank_to_oracle.insert(bank_address, oracle_address);

        Ok(())
    }

    pub fn try_update_account_wrapper(
        &self,
        address: &Pubkey,
        price_adapter: OraclePriceFeedAdapter,
    ) -> Result<()> {
        let mut wrapper: T = {
            self.account_wrappers
                .read()
                .map_err(|e| anyhow!("Failed to lock the account_wrappers map for search! {}", e))?
                .get(address)
                .ok_or(anyhow::anyhow!(
                    "Failed ot find the Oracle Wrapper for the Address {} in Cache!",
                    address
                ))?
                .clone()
        };

        wrapper.set_price_adapter(price_adapter);
        self.account_wrappers
            .write()
            .map_err(|e| anyhow!("Failed to lock the account_wrappers map for update! {}", e))?
            .insert(*address, wrapper);

        Ok(())
    }

    pub fn get_accounts(&self, addresses: &[Pubkey]) -> Vec<(Pubkey, Account)> {
        addresses
            .iter()
            .filter_map(|address| {
                self.accounts
                    .get(address)
                    .map(|account| (*address, account.clone()))
            })
            .collect()
    }

    pub fn get_addresses(&self) -> Vec<Pubkey> {
        if let Ok(wrappers) = self.account_wrappers.read().inspect_err(|e| {
            eprintln!(
                "Failed to lock the account_wrappers map for collecting addresses! {}",
                e
            )
        }) {
            wrappers.keys().cloned().collect()
        } else {
            vec![]
        }
    }

    pub fn get_wrapper_from_bank(&self, bank_address: &Pubkey) -> Option<T> {
        let oracle = self.bank_to_oracle.get(bank_address)?;
        let wrapper = self
            .account_wrappers
            .read()
            .inspect_err(|e| {
                eprintln!(
                    "Failed to lock the account_wrappers map searching address! {}",
                    e
                )
            })
            .ok()?
            .get(oracle)
            .cloned()?;
        Some(wrapper.clone())
    }
    pub fn try_get_wrapper_from_bank(&self, bank_pk: &Pubkey) -> Result<T> {
        let oracle_pk = self.bank_to_oracle.get(bank_pk).ok_or(anyhow::anyhow!(
            "Failed ot find the Oracle Pubkey for the Bank {} in Cache!",
            bank_pk
        ))?;
        let oracle = self
            .account_wrappers
            .read()
            .map_err(|e| {
                anyhow!(
                    "Failed to lock the account_wrappers map for search address by bank! {}",
                    e
                )
            })?
            .get(oracle_pk)
            .ok_or(anyhow::anyhow!(
                "Failed ot find the Oracle for the Pubkey {} in Cache!",
                oracle_pk
            ))?
            .clone();
        Ok(oracle)
    }

    pub fn try_get_bank_from_oracle(&self, oracle_address: &Pubkey) -> Result<Pubkey> {
        let bank_address = self
            .oracle_to_bank
            .get(oracle_address)
            .ok_or(anyhow::anyhow!(
                "Failed ot find the Bank address for the Oracle {} in Cache!",
                oracle_address
            ))?;
        Ok(*bank_address)
    }
}
#[cfg(test)]
mod tests {
    use crate::wrappers::oracle::test_utils::TestOracleWrapper;

    use super::*;

    #[test]
    fn test_try_insert() {
        let mut cache = OraclesCache::<TestOracleWrapper>::new();
        let oracle_account = Account::default();
        let oracle_wrapper = TestOracleWrapper::test_sol();
        let oracle_address = oracle_wrapper.address;
        let bank_address = Pubkey::new_unique();

        assert!(cache
            .try_insert(oracle_account.clone(), oracle_wrapper.clone(), bank_address)
            .is_ok());
        assert_eq!(cache.accounts.get(&oracle_address), Some(&oracle_account));
        assert!(cache
            .account_wrappers
            .read()
            .unwrap()
            .contains_key(&oracle_address));
        assert_eq!(
            cache.oracle_to_bank.get(&oracle_address),
            Some(&bank_address)
        );
        assert_eq!(
            cache.bank_to_oracle.get(&bank_address),
            Some(&oracle_address)
        );
    }

    #[test]
    fn test_get_accounts() {
        let mut cache = OraclesCache::<TestOracleWrapper>::new();
        let address1 = Pubkey::new_unique();
        let account1 = Account::default();
        cache.accounts.insert(address1, account1.clone());
        let address2 = Pubkey::new_unique();
        let account2 = Account::default();
        cache.accounts.insert(address2, account2.clone());

        let accounts = cache.get_accounts(&[address1, address2]);
        assert_eq!(accounts.len(), 2);
        assert!(accounts.contains(&(address1, account1)));
        assert!(accounts.contains(&(address2, account2)));
    }

    #[test]
    fn test_get_addresses() {
        let cache = OraclesCache::<TestOracleWrapper>::new();
        let wrapper1 = TestOracleWrapper::test_sol();
        let address1 = wrapper1.address;
        let wrapper2 = TestOracleWrapper::test_usdc();
        let address2 = wrapper2.address;

        cache
            .account_wrappers
            .write()
            .unwrap()
            .insert(address1, wrapper1);
        cache
            .account_wrappers
            .write()
            .unwrap()
            .insert(address2, wrapper2);

        let addresses = cache.get_addresses();
        assert_eq!(addresses.len(), 2);
        assert!(addresses.contains(&address1));
        assert!(addresses.contains(&address2));
    }

    #[test]
    fn test_get_wrapper_from_bank() {
        let mut cache = OraclesCache::<TestOracleWrapper>::new();
        let bank_address = Pubkey::new_unique();
        let oracle_address = Pubkey::new_unique();
        let mut wrapper = TestOracleWrapper::test_sol();
        wrapper.address = oracle_address.clone();

        cache.bank_to_oracle.insert(bank_address, oracle_address);
        cache
            .account_wrappers
            .write()
            .unwrap()
            .insert(oracle_address, wrapper.clone());

        let retrieved_wrapper = cache.get_wrapper_from_bank(&bank_address);
        assert!(retrieved_wrapper.is_some());
        assert_eq!(retrieved_wrapper.unwrap().get_address(), oracle_address);
    }

    #[test]
    fn test_try_get_bank_from_oracle() {
        let mut cache = OraclesCache::<TestOracleWrapper>::new();
        let bank_address = Pubkey::new_unique();
        let oracle_address = Pubkey::new_unique();

        cache.oracle_to_bank.insert(oracle_address, bank_address);

        let retrieved_bank = cache.try_get_bank_from_oracle(&oracle_address).unwrap();
        assert_eq!(retrieved_bank, bank_address);
    }
}
