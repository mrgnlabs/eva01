use std::sync::RwLock;

use indexmap::IndexMap;
use solana_sdk::pubkey::Pubkey;

use crate::wrappers::marginfi_account::MarginfiAccountWrapper;
use anyhow::{anyhow, Result};

#[derive(Default)]
pub struct MarginfiAccountsCache {
    accounts: RwLock<IndexMap<Pubkey, MarginfiAccountWrapper>>,
}

impl MarginfiAccountsCache {
    pub fn try_insert(&self, account: MarginfiAccountWrapper) -> Result<()> {
        self.accounts
            .write()
            .map_err(|e| anyhow!("Failed to lock the marginfi accounts map for insert! {}", e))?
            .insert(account.address, account);
        Ok(())
    }

    pub fn try_get_account(&self, address: &Pubkey) -> Result<MarginfiAccountWrapper> {
        self.accounts
            .read()
            .map_err(|e| {
                anyhow!(
                    "Failed to lock the Marginfi accounts map for for search! {}",
                    e
                )
            })?
            .get(address)
            .ok_or(anyhow!("Failed to find the Marginfi account: {}", &address))
            .cloned()
    }

    pub fn try_get_account_by_index(&self, index: usize) -> Result<MarginfiAccountWrapper> {
        self.accounts
            .read()
            .map_err(|e| {
                anyhow!(
                    "Failed to lock the Marginfi accounts map for for search by index: {}",
                    e
                )
            })?
            .get_index(index)
            .map(|(_, account)| account.clone())
            .ok_or(anyhow!(
                "Failed to find the Marginfi account with index: {}",
                index
            ))
    }

    pub fn len(&self) -> Result<usize> {
        Ok(self
            .accounts
            .read()
            .map_err(|e| {
                anyhow!(
                    "Failed to lock the marginfi accounts  map for getting it's size: {}",
                    e
                )
            })?
            .len())
    }
}
#[cfg(test)]
mod tests {
    use marginfi_type_crate::{
        constants::ASSET_TAG_DEFAULT,
        types::{Balance, LendingAccount, WrappedI80F48},
    };

    use super::*;

    fn create_test_account(address: Pubkey) -> MarginfiAccountWrapper {
        MarginfiAccountWrapper {
            address,
            lending_account: LendingAccount {
                balances: [Balance {
                    active: 1,
                    bank_pk: Pubkey::new_unique(),
                    bank_asset_tag: ASSET_TAG_DEFAULT,
                    _pad0: [0; 6],
                    asset_shares: WrappedI80F48::default(),
                    liability_shares: WrappedI80F48::default(),
                    emissions_outstanding: WrappedI80F48::default(),
                    last_update: 0,
                    _padding: [0_u64],
                }; 16],
                _padding: [0; 8],
            },
        }
    }

    #[test]
    fn test_try_insert_and_try_get_account() {
        let cache = MarginfiAccountsCache::default();
        let address = Pubkey::new_unique();
        let account = create_test_account(address);

        // Test insertion
        assert!(cache.try_insert(account.clone()).is_ok());

        // Test retrieval
        let retrieved_account = cache.try_get_account(&address).unwrap();
        assert_eq!(retrieved_account.address, account.address);
    }

    #[test]
    fn test_try_get_account_not_found() {
        let cache = MarginfiAccountsCache::default();
        let address = Pubkey::new_unique();

        // Test retrieval of non-existent account
        let result = cache.try_get_account(&address);
        assert!(result.is_err());
    }

    #[test]
    fn test_get_account_by_index() {
        let cache = MarginfiAccountsCache::default();
        let address1 = Pubkey::new_unique();
        let address2 = Pubkey::new_unique();
        let account1 = create_test_account(address1);
        let account2 = create_test_account(address2);

        // Insert accounts
        cache.try_insert(account1.clone()).unwrap();
        cache.try_insert(account2.clone()).unwrap();

        // Test retrieval by index
        let retrieved_account1 = cache.try_get_account_by_index(0).unwrap();
        let retrieved_account2 = cache.try_get_account_by_index(1).unwrap();
        assert_eq!(retrieved_account1.address, account1.address);
        assert_eq!(retrieved_account2.address, account2.address);

        // Test out-of-bounds index
        assert!(cache.try_get_account_by_index(2).is_err());
    }

    #[test]
    fn test_len() {
        let cache = MarginfiAccountsCache::default();
        let address1 = Pubkey::new_unique();
        let address2 = Pubkey::new_unique();
        let account1 = create_test_account(address1);
        let account2 = create_test_account(address2);

        // Initially empty
        assert_eq!(cache.len().unwrap(), 0);

        // Insert accounts and check length
        cache.try_insert(account1).unwrap();
        assert_eq!(cache.len().unwrap(), 1);

        cache.try_insert(account2).unwrap();
        assert_eq!(cache.len().unwrap(), 2);
    }
}
