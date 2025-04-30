use std::{collections::HashMap, sync::RwLock};

use marginfi::state::price::OraclePriceFeedAdapter;
use solana_sdk::{account::Account, pubkey::Pubkey};

use crate::wrappers::oracle::OracleWrapperTrait;
use anyhow::{anyhow, Result};

pub struct OraclesCache<T: OracleWrapperTrait + Clone> {
    accounts: HashMap<Pubkey, Account>,
    account_wrappers: RwLock<HashMap<Pubkey, T>>,
    oracle_to_bank: HashMap<Pubkey, Pubkey>,
    bank_to_oracle: HashMap<Pubkey, Pubkey>,
}

impl<T: OracleWrapperTrait + Clone> OraclesCache<T> {
    pub fn new() -> Self {
        Self {
            accounts: HashMap::new(),
            account_wrappers: RwLock::new(HashMap::new()),
            oracle_to_bank: HashMap::new(),
            bank_to_oracle: HashMap::new(),
        }
    }

    pub fn try_insert(&mut self, account: Account, wrapper: T, bank_address: Pubkey) -> Result<()> {
        let oracle_address = wrapper.get_address();
        self.accounts.insert(oracle_address, account);
        self.account_wrappers
            .write()
            .map_err(|e| anyhow!("Failed to lock the account_wrappers map for insert! {}", e))?
            .insert(oracle_address, wrapper);
        self.oracle_to_bank.insert(oracle_address, bank_address);
        self.bank_to_oracle.insert(bank_address, oracle_address);

        Ok(())
    }

    pub fn try_update_account_wrapper(
        &self,
        address: &Pubkey,
        price_adapter: OraclePriceFeedAdapter,
    ) -> Result<()> {
        let mut wrapper: T = {
            self.account_wrappers
                .read()
                .map_err(|e| anyhow!("Failed to lock the account_wrappers map for search! {}", e))?
                .get(address)
                .ok_or(anyhow::anyhow!(
                    "Failed ot find the Oracle Wrapper for the Address {} in Cache!",
                    address
                ))?
                .clone()
        };

        wrapper.set_price_adapter(price_adapter);
        self.account_wrappers
            .write()
            .map_err(|e| anyhow!("Failed to lock the account_wrappers map for update! {}", e))?
            .insert(*address, wrapper);

        Ok(())
    }

    pub fn get_accounts(&self, addresses: &[Pubkey]) -> Vec<(Pubkey, Account)> {
        addresses
            .iter()
            .filter_map(|address| {
                self.accounts
                    .get(address)
                    .map(|account| (*address, account.clone()))
            })
            .collect()
    }

    pub fn get_addresses(&self) -> Vec<Pubkey> {
        if let Ok(wrappers) = self.account_wrappers.read().inspect_err(|e| {
            eprintln!(
                "Failed to lock the account_wrappers map for collecting addresses! {}",
                e
            )
        }) {
            wrappers.keys().cloned().collect()
        } else {
            vec![]
        }
    }

    pub fn get_wrapper_from_bank(&self, bank_address: &Pubkey) -> Option<T> {
        let oracle = self.bank_to_oracle.get(bank_address)?;
        let wrapper = self
            .account_wrappers
            .read()
            .inspect_err(|e| {
                eprintln!(
                    "Failed to lock the account_wrappers map searching address! {}",
                    e
                )
            })
            .ok()?
            .get(oracle)
            .cloned()?;
        Some(wrapper.clone())
    }
    pub fn try_get_wrapper_from_bank(&self, bank_pk: &Pubkey) -> Result<T> {
        let oracle_pk = self.bank_to_oracle.get(bank_pk).ok_or(anyhow::anyhow!(
            "Failed ot find the Oracle Pubkey for the Bank {} in Cache!",
            bank_pk
        ))?;
        let oracle = self
            .account_wrappers
            .read()
            .map_err(|e| {
                anyhow!(
                    "Failed to lock the account_wrappers map for search address by bank! {}",
                    e
                )
            })?
            .get(oracle_pk)
            .ok_or(anyhow::anyhow!(
                "Failed ot find the Oracle for the Pubkey {} in Cache!",
                oracle_pk
            ))?
            .clone();
        Ok(oracle)
    }

    pub fn try_get_bank_from_oracle(&self, oracle_address: &Pubkey) -> Result<Pubkey> {
        let bank_address = self
            .oracle_to_bank
            .get(oracle_address)
            .ok_or(anyhow::anyhow!(
                "Failed ot find the Bank address for the Oracle {} in Cache!",
                oracle_address
            ))?;
        Ok(*bank_address)
    }
}
