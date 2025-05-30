mod accounts;
mod banks;
mod mints;
mod oracles;
mod tokens;

use std::sync::{Arc, Mutex};

use accounts::MarginfiAccountsCache;
use anyhow::Result;
use banks::BanksCache;
use mints::MintsCache;
use oracles::OraclesCache;
use solana_sdk::{account::Account, clock::Clock, pubkey::Pubkey};
use tokens::TokensCache;

use crate::{
    utils::accessor,
    wrappers::{
        bank::BankWrapperT,
        oracle::{try_build_oracle_wrapper, OracleWrapperTrait},
        token_account::TokenAccountWrapperT,
    },
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
            marginfi_accounts: MarginfiAccountsCache::new(),
            banks: BanksCache::new(),
            mints: MintsCache::new(),
            oracles: OraclesCache::new(),
            tokens: TokensCache::new(),
            clock,
        }
    }

    pub fn try_get_bank_wrapper<T: OracleWrapperTrait + Clone>(
        &self,
        bank_pk: &Pubkey,
    ) -> Result<BankWrapperT<T>> {
        let bank = self.banks.try_get_bank(bank_pk)?;
        let oracle = try_build_oracle_wrapper(self, bank_pk)?;
        Ok(BankWrapperT::new(*bank_pk, bank, oracle))
    }

    pub fn get_token_account_for_bank(&self, bank_pk: &Pubkey) -> Option<Account> {
        let mint = self.banks.get_bank(bank_pk)?.mint;
        let token = self.tokens.get_token_for_mint(&mint)?;
        self.tokens.get_account(&token)
    }

    pub fn try_get_token_wrapper<T: OracleWrapperTrait + Clone>(
        &self,
        token_address: &Pubkey,
    ) -> Result<TokenAccountWrapperT<T>> {
        let token_account = self.tokens.try_get_account(token_address)?;
        let mint_address = self.mints.try_get_mint_for_token(token_address)?;
        let bank_address = self.banks.try_get_account_for_mint(&mint_address)?;
        let bank_wrapper = self.try_get_bank_wrapper(&bank_address)?;

        Ok(TokenAccountWrapperT {
            balance: accessor::amount(&token_account.data),
            bank: bank_wrapper,
        })
    }
}

#[cfg(test)]
pub mod test_utils {
    use std::sync::{Arc, Mutex};

    use solana_sdk::{account::Account, clock::Clock, pubkey::Pubkey};

    use crate::wrappers::bank::test_utils::TestBankWrapper;

    use super::Cache;

    pub fn create_test_cache(bank_wrappers: &Vec<TestBankWrapper>) -> Cache {
        let mut cache = Cache::new(
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Arc::new(Mutex::new(Clock::default())),
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

            let oracle_account = Account::new(0, 0, &Pubkey::new_unique());
            cache
                .oracles
                .try_insert(bank_wrapper.oracle_adapter.address.clone(), oracle_account)
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
        );
        assert_eq!(cache.signer_pk, signer_pk);
        assert_eq!(cache.marginfi_program_id, marginfi_program_id);
        assert_eq!(cache.marginfi_group_address, marginfi_group_address);
    }
}
