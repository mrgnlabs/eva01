use anyhow::{anyhow, Result};
use solana_sdk::{account::Account, pubkey::Pubkey};
use std::{collections::HashMap, sync::RwLock};

#[derive(Default)]
pub struct OraclesCache {
    accounts: RwLock<HashMap<Pubkey, Account>>,
}

impl OraclesCache {
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

    pub fn try_get_accounts(&self, addresses: &[Pubkey]) -> Result<Vec<(Pubkey, Account)>> {
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

    pub fn try_get_addresses(&self) -> Result<Vec<Pubkey>> {
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
        let mut cache = OraclesCache::default();
        let oracle_account = Account::default();
        let oracle_wrapper = TestOracleWrapper::test_sol();
        let oracle_address = oracle_wrapper.address;

        assert!(cache
            .try_insert(oracle_address, oracle_account.clone())
            .is_ok());
        assert_eq!(
            cache.try_get_account(&oracle_address).unwrap(),
            oracle_account
        );
    }

    #[test]
    fn test_get_accounts() {
        let mut cache = OraclesCache::default();
        let address1 = Pubkey::new_unique();
        let account1 = Account::default();
        cache.try_insert(address1, account1.clone()).unwrap();
        let address2 = Pubkey::new_unique();
        let account2 = Account::default();
        cache.try_insert(address2, account2.clone()).unwrap();

        let accounts = cache.try_get_accounts(&[address1, address2]).unwrap();
        assert_eq!(cache.len().unwrap(), 2);
        assert!(accounts.contains(&(address1, account1)));
        assert!(accounts.contains(&(address2, account2)));
    }

    #[test]
    fn test_get_addresses() {
        let mut cache = OraclesCache::default();
        let address1 = Pubkey::new_unique();
        let address2 = Pubkey::new_unique();

        cache.try_insert(address1, Account::default()).unwrap();
        cache.try_insert(address2, Account::default()).unwrap();

        let addresses = cache.try_get_addresses().unwrap();
        assert_eq!(addresses.len(), 2);
        assert!(addresses.contains(&address1));
        assert!(addresses.contains(&address2));
    }
}
