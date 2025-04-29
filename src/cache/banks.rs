use std::collections::HashMap;

use marginfi::state::marginfi_group::Bank;
use solana_sdk::pubkey::Pubkey;

use crate::utils::find_oracle_keys;
use anyhow::{anyhow, Result};
pub struct BanksCache {
    banks: HashMap<Pubkey, Bank>,
    mint_to_bank: HashMap<Pubkey, Pubkey>,
}

impl BanksCache {
    pub fn new() -> Self {
        Self {
            banks: HashMap::new(),
            mint_to_bank: HashMap::new(),
        }
    }

    pub fn insert(&mut self, bank_address: Pubkey, bank: Bank) {
        self.banks.insert(bank_address, bank);
        self.mint_to_bank.insert(bank.mint, bank_address);
    }

    pub fn get_account(&self, address: &Pubkey) -> Option<Bank> {
        self.banks.get(address).map(|bank| bank.clone())
    }

    pub fn try_get_account(&self, address: &Pubkey) -> Result<Bank> {
        self.get_account(address)
            .ok_or(anyhow!("Failed ot find the Bank {} in Cache!", &address))
    }

    pub fn get_accounts(&self) -> Vec<(Pubkey, Bank)> {
        self.banks
            .iter()
            .map(|(address, bank)| (*address, bank.clone()))
            .collect()
    }

    pub fn get_oracles(&self) -> Vec<Pubkey> {
        self.banks
            .iter()
            .flat_map(|(_, bank)| find_oracle_keys(&bank.config))
            .collect::<Vec<_>>()
    }

    pub fn try_get_account_for_mint(&self, mint_address: &Pubkey) -> Result<Pubkey> {
        self.mint_to_bank
            .get(mint_address)
            .ok_or(anyhow!(
                "Failed to find Bank for the Mint {} in Cache!",
                &mint_address
            ))
            .map(|bank_address| bank_address.clone())
    }

    pub fn get_mints(&self) -> Vec<Pubkey> {
        self.banks
            .values()
            .map(|bank| bank.mint)
            .collect::<Vec<_>>()
    }

    pub fn len(&self) -> usize {
        self.banks.len()
    }
}
