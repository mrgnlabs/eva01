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
    wrappers::{bank::BankWrapper, token_account::TokenAccountWrapper},
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
}

impl Cache {
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

    pub fn get_bank_wrapper(&self, bank_pk: &Pubkey) -> Option<BankWrapper> {
        let bank = self.banks.get_account(bank_pk)?;
        let oracle = self.oracles.get_wrapper_from_bank(bank_pk)?;
        Some(BankWrapper::new(*bank_pk, bank, oracle))
    }

    pub fn try_get_bank_wrapper(&self, bank_pk: &Pubkey) -> Result<BankWrapper> {
        let bank = self.banks.try_get_account(bank_pk)?;
        let oracle = self.oracles.try_get_wrapper_from_bank(bank_pk)?;
        Ok(BankWrapper::new(*bank_pk, bank, oracle))
    }

    pub fn get_token_account_for_bank(&self, bank_pk: &Pubkey) -> Option<Account> {
        let mint = self.banks.get_account(bank_pk)?.mint;
        let token = self.tokens.get_token_for_mint(&mint)?;
        self.tokens.get_account(&token)
    }

    pub fn try_get_token_wrapper(&self, token_address: &Pubkey) -> Result<TokenAccountWrapper> {
        let token_account = self.tokens.try_get_account(token_address)?;
        let mint_address = self.mints.try_get_mint_for_token(token_address)?;
        let bank_address = self.banks.try_get_account_for_mint(&mint_address)?;
        let bank_wrapper = self.try_get_bank_wrapper(&bank_address)?;

        Ok(TokenAccountWrapper {
            balance: accessor::amount(&token_account.data),
            bank: bank_wrapper,
        })
    }
}

#[cfg(test)]
pub mod test_utils {
    use solana_sdk::pubkey::Pubkey;

    use super::Cache;

    pub fn create_test_cache() -> Cache {
        Cache::new(
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::pubkey::Pubkey;

    #[test]
    fn test_cache() {
        let signer_pk = Pubkey::new_unique();
        let marginfi_program_id = Pubkey::new_unique();
        let marginfi_group_address = Pubkey::new_unique();
        let cache = Cache::new(signer_pk, marginfi_program_id, marginfi_group_address);
        assert_eq!(cache.signer_pk, signer_pk);
        assert_eq!(cache.marginfi_program_id, marginfi_program_id);
        assert_eq!(cache.marginfi_group_address, marginfi_group_address);
    }
}
