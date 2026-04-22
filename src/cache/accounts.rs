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

    pub fn try_get_account_batch(
        &self,
        start_index: usize,
        batch_size: usize,
    ) -> Result<Vec<MarginfiAccountWrapper>> {
        if batch_size == 0 {
            return Ok(Vec::new());
        }

        let accounts = self.accounts.read().map_err(|e| {
            anyhow!(
                "Failed to lock the marginfi accounts map for batch snapshot: {}",
                e
            )
        })?;

        if start_index >= accounts.len() {
            return Ok(Vec::new());
        }

        let end_index = (start_index + batch_size).min(accounts.len());
        let mut out = Vec::with_capacity(end_index - start_index);
        for index in start_index..end_index {
            if let Some((_, account)) = accounts.get_index(index) {
                out.push(account.clone());
            }
        }

        Ok(out)
    }
}
