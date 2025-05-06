use std::collections::HashMap;

use anyhow::{anyhow, Result};
use solana_sdk::{account::Account, pubkey::Pubkey};

use crate::wrappers::mint::MintWrapper;

pub struct MintsCache {
    mints: HashMap<Pubkey, MintWrapper>,
    token_to_mint: HashMap<Pubkey, Pubkey>,
}

impl MintsCache {
    pub fn new() -> Self {
        Self {
            mints: HashMap::new(),
            token_to_mint: HashMap::new(),
        }
    }

    pub fn try_insert(
        &mut self,
        mint_address: Pubkey,
        mint: Account,
        token_address: Pubkey,
    ) -> anyhow::Result<()> {
        self.mints
            .insert(mint_address, MintWrapper::new(token_address, mint));
        self.token_to_mint.insert(token_address, mint_address);
        Ok(())
    }

    pub fn get_account(&self, address: &Pubkey) -> Option<MintWrapper> {
        self.mints.get(address).cloned()
    }

    pub fn try_get_account(&self, address: &Pubkey) -> Result<MintWrapper> {
        self.get_account(address).ok_or(anyhow!(
            "Failed ot find Mint for the Address {} in Cache!",
            &address
        ))
    }

    pub fn get_tokens(&self) -> Vec<Pubkey> {
        self.mints.values().map(|mint| mint.token).collect()
    }

    pub fn try_get_mint_for_token(&self, token_address: &Pubkey) -> Result<Pubkey> {
        self.token_to_mint
            .get(token_address)
            .ok_or(anyhow!(
                "Failed ot find Mint for the Token {} in Cache!",
                &token_address
            ))
            .copied()
    }

    pub fn len(&self) -> usize {
        self.mints.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::{account::Account, pubkey::Pubkey};

    #[test]
    fn test_mints_cache() {
        let mut mints_cache = MintsCache::new();
        let mint_address = Pubkey::new_unique();
        let token_address = Pubkey::new_unique();
        let mint_account = Account::new(0, 0, &Pubkey::new_unique());

        assert!(mints_cache
            .try_insert(mint_address, mint_account.clone(), token_address)
            .is_ok());

        assert_eq!(mints_cache.len(), 1);
        assert!(mints_cache.get_account(&mint_address).is_some());
        assert!(mints_cache.try_get_mint_for_token(&token_address).is_ok());
    }

    #[test]
    fn test_try_get_account_not_found() {
        let mints_cache = MintsCache::new();
        let mint_address = Pubkey::new_unique();

        let result = mints_cache.try_get_account(&mint_address);
        assert!(result.is_err());
    }

    #[test]
    fn test_try_get_mint_for_token_not_found() {
        let mints_cache = MintsCache::new();
        let token_address = Pubkey::new_unique();

        let result = mints_cache.try_get_mint_for_token(&token_address);
        assert!(result.is_err());
    }

    #[test]
    fn test_get_token() {
        let mut mints_cache = MintsCache::new();
        let mint_address = Pubkey::new_unique();
        let token_address = Pubkey::new_unique();
        let mint_account = Account::new(0, 0, &Pubkey::new_unique());

        mints_cache
            .try_insert(mint_address, mint_account.clone(), token_address)
            .unwrap();

        let tokens = mints_cache.get_tokens();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0], token_address);
    }

    #[test]
    fn test_len() {
        let mut mints_cache = MintsCache::new();
        assert_eq!(mints_cache.len(), 0);

        let mint_address = Pubkey::new_unique();
        let token_address = Pubkey::new_unique();
        let mint_account = Account::new(0, 0, &Pubkey::new_unique());

        mints_cache
            .try_insert(mint_address, mint_account.clone(), token_address)
            .unwrap();

        assert_eq!(mints_cache.len(), 1);
    }

    #[test]
    fn test_insert_duplicate() {
        let mut mints_cache = MintsCache::new();
        let mint_address = Pubkey::new_unique();
        let token_address = Pubkey::new_unique();
        let mint_account = Account::new(0, 0, &Pubkey::new_unique());

        assert!(mints_cache
            .try_insert(mint_address, mint_account.clone(), token_address)
            .is_ok());

        // Insert the same mint again
        assert!(mints_cache
            .try_insert(mint_address, mint_account.clone(), token_address)
            .is_ok());

        // Ensure the length remains 1
        assert_eq!(mints_cache.len(), 1);
    }
}
