use std::{collections::HashMap, sync::RwLock};

use log::error;
use marginfi::state::price::OraclePriceFeedAdapter;
use solana_sdk::{account::Account, pubkey::Pubkey};

use crate::wrappers::oracle::OracleWrapperTrait;
use anyhow::{anyhow, bail, Result};

pub struct OraclesCache<T: OracleWrapperTrait + Clone> {
    accounts: HashMap<Pubkey, Account>,
    oracle_to_banks: HashMap<Pubkey, Vec<Pubkey>>,
    bank_to_oracle: HashMap<Pubkey, Pubkey>,
    oracle_wrappers: RwLock<HashMap<(Pubkey, Pubkey), T>>,
}

impl<T: OracleWrapperTrait + Clone> OraclesCache<T> {
    pub fn new() -> Self {
        Self {
            accounts: HashMap::new(),
            oracle_to_banks: HashMap::new(),
            bank_to_oracle: HashMap::new(),
            oracle_wrappers: RwLock::new(HashMap::new()),
        }
    }

    pub fn try_insert(
        &mut self,
        address: Pubkey,
        account: Account,
        oracle_wrapper: Option<T>,
        bank_address: Pubkey,
    ) -> Result<()> {
        self.accounts.insert(address, account);
        self.oracle_to_banks
            .entry(address)
            .and_modify(|banks| banks.push(bank_address))
            .or_insert_with(|| vec![bank_address]);
        self.bank_to_oracle.insert(bank_address, address);
        if let Some(wrapper) = oracle_wrapper {
            self.oracle_wrappers
                .write()
                .map_err(|e| anyhow!("Failed to lock the oracle_wrappers map for insert! {}", e))?
                .insert((address, bank_address), wrapper);
        }

        Ok(())
    }

    pub fn try_get_account(&self, address: &Pubkey) -> Result<Account> {
        self.accounts
            .get(address)
            .ok_or(anyhow!(
                "Failed to find Oracle account with the Address {}!",
                &address
            ))
            .cloned()
    }

    pub fn exists(&self, address: &Pubkey) -> bool {
        self.try_get_account(address).is_ok()
    }

    pub fn try_wire_with_bank(
        &mut self,
        oracle_address: &Pubkey,
        bank_address: &Pubkey,
    ) -> Result<()> {
        if !self.exists(oracle_address) {
            bail!(
                "Unknown Oracle address {}! Cannot wire it with the Bank {}",
                oracle_address,
                bank_address
            );
        };
        self.oracle_to_banks
            .entry(*oracle_address)
            .and_modify(|banks| banks.push(*bank_address))
            .or_insert_with(|| vec![*bank_address]);

        self.bank_to_oracle.insert(*bank_address, *oracle_address);

        Ok(())
    }

    pub fn try_update_account_wrapper(
        &self,
        address: &Pubkey,
        bank_address: &Pubkey,
        price_adapter: OraclePriceFeedAdapter,
    ) -> Result<()> {
        let wrapper = T::new(*address, price_adapter);
        self.oracle_wrappers
            .write()
            .map_err(|e| anyhow!("Failed to lock the oracle_wrappers map for update! {}", e))?
            .insert((*address, *bank_address), wrapper);

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
        self.accounts.keys().cloned().collect()
    }

    pub fn try_get_wrappers(&self) -> Result<Vec<T>> {
        Ok(self
            .oracle_wrappers
            .read()
            .map_err(|e| anyhow!("Failed to lock the oracle_wrappers for copying! {}", e))?
            .values()
            .cloned()
            .collect())
    }

    pub fn try_get_wrapper(&self, address: &Pubkey, bank_address: &Pubkey) -> Result<T> {
        self.oracle_wrappers
            .read()
            .map_err(|e| {
                anyhow!(
                    "Failed to lock the oracle_wrappers map for searching! {}",
                    e
                )
            })?
            .get(&(*address, *bank_address))
            .cloned()
            .ok_or(anyhow!(
                "Failed to find Oracle wrapper for Oracle: {}, Bank: {}!",
                address,
                bank_address
            ))
    }

    pub fn try_get_wrapper_from_bank(&self, bank_address: &Pubkey) -> Result<T> {
        let oracle = self.bank_to_oracle.get(bank_address).ok_or(anyhow!(
            "Failed to find Oracle for the Bank {}!",
            bank_address
        ))?;

        self.try_get_wrapper(oracle, bank_address)
    }

    pub fn get_wrapper_from_bank(&self, bank_address: &Pubkey) -> Option<T> {
        self.try_get_wrapper_from_bank(bank_address)
            .map_err(|err| error!("{}", err))
            .ok()
    }

    pub fn try_get_banks_from_oracle(&self, oracle_address: &Pubkey) -> Result<Vec<Pubkey>> {
        self.oracle_to_banks
            .get(oracle_address)
            .cloned()
            .ok_or(anyhow::anyhow!(
                "Failed to find any Bank address for the Oracle {}!",
                oracle_address
            ))
    }

    pub fn len(&self) -> usize {
        self.accounts.len()
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
            .try_insert(
                oracle_address,
                oracle_account.clone(),
                Some(oracle_wrapper.clone()),
                bank_address
            )
            .is_ok());
        assert_eq!(cache.accounts.get(&oracle_address), Some(&oracle_account));
        assert!(cache
            .oracle_wrappers
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
        let mut cache = OraclesCache::<TestOracleWrapper>::new();
        let address1 = Pubkey::new_unique();
        let address2 = Pubkey::new_unique();

        cache.accounts.insert(address1, Account::default());
        cache.accounts.insert(address2, Account::default());

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
            .oracle_wrappers
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
    #[test]
    fn test_exists() {
        let mut cache = OraclesCache::<TestOracleWrapper>::new();
        let oracle_address = Pubkey::new_unique();
        let account = Account::default();

        assert!(!cache.exists(&oracle_address));
        cache.accounts.insert(oracle_address, account);
        assert!(cache.exists(&oracle_address));
    }

    #[test]
    fn test_try_wire_with_bank() {
        let mut cache = OraclesCache::<TestOracleWrapper>::new();
        let oracle_address = Pubkey::new_unique();
        let bank_address = Pubkey::new_unique();
        cache.accounts.insert(oracle_address, Account::default());

        assert!(cache
            .try_wire_with_bank(&oracle_address, &bank_address)
            .is_ok());
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
    fn test_try_get_wrappers() {
        let mut cache = OraclesCache::<TestOracleWrapper>::new();
        let account = Account::default();
        let oracle_wrapper = TestOracleWrapper::test_sol();
        let oracle_address = oracle_wrapper.get_address();

        cache.accounts.insert(oracle_address, account);
        cache
            .oracle_wrappers
            .write()
            .unwrap()
            .insert(oracle_address, oracle_wrapper);

        let wrappers = cache.try_get_wrappers().unwrap();
        assert_eq!(wrappers.len(), 1);
        assert_eq!(wrappers[0].get_address(), oracle_address);
    }
}
