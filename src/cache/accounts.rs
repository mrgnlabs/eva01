use indexmap::IndexMap;
use solana_sdk::pubkey::Pubkey;

use crate::wrappers::marginfi_account::MarginfiAccountWrapper;

pub struct MarginfiAccountsCache {
    accounts: IndexMap<Pubkey, MarginfiAccountWrapper>,
}

impl MarginfiAccountsCache {
    pub fn new() -> Self {
        Self {
            accounts: IndexMap::new(),
        }
    }

    pub fn insert(&mut self, account: MarginfiAccountWrapper) {
        self.accounts.insert(account.address.clone(), account);
    }

    pub fn get_account(&self, address: &Pubkey) -> Option<MarginfiAccountWrapper> {
        self.accounts.get(address).map(|account| account.clone())
    }

    pub fn get_account_by_index(&self, index: usize) -> Option<MarginfiAccountWrapper> {
        self.accounts
            .get_index(index)
            .map(|(_, account)| account.clone())
    }

    pub fn len(&self) -> usize {
        self.accounts.len()
    }
}
