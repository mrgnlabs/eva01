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
