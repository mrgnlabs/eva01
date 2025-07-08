use std::collections::{HashMap, HashSet};

use marginfi::state::marginfi_group::Bank;
use solana_sdk::pubkey::Pubkey;

use crate::{utils::find_oracle_keys, wrappers::bank::BankWrapper};
use anyhow::{anyhow, Result};
#[derive(Default)]
pub struct BanksCache {
    banks: HashMap<Pubkey, BankWrapper>,
    mint_to_bank: HashMap<Pubkey, Pubkey>,
}

impl BanksCache {
    pub fn insert(&mut self, bank_address: Pubkey, bank: Bank) {
        self.banks
            .insert(bank_address, BankWrapper::new(bank_address, bank));
        self.mint_to_bank.insert(bank.mint, bank_address);
    }

    pub fn try_get_bank(&self, address: &Pubkey) -> Result<BankWrapper> {
        self.banks
            .get(address)
            .ok_or(anyhow!("Failed to find the Bank {} in Cache!", address))
            .cloned()
    }

    pub fn get_bank(&self, address: &Pubkey) -> Option<BankWrapper> {
        self.try_get_bank(address).ok()
    }

    pub fn get_oracles(&self) -> HashSet<Pubkey> {
        self.banks
            .iter()
            .flat_map(|(_, bank)| find_oracle_keys(&bank.bank.config))
            .collect()
    }

    pub fn try_get_account_for_mint(&self, mint_address: &Pubkey) -> Result<Pubkey> {
        self.mint_to_bank
            .get(mint_address)
            .ok_or(anyhow!(
                "Failed to find Bank for the Mint {} in Cache!",
                &mint_address
            ))
            .copied()
    }

    pub fn get_mints(&self) -> Vec<Pubkey> {
        self.banks
            .values()
            .map(|bank| bank.bank.mint)
            .collect::<Vec<_>>()
    }

    pub fn len(&self) -> usize {
        self.banks.len()
    }

    //Handy for printing the most current banks configuration. Requires the `print_banks` feature to be enabled.
    #[cfg(feature = "print_banks")]
    pub fn print_banks_info(&self) {
        println!("Group, Bank, State, Risk-Tier, Mint, Oracle_Type, Oracles");
        for bank in self.banks.values() {
            println!(
                "{:?}, {:?}, {:?}, {:?}, {:?}, {:?}, {:?}",
                bank.bank.group,
                bank.address,
                bank.bank.config.operational_state,
                bank.bank.config.risk_tier,
                bank.bank.mint,
                bank.bank.config.oracle_setup,
                find_oracle_keys(&bank.bank.config)
            );
        }
    }
}

#[cfg(test)]
pub mod test_utils {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use fixed::types::I80F48;
    use fixed_macro::types::I80F48;
    use marginfi::{
        constants::MAX_ORACLE_KEYS,
        state::{
            marginfi_group::{Bank, BankConfig, InterestRateConfig},
            price::OracleSetup,
        },
    };

    pub fn create_test_bank(mint: Pubkey) -> Bank {
        let current_timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        Bank {
            mint,
            asset_share_value: I80F48::ONE.into(),
            liability_share_value: I80F48::ONE.into(),
            total_liability_shares: I80F48!(207_112_621_602).into(),
            total_asset_shares: I80F48!(10_000_000_000_000).into(),
            last_update: current_timestamp,
            config: BankConfig {
                oracle_setup: OracleSetup::SwitchboardPull,
                oracle_keys: [Pubkey::new_unique(); MAX_ORACLE_KEYS],
                asset_weight_init: I80F48!(0.5).into(),
                asset_weight_maint: I80F48!(0.75).into(),
                liability_weight_init: I80F48!(1.5).into(),
                liability_weight_maint: I80F48!(1.25).into(),
                borrow_limit: u64::MAX,
                deposit_limit: u64::MAX,
                interest_rate_config: InterestRateConfig {
                    optimal_utilization_rate: I80F48!(0.6).into(),
                    plateau_interest_rate: I80F48!(0.40).into(),
                    protocol_fixed_fee_apr: I80F48!(0.01).into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        }
    }
}

#[cfg(test)]
pub mod tests {
    use crate::cache::banks::test_utils::create_test_bank;

    use super::*;

    #[test]
    fn test_insert_and_get_account() {
        let mut cache = BanksCache::default();
        let bank_address = Pubkey::new_unique();
        let bank = create_test_bank(Pubkey::new_unique());

        cache.insert(bank_address, bank);
        let retrieved_bank = cache.get_bank(&bank_address);

        assert!(retrieved_bank.is_some());
        assert_eq!(retrieved_bank.unwrap().address, bank_address);
    }

    #[test]
    fn test_try_get_account() {
        let mut cache = BanksCache::default();
        let bank_address = Pubkey::new_unique();
        let bank = create_test_bank(Pubkey::new_unique());

        cache.insert(bank_address, bank);
        let result = cache.try_get_bank(&bank_address);

        assert!(result.is_ok());
        assert_eq!(result.unwrap().address, bank_address);
    }

    #[test]
    fn test_get_oracles() {
        let mut cache = BanksCache::default();
        let bank_address = Pubkey::new_unique();
        let bank = create_test_bank(Pubkey::new_unique());

        cache.insert(bank_address, bank);
        let oracles = cache.get_oracles();

        assert_eq!(oracles.len(), 1);
        assert_eq!(oracles.iter().next().unwrap(), &bank.config.oracle_keys[0]);
    }

    #[test]
    fn test_try_get_account_for_mint() {
        let mut cache = BanksCache::default();
        let bank_address = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let bank = create_test_bank(mint);

        cache.insert(bank_address, bank);
        let result = cache.try_get_account_for_mint(&mint);

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), bank_address);
    }

    #[test]
    fn test_get_mints() {
        let mut cache = BanksCache::default();
        let bank_address1 = Pubkey::new_unique();
        let bank_address2 = Pubkey::new_unique();
        let mint1 = Pubkey::new_unique();
        let mint2 = Pubkey::new_unique();
        let bank1 = create_test_bank(mint1);
        let bank2 = create_test_bank(mint2);

        cache.insert(bank_address1, bank1);
        cache.insert(bank_address2, bank2);

        let mints = cache.get_mints();
        assert_eq!(mints.len(), 2);
        assert!(mints.contains(&mint1));
        assert!(mints.contains(&mint2));
    }

    #[test]
    fn test_len() {
        let mut cache = BanksCache::default();
        assert_eq!(cache.len(), 0);

        let bank_address = Pubkey::new_unique();
        let bank = create_test_bank(Pubkey::new_unique());
        cache.insert(bank_address, bank);

        assert_eq!(cache.len(), 1);
    }
}
