use anyhow::{anyhow, Result};
use fixed::types::I80F48;
use solana_sdk::{account::Account, pubkey::Pubkey};
use std::{collections::HashMap, sync::RwLock};

#[derive(Default)]
pub struct OraclesCache {
    accounts: RwLock<HashMap<Pubkey, Account>>,
    simulated_prices: RwLock<HashMap<Pubkey, f64>>,
}

impl OraclesCache {
    pub fn try_insert(&mut self, address: Pubkey, account: Account) -> Result<()> {
        self.accounts
            .write()
            .map_err(|e| anyhow!("Failed to lock the Oracle accounts map for insert! {}", e))?
            .insert(address, account);

        self.simulated_prices
            .write()
            .map_err(|e| {
                anyhow!(
                    "Failed to lock the Oracle simulated prices map for insert! {}",
                    e
                )
            })?
            .remove(&address);

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

        self.simulated_prices
            .write()
            .map_err(|e| {
                anyhow!(
                    "Failed to lock the Oracle simulated prices map for insert! {}",
                    e
                )
            })?
            .remove(address);
        Ok(())
    }

    pub fn try_update_simulated_prices(
        &self,
        simulated_prices: HashMap<Pubkey, f64>,
    ) -> Result<()> {
        self.simulated_prices
            .write()
            .map_err(|e| {
                anyhow!(
                    "Failed to lock the Oracle simulated prices map for update! {}",
                    e
                )
            })?
            .extend(simulated_prices);
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

    pub fn try_get_simulated_price(&self, address: &Pubkey) -> Result<Option<I80F48>> {
        Ok(self
            .simulated_prices
            .read()
            .map_err(|e| {
                anyhow!(
                    "Failed to lock the Oracle simulated prices map for for search! {}",
                    e
                )
            })?
            .get(address)
            .and_then(|price| I80F48::checked_from_num(*price)))
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

    //Handy for obtaining the most current oracles configuration. Requires the `print_oracles` feature to be enabled.
    #[cfg(feature = "print_oracles")]
    pub fn print_oracles_info(&self) {
        println!("Address, Owner");
        for (address, account) in self.accounts.read().unwrap().iter() {
            println!("{}, {}", address, account.owner);
        }
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

    #[test]
    fn test_try_update_simulated_prices_and_get_simulated_price() {
        let cache = OraclesCache::default();
        let address1 = Pubkey::new_unique();
        let address2 = Pubkey::new_unique();

        let mut simulated_prices = HashMap::new();
        simulated_prices.insert(address1, 123.45_f64);
        simulated_prices.insert(address2, 67.89_f64);

        cache.try_update_simulated_prices(simulated_prices.clone()).unwrap();

        let price1 = cache.try_get_simulated_price(&address1).unwrap();
        let price2 = cache.try_get_simulated_price(&address2).unwrap();

        assert_eq!(price1.unwrap().to_num::<f64>(), 123.45_f64);
        assert_eq!(price2.unwrap().to_num::<f64>(), 67.89_f64);
    }

    #[test]
    fn test_try_update_simulated_prices_overwrite() {
        let cache = OraclesCache::default();
        let address = Pubkey::new_unique();

        let mut simulated_prices = HashMap::new();
        simulated_prices.insert(address, 10.0_f64);
        cache.try_update_simulated_prices(simulated_prices).unwrap();

        let mut new_prices = HashMap::new();
        new_prices.insert(address, 20.0_f64);
        cache.try_update_simulated_prices(new_prices).unwrap();

        let price = cache.try_get_simulated_price(&address).unwrap();
        assert_eq!(price.unwrap().to_num::<f64>(), 20.0_f64);
    }

    #[test]
    fn test_try_get_simulated_price_none() {
        let cache = OraclesCache::default();
        let address = Pubkey::new_unique();
        let price = cache.try_get_simulated_price(&address).unwrap();
        assert!(price.is_none());
    }

    #[test]
    fn test_try_update_and_remove_simulated_price_on_account_update() {
        let mut cache = OraclesCache::default();
        let address = Pubkey::new_unique();

        // Insert simulated price
        let mut simulated_prices = HashMap::new();
        simulated_prices.insert(address, 42.0_f64);
        cache.try_update_simulated_prices(simulated_prices).unwrap();
        assert!(cache.try_get_simulated_price(&address).unwrap().is_some());

        // Insert account, which should remove simulated price
        cache.try_insert(address, Account::default()).unwrap();
        assert!(cache.try_get_simulated_price(&address).unwrap().is_none());
    }

    #[test]
    fn test_try_update_and_remove_simulated_price_on_account_update_existing() {
        let mut cache = OraclesCache::default();
        let address = Pubkey::new_unique();

        cache.try_insert(address, Account::default()).unwrap();

        let mut simulated_prices = HashMap::new();
        simulated_prices.insert(address, 99.0_f64);
        cache.try_update_simulated_prices(simulated_prices).unwrap();
        assert!(cache.try_get_simulated_price(&address).unwrap().is_some());

        // Update account, which should remove simulated price
        cache.try_update(&address, Account::default()).unwrap();
        assert!(cache.try_get_simulated_price(&address).unwrap().is_none());
    }
}
