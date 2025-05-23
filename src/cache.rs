mod accounts;
mod banks;
mod mints;
mod oracles;
mod tokens;

use accounts::MarginfiAccountsCache;
use anyhow::Result;
use banks::BanksCache;
use mints::MintsCache;
use oracles::OraclesCache;
use solana_sdk::{account::Account, pubkey::Pubkey};
use tokens::TokensCache;

use crate::{
    utils::accessor,
    wrappers::{
        bank::BankWrapperT,
        oracle::{OracleWrapper, OracleWrapperTrait},
        token_account::TokenAccountWrapperT,
    },
};

pub struct CacheT<T: OracleWrapperTrait + Clone> {
    pub signer_pk: Pubkey,
    pub marginfi_program_id: Pubkey,
    pub marginfi_group_address: Pubkey,
    pub marginfi_accounts: MarginfiAccountsCache,
    pub banks: BanksCache,
    pub mints: MintsCache,
    pub oracles: OraclesCache<T>,
    pub tokens: TokensCache,
}

pub type Cache = CacheT<OracleWrapper>;

impl<T: OracleWrapperTrait + Clone> CacheT<T> {
    pub fn new(
        signer_pk: Pubkey,
        marginfi_program_id: Pubkey,
        marginfi_group_address: Pubkey,
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
        }
    }

    pub fn get_bank_wrapper(&self, bank_pk: &Pubkey) -> Option<BankWrapperT<T>> {
        let bank = self.banks.get_bank(bank_pk)?;
        let oracle = self.oracles.get_wrapper_from_bank(bank_pk)?;
        Some(BankWrapperT::new(*bank_pk, bank, oracle))
    }

    pub fn try_get_bank_wrapper(&self, bank_pk: &Pubkey) -> Result<BankWrapperT<T>> {
        let bank = self.banks.try_get_bank(bank_pk)?;
        let oracle = self.oracles.try_get_wrapper_from_bank(bank_pk)?;
        Ok(BankWrapperT::new(*bank_pk, bank, oracle))
    }

    pub fn get_token_account_for_bank(&self, bank_pk: &Pubkey) -> Option<Account> {
        let mint = self.banks.get_bank(bank_pk)?.mint;
        let token = self.tokens.get_token_for_mint(&mint)?;
        self.tokens.get_account(&token)
    }

    pub fn try_get_token_wrapper(&self, token_address: &Pubkey) -> Result<TokenAccountWrapperT<T>> {
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
    use solana_sdk::{account::Account, pubkey::Pubkey};

    use crate::wrappers::{
        bank::test_utils::TestBankWrapper, oracle::test_utils::TestOracleWrapper,
    };

    use super::CacheT;

    pub fn create_test_cache(bank_wrappers: &Vec<TestBankWrapper>) -> CacheT<TestOracleWrapper> {
        let mut cache = CacheT::<TestOracleWrapper>::new(
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
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
                .try_insert(
                    bank_wrapper.oracle_adapter.address.clone(),
                    oracle_account,
                    Some(bank_wrapper.oracle_adapter.clone()),
                    bank_wrapper.address,
                )
                .unwrap();
        }

        cache
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{
        cache::test_utils::create_test_cache,
        wrappers::{bank::test_utils::TestBankWrapper, oracle::test_utils::TestOracleWrapper},
    };

    use super::*;
    use solana_sdk::pubkey::Pubkey;

    #[test]
    fn test_cache() {
        let signer_pk = Pubkey::new_unique();
        let marginfi_program_id = Pubkey::new_unique();
        let marginfi_group_address = Pubkey::new_unique();
        let cache: CacheT<TestOracleWrapper> =
            CacheT::new(signer_pk, marginfi_program_id, marginfi_group_address);
        assert_eq!(cache.signer_pk, signer_pk);
        assert_eq!(cache.marginfi_program_id, marginfi_program_id);
        assert_eq!(cache.marginfi_group_address, marginfi_group_address);
    }

    #[test]
    fn test_try_get_token_wrapper() {
        let sol_bank_wrapper = TestBankWrapper::test_sol();
        let usdc_bank_wrapper = TestBankWrapper::test_usdc();
        let cache = create_test_cache(&vec![sol_bank_wrapper.clone(), usdc_bank_wrapper.clone()]);
        let cache = Arc::new(cache);

        let token_address = cache.tokens.get_addresses().get(0).unwrap().clone();
        let result = cache.try_get_token_wrapper(&token_address);
        assert!(result.is_ok());
    }
}
