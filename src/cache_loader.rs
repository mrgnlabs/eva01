use std::{collections::HashMap, path::PathBuf, sync::Arc};

use anchor_lang::Discriminator;
use anchor_spl::associated_token;
use anyhow::Ok;
use marginfi::state::{marginfi_account::MarginfiAccount, marginfi_group::Bank};
use solana_account_decoder::{UiAccountEncoding, UiDataSliceConfig};
use solana_client::{
    rpc_client::RpcClient,
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType},
};
use solana_sdk::{
    account::Account,
    address_lookup_table::{state::AddressLookupTable, AddressLookupTableAccount},
    bs58,
    instruction::Instruction,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair},
    signer::Signer,
};

use crate::{
    cache::Cache,
    geyser::AccountType,
    sender::{SenderCfg, TransactionSender},
    thread_debug, thread_error, thread_info, thread_warn,
    utils::{batch_get_multiple_accounts, BatchLoadingConfig},
    wrappers::marginfi_account::MarginfiAccountWrapper,
};
use anchor_client::Program;
use anyhow::{anyhow, Result};
use rayon::iter::{IndexedParallelIterator, IntoParallelRefIterator, ParallelIterator};

const BANK_GROUP_PK_OFFSET: usize = 32 + 1 + 8;
const MAX_INIT_TA_IXS: usize = 10;

pub struct CacheLoader {
    signer: Keypair,
    rpc_url: String,
    rpc_client: RpcClient,
    lut_addresses: Vec<Pubkey>,
}

impl CacheLoader {
    pub fn new(keypair_path: PathBuf, rpc_url: String, lut_addresses: Vec<Pubkey>) -> Result<Self> {
        let signer =
            read_keypair_file(&keypair_path).map_err(|e| anyhow!("Keypair read failed: {}", e))?;
        let rpc_client = RpcClient::new(&rpc_url);

        Ok(Self {
            signer,
            rpc_url,
            rpc_client,
            lut_addresses,
        })
    }

    pub fn load_cache(
        &self,
        cache: &mut Cache,
        new_liquidator_account: &Option<Pubkey>,
    ) -> anyhow::Result<()> {
        self.load_luts(cache)?;
        self.load_marginfi_accounts(cache, new_liquidator_account)?;
        self.load_banks(cache)?;
        self.load_mints(cache)?;
        self.load_oracles(cache)?;
        self.load_tokens(cache)?;
        Ok(())
    }

    fn load_luts(&self, cache: &mut Cache) -> anyhow::Result<()> {
        thread_info!("Loading LUTs.");

        let lut_accounts = self.rpc_client.get_multiple_accounts(&self.lut_addresses)?;

        let luts: Vec<AddressLookupTableAccount> = self
            .lut_addresses
            .iter()
            .zip(lut_accounts.into_iter())
            .map(|(lut_address, lut_account_opt)| {
                let lut_account = lut_account_opt
                    .ok_or_else(|| anyhow!("Failed to find the {} LUT.", lut_address))?;
                let lut = AddressLookupTable::deserialize(&lut_account.data).map_err(|e| {
                    anyhow!("Failed to deserialize the {} LUT : {:?}", lut_address, e)
                })?;
                Ok(AddressLookupTableAccount {
                    key: *lut_address,
                    addresses: lut.addresses.to_vec(),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        luts.into_iter().for_each(|lut| cache.add_lut(lut));

        thread_info!("Loaded {} LUTs.", &cache.luts.len());

        Ok(())
    }

    fn load_marginfi_accounts(
        &self,
        cache: &mut Cache,
        new_liquidator_account: &Option<Pubkey>,
    ) -> anyhow::Result<()> {
        thread_info!("Loading marginfi accounts, this may take a few minutes, please be patient!");
        let start = std::time::Instant::now();
        let mut marginfi_accounts_pubkeys = self.load_marginfi_account_addresses(
            &cache.marginfi_program_id,
            &cache.marginfi_group_address,
        )?;
        if let Some(new_liquidator_account) = new_liquidator_account {
            thread_debug!("PUSHING NEW ONE");
            marginfi_accounts_pubkeys.push(*new_liquidator_account);
        }

        let marginfi_accounts = batch_get_multiple_accounts(
            &self.rpc_client,
            &marginfi_accounts_pubkeys,
            BatchLoadingConfig {
                max_batch_size: BatchLoadingConfig::DEFAULT.max_batch_size,
                max_concurrent_calls: 32,
            },
        )?;

        for (address, account_opt) in marginfi_accounts_pubkeys
            .iter()
            .zip(marginfi_accounts.iter())
        {
            if let Some(account) = account_opt {
                let marginfi_account = bytemuck::from_bytes::<MarginfiAccount>(&account.data[8..]);
                let maw = MarginfiAccountWrapper {
                    address: *address,
                    lending_account: marginfi_account.lending_account,
                };
                cache.marginfi_accounts.try_insert(maw)?;
            } else {
                thread_warn!("Couldn't load Marginfi account for key: {}", *address);
            }
        }

        thread_info!(
            "Loaded {} marginfi accounts in {:?}",
            marginfi_accounts.len(),
            start.elapsed()
        );

        Ok(())
    }

    fn load_marginfi_account_addresses(
        &self,
        marginfi_program_id: &Pubkey,
        marginfi_group_address: &Pubkey,
    ) -> anyhow::Result<Vec<Pubkey>> {
        thread_info!("Loading marginfi account addresses...");
        let marginfi_account_addresses = &self.rpc_client.get_program_accounts_with_config(
            marginfi_program_id,
            RpcProgramAccountsConfig {
                account_config: RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    data_slice: Some(UiDataSliceConfig {
                        offset: 0,
                        length: 0,
                    }),
                    ..Default::default()
                },
                filters: Some(vec![
                    RpcFilterType::Memcmp(Memcmp::new(
                        8,
                        MemcmpEncodedBytes::Base58(marginfi_group_address.to_string()),
                    )),
                    RpcFilterType::Memcmp(Memcmp::new(
                        0,
                        MemcmpEncodedBytes::Base58(
                            bs58::encode(MarginfiAccount::DISCRIMINATOR).into_string(),
                        ),
                    )),
                ]),
                with_context: Some(false),
                sort_results: None,
            },
        )?;

        let marginfi_account_pubkeys: Vec<Pubkey> = marginfi_account_addresses
            .iter()
            .map(|(pubkey, _)| *pubkey)
            .collect();

        thread_info!(
            "Loaded {} marginfi account addresses.",
            marginfi_account_pubkeys.len()
        );
        Ok(marginfi_account_pubkeys)
    }

    fn load_banks(&self, cache: &mut Cache) -> anyhow::Result<()> {
        let anchor_client = anchor_client::Client::new(
            anchor_client::Cluster::Custom(self.rpc_url.clone(), String::from("")),
            Arc::new(Keypair::new()),
        );

        let program: Program<Arc<Keypair>> = anchor_client.program(cache.marginfi_program_id)?;

        thread_info!("Loading banks...");
        for (bank_address, bank) in program
            .accounts::<Bank>(vec![RpcFilterType::Memcmp(Memcmp::new_base58_encoded(
                BANK_GROUP_PK_OFFSET,
                cache.marginfi_group_address.as_ref(),
            ))])?
            .iter()
        {
            cache.banks.insert(*bank_address, *bank);
            thread_debug!("Loaded the Bank {:?}.", bank_address);
        }

        thread_info!("Loaded {} banks", cache.banks.len());

        Ok(())
    }

    fn load_oracles(&self, cache: &mut Cache) -> anyhow::Result<()> {
        thread_info!("Loading Oracles...");

        let oracle_addresses = cache.banks.get_oracles().into_iter().collect::<Vec<_>>();
        let oracle_accounts = batch_get_multiple_accounts(
            &self.rpc_client,
            &oracle_addresses,
            BatchLoadingConfig::DEFAULT,
        )?;
        let oracle_map: HashMap<Pubkey, Account> = oracle_addresses
            .iter()
            .zip(oracle_accounts.iter())
            .filter_map(|(pk, account)| account.as_ref().map(|acc| (*pk, acc.clone())))
            .collect();

        for (oracle_address, oracle_account) in oracle_map.iter() {
            thread_debug!("Loaded the Oracle {:?} .", oracle_address);
            cache
                .oracles
                .try_insert(*oracle_address, oracle_account.clone())?;
        }

        thread_info!("Loaded {} Oracles.", cache.oracles.len()?);

        Ok(())
    }

    fn load_mints(&self, cache: &mut Cache) -> anyhow::Result<()> {
        thread_info!("Loading Mints");
        let mint_addresses = cache.banks.get_mints();
        let mint_accounts = batch_get_multiple_accounts(
            &self.rpc_client,
            &mint_addresses,
            BatchLoadingConfig::DEFAULT,
        )?;

        for (mint_address, mint_account_opt) in mint_addresses.iter().zip(mint_accounts.iter()) {
            if let Some(mint_account) = mint_account_opt {
                let token_address = associated_token::get_associated_token_address_with_program_id(
                    &cache.signer_pk,
                    mint_address,
                    &mint_account.owner,
                );

                cache
                    .mints
                    .insert(*mint_address, mint_account.clone(), token_address);
                thread_debug!("Loaded the Mint {:?}.", mint_address);
            } else {
                thread_error!("Mint account {:?} not found.", mint_address);
            }
        }

        thread_info!("Loaded {} mints.", cache.mints.len());
        Ok(())
    }

    fn load_tokens(&self, cache: &mut Cache) -> anyhow::Result<()> {
        thread_info!("Loading Token accounts...");

        let token_addresses = cache.mints.get_tokens();
        let token_accounts = batch_get_multiple_accounts(
            &self.rpc_client,
            &token_addresses,
            BatchLoadingConfig::DEFAULT,
        )?;

        let mut new_token_addresses: Vec<Pubkey> = vec![];
        for (token_address, token_account_opt) in token_addresses.iter().zip(token_accounts.iter())
        {
            match token_account_opt {
                Some(token_account) => {
                    let mint_address = cache.mints.try_get_mint_for_token(token_address)?;
                    cache
                        .tokens
                        .try_insert(*token_address, token_account.clone(), mint_address)?;
                    thread_debug!("Loaded the Token account {:?}.", token_address);
                }
                None => {
                    new_token_addresses.push(*token_address);
                }
            }
        }
        thread_info!("Loaded {}  Token accounts.", cache.tokens.len()?);

        if !new_token_addresses.is_empty() {
            thread_info!("Creating {} new Token accounts", new_token_addresses.len());

            let mut instructions: Vec<Instruction> = vec![];
            for token_address in &new_token_addresses {
                let mint_account = cache.mints.try_get_mint_for_token(token_address)?;
                let mint = cache.mints.try_get_account(&mint_account)?;
                let signer_pk = cache.signer_pk;
                let ix = spl_associated_token_account::instruction::create_associated_token_account_idempotent(&signer_pk, &signer_pk, &mint_account, &mint.account.owner);
                instructions.push(ix);
            }

            self.create_token_accounts(instructions)?;

            thread_info!("Fetching newly created Token accounts...");
            let new_token_accounts = batch_get_multiple_accounts(
                &self.rpc_client,
                &new_token_addresses,
                BatchLoadingConfig::DEFAULT,
            )?;
            for (new_token_address, new_token_account_opt) in
                new_token_addresses.iter().zip(new_token_accounts.iter())
            {
                if let Some(new_token_account) = new_token_account_opt {
                    let mint_address = cache.mints.try_get_mint_for_token(new_token_address)?;
                    cache.tokens.try_insert(
                        *new_token_address,
                        new_token_account.clone(),
                        mint_address,
                    )?;
                    thread_debug!(
                        "Fetched the newly created Token account {:?}.",
                        new_token_address
                    );
                }
            }
            thread_info!(
                "Fetched {} newly created Token accounts.",
                new_token_addresses.len()
            );
        }

        Ok(())
    }

    fn create_token_accounts(&self, instructions: Vec<Instruction>) -> Result<()> {
        let recent_blockhash = self.rpc_client.get_latest_blockhash()?;

        instructions
            .par_iter()
            .chunks(MAX_INIT_TA_IXS)
            .try_for_each(|chunk| {
                let ixs = chunk.iter().map(|ix| (*ix).clone()).collect::<Vec<_>>();

                let tx = solana_sdk::transaction::Transaction::new_signed_with_payer(
                    &ixs,
                    Some(&self.signer.pubkey()),
                    &[&self.signer],
                    recent_blockhash,
                );

                let sig = TransactionSender::aggressive_send_tx(
                    &self.rpc_client,
                    &tx,
                    SenderCfg::DEFAULT,
                )
                .map_err(|e| anyhow!("Failed to send transaction: {:?}", e))?;

                thread_info!(
                    "{} new Token accounts were successfully registered. Txn sig: {:?}",
                    chunk.len(),
                    sig
                );

                Ok(())
            })?;

        Ok(())
    }
}

pub fn get_accounts_to_track(cache: &Cache) -> Result<HashMap<Pubkey, AccountType>> {
    let mut accounts: HashMap<Pubkey, AccountType> = HashMap::new();

    for oracle_pk in cache.oracles.try_get_addresses()? {
        accounts.insert(oracle_pk, AccountType::Oracle);
    }

    for token in cache.mints.get_tokens() {
        accounts.insert(token, AccountType::Token);
    }

    Ok(accounts)
}
