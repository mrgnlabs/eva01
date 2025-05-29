use anyhow::{anyhow, Result};
use solana_sdk::{account::Account, pubkey::Pubkey};
use std::{collections::HashMap, sync::RwLock};

pub struct OraclesCache {
    accounts: RwLock<HashMap<Pubkey, Account>>,
}

impl OraclesCache {
    pub fn new() -> Self {
        Self {
            accounts: RwLock::new(HashMap::new()),
        }
    }

    pub fn try_insert(&mut self, address: Pubkey, account: Account) -> Result<()> {
        self.accounts
            .write()
            .map_err(|e| anyhow!("Failed to lock the Oracle accounts map for insert! {}", e))?
            .insert(address, account);

        Ok(())
    }

    pub fn try_update(&self, address: &Pubkey, account: Account) -> Result<()> {
        self.accounts
            .write()
            .map_err(|e| {
                anyhow!(
                    "Failed to lock the Oracle accounts map for for update! {}",
                    e
                )
            })?
            .insert(*address, account);
        Ok(())
    }

    pub fn try_get_account(&self, address: &Pubkey) -> Result<Account> {
        self.accounts
            .read()
            .map_err(|e| {
                anyhow!(
                    "Failed to lock the Oracle accounts map for for search! {}",
                    e
                )
            })?
            .get(address)
            .ok_or(anyhow!(
                "Failed to find Oracle account with the Address {}!",
                &address
            ))
            .cloned()
    }

    pub fn get_accounts(&self, addresses: &[Pubkey]) -> Result<Vec<(Pubkey, Account)>> {
        let accounts_guard = self.accounts.read().map_err(|e| {
            anyhow!(
                "Failed to lock the Oracle accounts map for returning specific accounts! {}",
                e
            )
        })?;

        Ok(addresses
            .iter()
            .filter_map(|address| {
                accounts_guard
                    .get(address)
                    .map(|account| (*address, account.clone()))
            })
            .collect())
    }

    pub fn get_addresses(&self) -> Result<Vec<Pubkey>> {
        let accounts_guard = self.accounts.read().map_err(|e| {
            anyhow!(
                "Failed to lock the Oracle accounts map for returning all addresses! {}",
                e
            )
        })?;

        Ok(accounts_guard.keys().cloned().collect())
    }

    pub fn len(&self) -> Result<usize> {
        Ok(self
            .accounts
            .read()
            .map_err(|e| {
                anyhow!(
                    "Failed to lock the Oracle accounts map checking the size! {}",
                    e
                )
            })?
            .len())
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
            .try_insert(oracle_address, oracle_account.clone(), bank_address)
            .is_ok());
        assert_eq!(cache.accounts.get(&oracle_address), Some(&oracle_account));
        assert!(cache
            .oracle_wrappers
            .read()
            .unwrap()
            .contains_key(&(oracle_address)));
        assert_eq!(
            cache.oracle_to_banks.get(&oracle_address),
            Some(&vec![bank_address])
        );
        assert_eq!(
            cache.bank_to_oracles.get(&bank_address),
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

        cache.bank_to_oracles.insert(bank_address, oracle_address);
        cache
            .oracle_wrappers
            .write()
            .unwrap()
            .insert(oracle_address, wrapper.clone());

        let retrieved_wrapper = cache.get_wrapper_for_bank(&bank_address);
        assert!(retrieved_wrapper.is_some());
        assert_eq!(retrieved_wrapper.unwrap().get_address(), oracle_address);
    }

    #[test]
    fn test_try_get_bank_from_oracle() {
        let mut cache = OraclesCache::<TestOracleWrapper>::new();
        let bank_address = Pubkey::new_unique();
        let oracle_address = Pubkey::new_unique();

        cache
            .oracle_to_banks
            .insert(oracle_address, vec![bank_address]);

        let retrieved_banks = cache.try_get_banks_for_oracle(&oracle_address).unwrap();
        assert_eq!(retrieved_banks.len(), 1);
        assert_eq!(retrieved_banks[0], bank_address);
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
            .try_bind_with_bank(&oracle_address, &bank_address)
            .is_ok());
        assert_eq!(
            cache.oracle_to_banks.get(&oracle_address),
            Some(&vec![bank_address])
        );
        assert_eq!(
            cache.bank_to_oracles.get(&bank_address),
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
