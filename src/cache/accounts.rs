use std::sync::RwLock;

use indexmap::IndexMap;
use solana_sdk::pubkey::Pubkey;

use crate::wrappers::marginfi_account::MarginfiAccountWrapper;
use anyhow::{anyhow, Result};

pub struct MarginfiAccountsCache {
    accounts: RwLock<IndexMap<Pubkey, MarginfiAccountWrapper>>,
}

impl MarginfiAccountsCache {
    pub fn new() -> Self {
        Self {
            accounts: RwLock::new(IndexMap::new()),
        }
    }

    pub fn try_insert(&self, account: MarginfiAccountWrapper) -> Result<()> {
        self.accounts
            .write()
            .map_err(|e| {
                anyhow!(
                    "Failed to lock the marginfi accounts  map for insert! {}",
                    e
                )
            })?
            .insert(account.address, account);
        Ok(())
    }

    pub fn try_get_account(&self, address: &Pubkey) -> Result<MarginfiAccountWrapper> {
        self.accounts
            .read()
            .map_err(|e| {
                anyhow!(
                    "Failed to lock the marginfi accounts  map for for search! {}",
                    e
                )
            })?
            .get(address)
            .ok_or(anyhow!(
                "Failed ot find the Marginfi account {} in Cache!",
                &address
            ))
            .cloned()
    }

    pub fn get_account_by_index(&self, index: usize) -> Option<MarginfiAccountWrapper> {
        self.accounts
            .read()
            .inspect_err(|e| {
                eprintln!(
                    "Failed to lock the marginfi accounts map for get by index! {}",
                    e
                )
            })
            .ok()?
            .get_index(index)
            .map(|(_, account)| account.clone())
    }

    pub fn len(&self) -> Result<usize> {
        Ok(self
            .accounts
            .read()
            .map_err(|e| {
                anyhow!(
                    "Failed to lock the marginfi accounts  map for getting it's size! {}",
                    e
                )
            })?
            .len())
    }
}
