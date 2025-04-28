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

    pub fn insert(
        &mut self,
        mint_address: Pubkey,
        mint: Account,
        token_address: Pubkey,
    ) -> anyhow::Result<()> {
        self.mints.insert(
            mint_address,
            MintWrapper::new(mint_address, token_address, mint),
        );
        self.token_to_mint.insert(token_address, mint_address);
        Ok(())
    }

    pub fn get_mint_account(&self, address: &Pubkey) -> Option<MintWrapper> {
        self.mints.get(address).map(|mint| mint.clone())
    }

    pub fn try_get_mint_account(&self, address: &Pubkey) -> Result<MintWrapper> {
        self.get_mint_account(address)
            .ok_or(anyhow!(
                "Failed ot find Mint for the Address {} in Cache!",
                &address
            ))
            .map(|mint| mint.clone())
    }

    pub fn get_token_addresses(&self) -> Vec<Pubkey> {
        self.mints.values().map(|mint| mint.token.clone()).collect()
    }

    pub fn get_mint_token_addresses(&self) -> Vec<(Pubkey, Pubkey)> {
        self.mints
            .iter()
            .map(|(mint_address, mint)| (mint_address.clone(), mint.token.clone()))
            .collect()
    }

    pub fn try_get_mint_for_token(&self, token_address: &Pubkey) -> Result<Pubkey> {
        self.token_to_mint
            .get(token_address)
            .ok_or(anyhow!(
                "Failed ot find Mint for the Token {} in Cache!",
                &token_address
            ))
            .map(|address| address.clone())
    }

    pub fn try_get_mint_account_for_token(&self, token_address: &Pubkey) -> Result<MintWrapper> {
        let mint_address = self.try_get_mint_for_token(token_address)?;
        self.try_get_mint_account(&mint_address)
    }

    pub fn len(&self) -> usize {
        self.mints.len()
    }
}
