mod accounts;
mod banks;
pub mod mints;
mod oracles;
mod tokens;

use std::sync::{Arc, Mutex};

use accounts::MarginfiAccountsCache;
use anyhow::Result;
use banks::BanksCache;
use mints::MintsCache;
use oracles::OraclesCache;
use solana_sdk::{address_lookup_table::AddressLookupTableAccount, clock::Clock, pubkey::Pubkey};
use tokens::TokensCache;

use crate::{
    utils::accessor,
    wrappers::{oracle::OracleWrapperTrait, token_account::TokenAccountWrapper},
};

pub struct Cache {
    pub signer_pk: Pubkey,
    pub marginfi_program_id: Pubkey,
    pub marginfi_group_address: Pubkey,
    pub marginfi_accounts: MarginfiAccountsCache,
    pub banks: BanksCache,
    pub mints: MintsCache,
    pub oracles: OraclesCache,
    pub tokens: TokensCache,
    pub clock: Arc<Mutex<Clock>>,
    pub luts: Vec<AddressLookupTableAccount>,
}

impl Cache {
    pub fn new(
        signer_pk: Pubkey,
        marginfi_program_id: Pubkey,
        marginfi_group_address: Pubkey,
        clock: Arc<Mutex<Clock>>,
    ) -> Self {
        Self {
            signer_pk,
            marginfi_program_id,
            marginfi_group_address,
            marginfi_accounts: MarginfiAccountsCache::default(),
            banks: BanksCache::default(),
            mints: MintsCache::default(),
            oracles: OraclesCache::default(),
            tokens: TokensCache::default(),
            clock,
            luts: vec![],
        }
    }

    pub fn add_lut(&mut self, lut: AddressLookupTableAccount) {
        self.luts.push(lut)
    }

    pub fn try_get_token_wrapper<T: OracleWrapperTrait>(
        &self,
        mint_address: &Pubkey,
        token_address: &Pubkey,
    ) -> Result<TokenAccountWrapper<T>> {
        let token_account = self.tokens.try_get_account(token_address)?;
        let bank_address = self.banks.try_get_account_for_mint(mint_address)?;
        let bank_wrapper = self.banks.try_get_bank(&bank_address)?;
        let oracle_wrapper = T::build(self, &bank_address)?;

        Ok(TokenAccountWrapper {
            balance: accessor::amount(&token_account.data)?,
            bank_wrapper,
            oracle_wrapper,
        })
    }
}

#[cfg(test)]
pub mod test_utils {
    use std::{
        collections::HashSet,
        sync::{Arc, Mutex, RwLock},
    };

    use solana_sdk::{account::Account, clock::Clock, pubkey::Pubkey};

    use crate::wrappers::{bank::BankWrapper, oracle::test_utils::create_empty_oracle_account};

    use super::Cache;

    pub fn create_test_cache(bank_wrappers: &Vec<BankWrapper>) -> Cache {
        let mut cache = Cache::new(
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Arc::new(Mutex::new(Clock::default())),
            Arc::new(RwLock::new(HashSet::new())),
        );

        for bank_wrapper in bank_wrappers {
            let token_address = Pubkey::new_unique();
            let mut token_account = Account::default();
            token_account.data.resize(128, 0);
            cache
                .mints
                .insert(bank_wrapper.bank.mint, Account::default(), token_address);
            cache
                .tokens
                .try_insert(token_address, token_account, bank_wrapper.bank.mint)
                .unwrap();
            cache.banks.insert(bank_wrapper.address, bank_wrapper.bank);

            let oracle_account = create_empty_oracle_account();
            cache
                .oracles
                .try_insert(bank_wrapper.bank.config.oracle_keys[0], oracle_account)
                .unwrap();
        }

        cache
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use solana_sdk::pubkey::Pubkey;

    #[test]
    fn test_cache() {
        let signer_pk = Pubkey::new_unique();
        let marginfi_program_id = Pubkey::new_unique();
        let marginfi_group_address = Pubkey::new_unique();
        let cache = Cache::new(
            signer_pk,
            marginfi_program_id,
            marginfi_group_address,
            Arc::new(Mutex::new(Clock::default())),
            Arc::new(RwLock::new(HashSet::new())),
        );
        assert_eq!(cache.signer_pk, signer_pk);
        assert_eq!(cache.marginfi_program_id, marginfi_program_id);
        assert_eq!(cache.marginfi_group_address, marginfi_group_address);
    }

    #[test]
    fn test_add_lut() {
        let signer_pk = Pubkey::new_unique();
        let marginfi_program_id = Pubkey::new_unique();
        let marginfi_group_address = Pubkey::new_unique();
        let preferred_mints = Arc::new(RwLock::new(HashSet::new()));
        let mut cache = Cache::new(
            signer_pk,
            marginfi_program_id,
            marginfi_group_address,
            Arc::new(Mutex::new(Clock::default())),
            preferred_mints,
        );
        let lut = AddressLookupTableAccount {
            key: Pubkey::new_unique(),
            addresses: vec![Pubkey::new_unique()],
        };
        assert_eq!(cache.luts.len(), 0);
        cache.add_lut(lut.clone());
        assert_eq!(cache.luts.len(), 1);
        assert_eq!(cache.luts[0].key, lut.key);
    }
}
