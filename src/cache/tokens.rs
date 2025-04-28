use std::collections::HashMap;

use anyhow::{anyhow, Result};
use indexmap::IndexMap;
use solana_sdk::{account::Account, pubkey::Pubkey};

pub struct TokensCache {
    signer: Pubkey,
    tokens: IndexMap<Pubkey, Account>,
    mint_to_token: HashMap<Pubkey, Pubkey>,
}

impl TokensCache {
    pub fn new(signer: Pubkey) -> Self {
        Self {
            signer,
            tokens: IndexMap::new(),
            mint_to_token: HashMap::new(),
        }
    }

    pub fn insert(
        &mut self,
        token_address: Pubkey,
        token: Account,
        mint_address: Pubkey,
    ) -> anyhow::Result<()> {
        self.tokens.insert(token_address, token);
        self.mint_to_token.insert(mint_address, token_address);
        Ok(())
    }

    pub fn get_token_account(&self, address: &Pubkey) -> Option<Account> {
        self.tokens.get(address).map(|account| account.clone())
    }

    pub fn get_token_address_by_index(&self, index: usize) -> Option<Pubkey> {
        self.tokens
            .get_index(index)
            .map(|(address, _)| address.clone())
    }

    pub fn try_get_token_account(&self, address: &Pubkey) -> Result<Account> {
        self.get_token_account(address)
            .ok_or(anyhow!(
                "Failed ot find Token for the Address {} in Cache!",
                &address
            ))
            .map(|mint| mint.clone())
    }

    pub fn get_token_for_mint(&self, mint_address: &Pubkey) -> Option<Pubkey> {
        self.mint_to_token
            .get(mint_address)
            .map(|address| address.clone())
    }

    pub fn try_get_token_for_mint(&self, mint_address: &Pubkey) -> Result<Pubkey> {
        self.mint_to_token
            .get(mint_address)
            .ok_or(anyhow!(
                "Failed to find Token for the Mint {} in Cache!",
                &mint_address
            ))
            .map(|address| address.clone())
    }

    pub fn len(&self) -> usize {
        self.tokens.len()
    }
}
