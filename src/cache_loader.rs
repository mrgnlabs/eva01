use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anchor_lang::Discriminator;
use anchor_spl::associated_token;
use anyhow::{bail, Ok};
use marginfi::state::{
    marginfi_account::MarginfiAccount,
    marginfi_group::Bank,
    price::{OraclePriceFeedAdapter, OracleSetup, SwitchboardPullPriceFeed},
};
use solana_account_decoder::{UiAccountEncoding, UiDataSliceConfig};
use solana_client::{
    rpc_client::RpcClient,
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType},
};
use solana_sdk::{
    account::Account,
    account_info::IntoAccountInfo,
    bs58,
    clock::Clock,
    instruction::Instruction,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair},
    signer::Signer,
};
use switchboard_on_demand_client::PullFeedAccountData;

use crate::{
    cache::CacheT,
    clock_manager,
    geyser::AccountType,
    sender::{SenderCfg, TransactionSender},
    thread_debug, thread_error, thread_info,
    utils::{
        batch_get_multiple_accounts, find_oracle_keys, load_swb_pull_account_from_bytes,
        log_genuine_error, BatchLoadingConfig,
    },
    wrappers::{
        marginfi_account::MarginfiAccountWrapper,
        oracle::{price_adapter_name, OracleWrapperTrait},
    },
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
    clock: Arc<Mutex<Clock>>,
}

impl CacheLoader {
    pub fn new(keypair_path: PathBuf, rpc_url: String, clock: Arc<Mutex<Clock>>) -> Result<Self> {
        let signer =
            read_keypair_file(&keypair_path).map_err(|e| anyhow!("Keypair read failed: {}", e))?;
        let rpc_client = RpcClient::new(&rpc_url);

        Ok(Self {
            signer,
            rpc_url,
            rpc_client,
            clock,
        })
    }

    pub fn load_cache<T: OracleWrapperTrait + Clone>(
        &self,
        cache: &mut CacheT<T>,
    ) -> anyhow::Result<()> {
        self.load_marginfi_accounts(cache)?;
        self.load_banks(cache)?;
        self.load_mints(cache)?;
        self.load_oracles(cache)?;
        self.load_tokens(cache)?;
        Ok(())
    }

    fn load_marginfi_accounts<T: OracleWrapperTrait + Clone>(
        &self,
        cache: &mut CacheT<T>,
    ) -> anyhow::Result<()> {
        thread_info!("Loading marginfi accounts, this may take a few minutes, please be patient!");
        let start = std::time::Instant::now();
        let marginfi_accounts_pubkeys = self.load_marginfi_account_addresses(
            &cache.marginfi_program_id,
            &cache.marginfi_group_address,
        )?;

        let marginfi_accounts = batch_get_multiple_accounts(
            &self.rpc_client,
            &marginfi_accounts_pubkeys,
            BatchLoadingConfig {
                max_batch_size: BatchLoadingConfig::DEFAULT.max_batch_size,
                max_concurrent_calls: 32,
            },
        )?;

        thread_info!("Loaded {} marginfi accounts", marginfi_accounts.len());

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
            }
        }

        thread_info!("Loaded pubkeys in {:?}", start.elapsed());

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

    fn load_banks<T: OracleWrapperTrait + Clone>(
        &self,
        cache: &mut CacheT<T>,
    ) -> anyhow::Result<()> {
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

    fn load_oracles<T: OracleWrapperTrait + Clone>(
        &self,
        cache: &mut CacheT<T>,
    ) -> anyhow::Result<()> {
        thread_info!("Loading Oracles...");

        let oracle_addresses = cache.banks.get_oracles();
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

        for (bank_address, bank) in cache.banks.get_banks().iter() {
            thread_debug!(
                "Loading the {:?} Oracles for the Bank {:?} ...",
                bank.config.oracle_setup,
                bank_address
            );

            let oracle_keys_excluded_default = bank
                .config
                .oracle_keys
                .iter()
                .filter(|key| *key != &Pubkey::default())
                .collect::<Vec<_>>();

            if oracle_keys_excluded_default.len() > 1
                && bank.config.oracle_setup != OracleSetup::StakedWithPythPush
            {
                thread_error!(
                    "Discarding Oracles for the non-staked Bank {:?} because it has more than one oracle key, which is not supported.",
                    bank_address
                );
                continue;
            }

            // Use the first supported Oracle (for non-staked oracles only).
            let (oracle_address, oracle_account) = {
                let oracle_addresses = find_oracle_keys(&bank.config);
                let mut oracle_account = None;
                let mut oracle_address = None;

                for address in oracle_addresses.iter() {
                    if let Some(account) = oracle_map.get(address) {
                        oracle_account = Some(account.clone());
                        oracle_address = Some(*address);
                        break;
                    }
                }

                (oracle_address, oracle_account)
            };

            match (oracle_address, oracle_account) {
                (Some(oracle_address), Some(mut oracle_account)) => {
                    if cache.oracles.exists(&oracle_address) {
                        thread_debug!(
                            "The {} Oracle is already registered. Wiring it with the Bank {}.",
                            oracle_address,
                            bank_address
                        );
                        cache
                            .oracles
                            .try_wire_with_bank(&oracle_address, bank_address)?;
                        if let OracleSetup::StakedWithPythPush = bank.config.oracle_setup {
                        } else {
                            continue;
                        }
                    }

                    let price_adapter = CacheLoader::try_create_price_adapter(
                        cache,
                        bank_address,
                        bank,
                        oracle_address,
                        &mut oracle_account,
                        &oracle_map,
                        self.clock.clone(),
                    );

                    match price_adapter {
                        std::result::Result::Ok(adapter) => {
                            let adapter_name = price_adapter_name(&adapter);
                            let oracle = T::new(oracle_address, adapter);

                            cache.oracles.try_insert(
                                oracle_address,
                                oracle_account.clone(),
                                Some(oracle),
                                *bank_address,
                            )?;

                            thread_debug!(
                                "Added the {:?} Oracle {:?} for the Bank {:?} to Cache.",
                                adapter_name,
                                oracle_address,
                                bank_address
                            );
                        }
                        Err(error) => {
                            cache.oracles.try_insert(
                                oracle_address,
                                oracle_account.clone(),
                                None,
                                *bank_address,
                            )?;

                            log_genuine_error(
                                format!(
                                    "Failed to load Oracle {:?} for Bank {:?}",
                                    oracle_address, bank_address
                                )
                                .as_str(),
                                error,
                            );
                        }
                    };
                }
                (_, _) => {
                    thread_error!(
                        "Failed to bind Oracles to the Bank {:?} with config {:?}.",
                        bank_address,
                        bank.config
                    );
                    continue;
                }
            };
        }
        thread_info!("Loaded {} Oracles.", cache.oracles.len());

        Ok(())
    }

    fn try_create_price_adapter<T: OracleWrapperTrait + Clone>(
        cache: &mut CacheT<T>,
        bank_address: &Pubkey,
        bank: &Bank,
        oracle_address: Pubkey,
        oracle_account: &mut Account,
        oracle_map: &HashMap<Pubkey, Account>,
        clock: Arc<Mutex<Clock>>,
    ) -> Result<OraclePriceFeedAdapter> {
        match bank.config.oracle_setup {
            OracleSetup::SwitchboardPull => {
                let mut offsets_data = [0u8; std::mem::size_of::<PullFeedAccountData>()];
                offsets_data.copy_from_slice(
                    &oracle_account.data[8..std::mem::size_of::<PullFeedAccountData>() + 8],
                );
                let swb_feed = load_swb_pull_account_from_bytes(&offsets_data)?;

                Ok(OraclePriceFeedAdapter::SwitchboardPull(
                    SwitchboardPullPriceFeed {
                        feed: Box::new((&swb_feed).into()),
                    },
                ))
            }
            OracleSetup::PythPushOracle => {
                let oracle_account_info = (&oracle_address, oracle_account).into_account_info();
                OraclePriceFeedAdapter::try_from_bank_config(
                    &bank.config,
                    &[oracle_account_info],
                    &clock_manager::get_clock(&clock)?,
                )
                .map_err(Into::into)
            }
            OracleSetup::StakedWithPythPush => {
                let oracle_keys = find_oracle_keys(&bank.config);
                if oracle_keys.len() != 3 {
                    return Err(anyhow!(
                        "StakedWithPythPush setup requires exactly 3 oracle keys, but found {} for the Bank {:?}.",
                        oracle_keys.len(), bank_address
                    ));
                }

                let bank_oracle_address = *oracle_keys.first().unwrap();
                let mut bank_oracle =
                        oracle_map.get(&bank_oracle_address).ok_or_else(|| {
                            anyhow!(
                                "Failed to find the Oracle key {} in Oracles map for the StakedWithPythPush Bank {:?}.",
                                bank_oracle_address,
                                bank_address
                            )
                        })?.clone();
                cache.oracles.try_insert(
                    bank_oracle_address,
                    bank_oracle.clone(),
                    None,
                    *bank_address,
                )?;

                let lst_mint_oracle_address = *oracle_keys.get(1).unwrap();
                let mut lst_mint_oracle = oracle_map.get(&lst_mint_oracle_address).ok_or_else(|| {
                            anyhow!("Failed to find the Lst Mint Oracle key {} in Oracles map for the StakedWithPythPush Bank {:?}.",
                                lst_mint_oracle_address,
                                bank_address
                            )
                        })?.clone();
                cache.oracles.try_insert(
                    lst_mint_oracle_address,
                    lst_mint_oracle.clone(),
                    None,
                    *bank_address,
                )?;

                let sol_pool_oracle_address = *oracle_keys.get(2).unwrap();
                let mut sol_pool_oracle = oracle_map.get(&sol_pool_oracle_address).ok_or_else(|| {
                            anyhow!(
                                "Failed to find the SOL pool Oracle key {} the Oracles map for the StakedWithPythPush Bank {:?}.",
                                sol_pool_oracle_address,
                                bank_address
                            )
                        })?.clone();
                cache.oracles.try_insert(
                    sol_pool_oracle_address,
                    sol_pool_oracle.clone(),
                    None,
                    *bank_address,
                )?;

                // Convert Account to AccountInfo-compatible structure if needed
                let oracle_account_info =
                    (&bank_oracle_address, &mut bank_oracle).into_account_info();
                let lst_mint_account_info =
                    (&lst_mint_oracle_address, &mut lst_mint_oracle).into_account_info();
                let sol_pool_account_info =
                    (&sol_pool_oracle_address, &mut sol_pool_oracle).into_account_info();

                OraclePriceFeedAdapter::try_from_bank_config(
                    &bank.config,
                    &[
                        oracle_account_info,
                        lst_mint_account_info,
                        sol_pool_account_info,
                    ],
                    &clock_manager::get_clock(&clock)?,
                )
                .map_err(Into::into)
            }

            _ => bail!(
                "Unsupported oracle setup for the Bank {:?}! {:?}",
                bank_address,
                bank.config.oracle_setup
            ),
        }
    }

    fn load_mints<T: OracleWrapperTrait + Clone>(
        &self,
        cache: &mut CacheT<T>,
    ) -> anyhow::Result<()> {
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
            }
        }

        thread_info!("Loaded {} mints.", cache.mints.len());
        Ok(())
    }

    fn load_tokens<T: OracleWrapperTrait + Clone>(
        &self,
        cache: &mut CacheT<T>,
    ) -> anyhow::Result<()> {
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

pub fn get_accounts_to_track<T: OracleWrapperTrait + Clone>(
    cache: &CacheT<T>,
) -> HashMap<Pubkey, AccountType> {
    let mut accounts: HashMap<Pubkey, AccountType> = HashMap::new();

    for oracle_pk in cache.oracles.get_addresses() {
        accounts.insert(oracle_pk, AccountType::Oracle);
    }

    for token in cache.mints.get_tokens() {
        accounts.insert(token, AccountType::Token);
    }

    accounts
}
