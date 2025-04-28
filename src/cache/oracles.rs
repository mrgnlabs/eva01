use std::collections::HashMap;

use solana_sdk::pubkey::Pubkey;

use crate::wrappers::oracle::OracleWrapper;
use anyhow::Result;
use log::error;

pub struct OraclesCache {
    oracles: HashMap<Pubkey, OracleWrapper>,
    oracle_to_bank: HashMap<Pubkey, Pubkey>,
    bank_to_oracle: HashMap<Pubkey, Pubkey>,
}

impl OraclesCache {
    pub fn new() -> Self {
        Self {
            oracles: HashMap::new(),
            oracle_to_bank: HashMap::new(),
            bank_to_oracle: HashMap::new(),
        }
    }

    pub fn insert(&mut self, oracle: OracleWrapper, bank_address: Pubkey) {
        let oracle_address = oracle.address.clone();
        self.oracles.insert(oracle_address, oracle);
        self.oracle_to_bank.insert(oracle_address, bank_address);
        self.bank_to_oracle.insert(bank_address, oracle_address);
    }

    pub fn get_oracle_account(&self, address: &Pubkey) -> Option<OracleWrapper> {
        self.oracles
            .get(address)
            .or_else(|| {
                error!(
                    "Failed ot find the Oracle with the Address {} in Cache!",
                    &address,
                );
                None
            })
            .map(|oracle| oracle.clone())
    }

    pub fn get_by_oracle_account_by_bank(&self, bank_address: &Pubkey) -> Option<OracleWrapper> {
        self.bank_to_oracle
            .get(bank_address)
            .map(|pubkey| self.oracles.get(pubkey))?
            .map(|oracle| oracle.clone())
    }
    pub fn try_get_by_bank(&self, bank_pk: &Pubkey) -> Result<OracleWrapper> {
        let oracle_pk = self.bank_to_oracle.get(bank_pk).ok_or(anyhow::anyhow!(
            "Failed ot find the Oracle Pubkey for the Bank {} in Cache!",
            bank_pk
        ))?;
        let oracle = self.oracles.get(oracle_pk).ok_or(anyhow::anyhow!(
            "Failed ot find the Oracle for the Pubkey {} in Cache!",
            oracle_pk
        ))?;
        Ok(oracle.clone())
    }

    pub fn get_oracle_pks(&self) -> Vec<Pubkey> {
        self.oracles.keys().cloned().collect()
    }
}
