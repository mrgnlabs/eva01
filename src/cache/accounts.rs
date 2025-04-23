use std::collections::HashMap;

use solana_sdk::pubkey::Pubkey;

use crate::wrappers::marginfi_account::MarginfiAccountWrapper;

pub struct MarginfiAccountsCache {
    accounts: HashMap<Pubkey, MarginfiAccountWrapper>,
}

impl MarginfiAccountsCache {
    pub fn new() -> Self {
        Self {
            accounts: HashMap::new(),
        }
    }

    pub fn insert(&mut self, account: MarginfiAccountWrapper) {
        self.accounts.insert(account.address.clone(), account);
    }

    pub fn get(&self, address: &Pubkey) -> Option<&MarginfiAccountWrapper> {
        self.accounts.get(address)
    }
}
