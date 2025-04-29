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

    pub fn get_token(&self) -> Vec<Pubkey> {
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
