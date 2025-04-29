use std::{collections::HashMap, sync::RwLock};

use anyhow::{anyhow, Result};
use indexmap::IndexMap;
use solana_sdk::{account::Account, pubkey::Pubkey};

pub struct TokensCache {
    tokens: RwLock<IndexMap<Pubkey, Account>>,
    mint_to_token: HashMap<Pubkey, Pubkey>,
}

impl TokensCache {
    pub fn new() -> Self {
        Self {
            tokens: RwLock::new(IndexMap::new()),
            mint_to_token: HashMap::new(),
        }
    }

    pub fn try_insert(
        &mut self,
        token_address: Pubkey,
        token: Account,
        mint_address: Pubkey,
    ) -> anyhow::Result<()> {
        self.tokens
            .write()
            .map_err(|e| anyhow!("Failed to lock the token map for insert! {}", e))?
            .insert(token_address, token);
        self.mint_to_token.insert(mint_address, token_address);
        Ok(())
    }

    pub fn get_account(&self, address: &Pubkey) -> Option<Account> {
        self.tokens
            .read()
            .inspect_err(|e| eprintln!("Failed to lock the tokens map for search! {}", e))
            .ok()?
            .get(address)
            .cloned()
    }

    pub fn try_get_account(&self, address: &Pubkey) -> Result<Account> {
        self.get_account(address).ok_or(anyhow!(
            "Failed ot find Token for the Address {} in Cache!",
            &address
        ))
    }

    pub fn get_address_by_index(&self, index: usize) -> Option<Pubkey> {
        self.tokens
            .read()
            .inspect_err(|e| {
                eprintln!(
                    "Failed to lock the tokens accounts map for search by index! {}",
                    e
                )
            })
            .ok()?
            .get_index(index)
            .map(|(address, _)| *address)
    }

    pub fn try_update_account(&self, address: Pubkey, account: Account) -> Result<()> {
        self.tokens
            .write()
            .map_err(|e| anyhow!("Failed to lock the token map for update! {}", e))?
            .insert(address, account)
            .ok_or(anyhow!(
                "Failed ot update Token for the Address {} in Cache!",
                &address
            ))?;
        Ok(())
    }

    pub fn get_token_for_mint(&self, mint_address: &Pubkey) -> Option<Pubkey> {
        self.mint_to_token.get(mint_address).copied()
    }

    pub fn try_get_token_for_mint(&self, mint_address: &Pubkey) -> Result<Pubkey> {
        self.mint_to_token
            .get(mint_address)
            .ok_or(anyhow!(
                "Failed to find Token for the Mint {} in Cache!",
                &mint_address
            ))
            .copied()
    }

    pub fn len(&self) -> Result<usize> {
        Ok(self
            .tokens
            .read()
            .map_err(|e| anyhow!("Failed to lock the tokens map for size! {}", e))?
            .len())
    }
}
