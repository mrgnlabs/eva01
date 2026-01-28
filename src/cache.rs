mod accounts;
mod banks;
pub mod mints;
mod oracles;
mod tokens;

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use accounts::MarginfiAccountsCache;
use anyhow::Result;
use banks::BanksCache;
use log::info;
use marginfi_type_crate::constants::FEE_STATE_SEED;
use mints::MintsCache;
use oracles::OraclesCache;
use solana_client::{rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig};
use solana_sdk::{
    address_lookup_table::{self, state::AddressLookupTable, AddressLookupTableAccount},
    clock::Clock,
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
};
use tokens::TokensCache;

use crate::{
    drift::accounts::{SpotMarket, User as DriftUser},
    kamino_lending::accounts::Reserve,
    utils::accessor,
    wrappers::{oracle::OracleWrapperTrait, token_account::TokenAccountWrapper},
};

const LUT_CAPACITY: usize = 265usize;

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
    pub luts: Arc<Mutex<Vec<AddressLookupTableAccount>>>,
    pub global_fee_state_key: Pubkey,
    pub global_fee_wallet: Pubkey,
    pub kamino_reserves: HashMap<Pubkey, KaminoReserve>,
    pub drift_markets: HashMap<Pubkey, DriftSpotMarket>,
    pub drift_users: HashMap<Pubkey, DriftUser>,
}

pub struct KaminoReserve {
    pub address: Pubkey,
    pub reserve: Reserve,
    pub lending_market_authority: Pubkey,
}

pub struct DriftSpotMarket {
    pub address: Pubkey,
    pub market: SpotMarket,
}

impl Cache {
    pub fn new(
        signer_pk: Pubkey,
        marginfi_program_id: Pubkey,
        marginfi_group_address: Pubkey,
        clock: Arc<Mutex<Clock>>,
    ) -> Self {
        let (global_fee_state_key, _) =
            Pubkey::find_program_address(&[FEE_STATE_SEED.as_bytes()], &marginfi_program_id);
        let luts = Arc::new(Mutex::new(vec![]));
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
            luts,
            global_fee_state_key,
            global_fee_wallet: Pubkey::default(),
            kamino_reserves: HashMap::new(),
            drift_markets: HashMap::new(),
            drift_users: HashMap::new(),
        }
    }

    pub fn add_lut(&mut self, lut: AddressLookupTableAccount) {
        self.luts.lock().unwrap().push(lut)
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

    // TODO: think of a better place for this
    pub fn add_addresses_to_lut(
        &self,
        rpc_client: &RpcClient,
        signer_keypair: &Keypair,
        addresses: HashSet<Pubkey>,
    ) -> anyhow::Result<()> {
        let mut luts = self.luts.lock().unwrap();
        if luts.is_empty() {
            let new_lut = create_lut(rpc_client, signer_keypair, addresses.into_iter().collect())?;
            luts.push(new_lut);
            return Ok(());
        }

        let lut = luts.last_mut().unwrap();
        let existing: HashSet<Pubkey> = lut.addresses.iter().cloned().collect();
        let addresses_to_add: Vec<Pubkey> = addresses
            .into_iter()
            .filter(|k| !existing.contains(k))
            .collect();
        if existing.len() + addresses_to_add.len() > LUT_CAPACITY {
            let new_lut = create_lut(rpc_client, signer_keypair, addresses_to_add)?;
            luts.push(new_lut);
            return Ok(());
        }

        lut.addresses = extend_lut(rpc_client, signer_keypair, lut.key, addresses_to_add)?;

        Ok(())
    }
}

fn create_lut(
    rpc_client: &RpcClient,
    signer_keypair: &Keypair,
    addresses: Vec<Pubkey>,
) -> anyhow::Result<AddressLookupTableAccount> {
    let recent_slot = rpc_client.get_slot()?;
    let (create_ix, lut_address) = address_lookup_table::instruction::create_lookup_table(
        signer_keypair.pubkey(),
        signer_keypair.pubkey(),
        recent_slot,
    );

    let recent_blockhash = rpc_client.get_latest_blockhash()?;
    let tx = solana_sdk::transaction::Transaction::new_signed_with_payer(
        &[create_ix],
        Some(&signer_keypair.pubkey()),
        &[signer_keypair],
        recent_blockhash,
    );

    rpc_client.send_and_confirm_transaction_with_spinner_and_config(
        &tx,
        CommitmentConfig::finalized(),
        RpcSendTransactionConfig {
            skip_preflight: true,
            ..Default::default()
        },
    )?;

    info!("Initialized new LUT: {:?}", lut_address);

    let updated_addresses = extend_lut(rpc_client, signer_keypair, lut_address, addresses)?;
    Ok(AddressLookupTableAccount {
        key: lut_address,
        addresses: updated_addresses.to_vec(),
    })
}

fn extend_lut(
    rpc_client: &RpcClient,
    signer_keypair: &Keypair,
    lut_address: Pubkey,
    addresses: Vec<Pubkey>,
) -> anyhow::Result<Vec<Pubkey>> {
    let ix = address_lookup_table::instruction::extend_lookup_table(
        lut_address,
        signer_keypair.pubkey(),
        Some(signer_keypair.pubkey()),
        addresses.clone(),
    );

    let recent_blockhash = rpc_client.get_latest_blockhash()?;
    let tx = solana_sdk::transaction::Transaction::new_signed_with_payer(
        &[ix],
        Some(&signer_keypair.pubkey()),
        &[signer_keypair],
        recent_blockhash,
    );

    rpc_client.send_and_confirm_transaction_with_spinner_and_config(
        &tx,
        CommitmentConfig::finalized(),
        RpcSendTransactionConfig {
            skip_preflight: true,
            ..Default::default()
        },
    )?;

    info!(
        "Extended LUT: {:?} with the new addresses: {:?}",
        lut_address, addresses
    );

    let lut_account = rpc_client.get_account(&lut_address)?;
    let lut = AddressLookupTable::deserialize(&lut_account.data).unwrap();

    Ok(lut.addresses.to_vec())
}

#[cfg(test)]
pub mod test_utils {
    use std::sync::{Arc, Mutex};

    use solana_sdk::{account::Account, clock::Clock, pubkey::Pubkey};

    use crate::wrappers::{bank::BankWrapper, oracle::test_utils::create_empty_oracle_account};

    use super::Cache;

    pub fn create_test_cache(bank_wrappers: &Vec<BankWrapper>) -> Cache {
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
        let mut cache = Cache::new(
            signer_pk,
            marginfi_program_id,
            marginfi_group_address,
            Arc::new(Mutex::new(Clock::default())),
        );
        let lut = AddressLookupTableAccount {
            key: Pubkey::new_unique(),
            addresses: vec![Pubkey::new_unique()],
        };

        assert_eq!(cache.luts.lock().unwrap().len(), 0);
        cache.add_lut(lut.clone());

        let luts = cache.luts.lock().unwrap();
        assert_eq!(cache.luts.lock().unwrap().len(), 1);
        assert_eq!(luts[0].key, lut.key);
    }
}
