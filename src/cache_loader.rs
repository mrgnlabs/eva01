use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anchor_lang::Discriminator;
use anchor_spl::associated_token;
use anyhow::{bail, Ok};
use log::{debug, error, info};
use marginfi::state::{
    marginfi_account::MarginfiAccount,
    marginfi_group::Bank,
    price::{
        OraclePriceFeedAdapter, OracleSetup, SwitchboardPullPriceFeed, SwitchboardV2PriceFeed,
    },
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
use tokio::runtime::{Builder, Runtime};

use crate::{
    cache::Cache,
    clock_manager,
    geyser::AccountType,
    sender::{SenderCfg, TransactionSender},
    utils::{
        batch_get_multiple_accounts, find_oracle_keys, load_swb_pull_account_from_bytes,
        BatchLoadingConfig,
    },
    ward,
    wrappers::{marginfi_account::MarginfiAccountWrapper, oracle::OracleWrapper},
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
    tokio_rt: Runtime,
    clock: Arc<Mutex<Clock>>,
}

impl CacheLoader {
    pub fn new(keypair_path: PathBuf, rpc_url: String, clock: Arc<Mutex<Clock>>) -> Result<Self> {
        let signer =
            read_keypair_file(&keypair_path).map_err(|e| anyhow!("Keypair read failed: {}", e))?;
        let rpc_client = RpcClient::new(&rpc_url);
        let tokio_rt = Builder::new_multi_thread()
            .thread_name("CacheLoader")
            .worker_threads(2)
            .enable_all()
            .build()?;

        Ok(Self {
            signer,
            rpc_url,
            rpc_client,
            tokio_rt,
            clock,
        })
    }

    pub fn load_cache(&self, cache: &mut Cache) -> anyhow::Result<()> {
        self.load_marginfi_accounts(cache)?;
        self.load_banks(cache)?;
        self.load_mints(cache)?;
        self.load_oracles(cache)?;
        self.load_tokens(cache)?;
        Ok(())
    }

    fn load_marginfi_accounts(&self, cache: &mut Cache) -> anyhow::Result<()> {
        info!("Loading marginfi accounts, this may take a few minutes, please be patient!");
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

        info!("Fetched {} marginfi accounts", marginfi_accounts.len());

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

        info!("Loaded pubkeys in {:?}", start.elapsed());

        Ok(())
    }

    fn load_marginfi_account_addresses(
        &self,
        marginfi_program_id: &Pubkey,
        marginfi_group_address: &Pubkey,
    ) -> anyhow::Result<Vec<Pubkey>> {
        info!("Loading marginfi account addresses...");
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
                    #[allow(deprecated)]
                    RpcFilterType::Memcmp(Memcmp {
                        offset: 8,
                        #[allow(deprecated)]
                        bytes: MemcmpEncodedBytes::Base58(marginfi_group_address.to_string()),
                        #[allow(deprecated)]
                        encoding: None,
                    }),
                    #[allow(deprecated)]
                    RpcFilterType::Memcmp(Memcmp {
                        offset: 0,
                        #[allow(deprecated)]
                        bytes: MemcmpEncodedBytes::Base58(
                            bs58::encode(MarginfiAccount::DISCRIMINATOR).into_string(),
                        ),
                        #[allow(deprecated)]
                        encoding: None,
                    }),
                ]),
                with_context: Some(false),
            },
        )?;

        let marginfi_account_pubkeys: Vec<Pubkey> = marginfi_account_addresses
            .iter()
            .map(|(pubkey, _)| *pubkey)
            .collect();

        info!(
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

        info!("Loading banks...");
        for (bank_address, bank) in self
            .tokio_rt
            .block_on(program.accounts::<Bank>(vec![RpcFilterType::Memcmp(
                Memcmp::new_base58_encoded(
                    BANK_GROUP_PK_OFFSET,
                    cache.marginfi_group_address.as_ref(),
                ),
            )]))?
            .iter()
        {
            cache.banks.insert(*bank_address, *bank);
        }

        info!("Fetched {} banks", cache.banks.len());

        Ok(())
    }

    fn load_oracles(&self, cache: &mut Cache) -> anyhow::Result<()> {
        info!("Loading oracles...");

        let oracle_keys = cache.banks.get_oracles();
        let oracle_accounts = self.rpc_client.get_multiple_accounts(&oracle_keys)?;
        let oracle_map: HashMap<Pubkey, Account> = oracle_keys
            .iter()
            .zip(oracle_accounts.iter())
            .filter_map(|(pk, account)| account.as_ref().map(|acc| (*pk, acc.clone())))
            .collect();

        for (bank_address, bank) in cache.banks.get_accounts().iter() {
            if bank.config.oracle_setup == OracleSetup::StakedWithPythPush {
                debug!("Loading STAKED bank: {:?}", bank_address);
            }

            let oracle_keys_excluded_default = bank
                .config
                .oracle_keys
                .iter()
                .filter(|key| *key != &Pubkey::default())
                .collect::<Vec<_>>();

            if oracle_keys_excluded_default.len() > 1
                && bank.config.oracle_setup != OracleSetup::StakedWithPythPush
            {
                error!(
                    "Discarding the Bank {:?} because has more than one oracle key, which is not supported.",
                    bank_address
                );
                continue;
            }

            // TODO: what is this for?
            let (oracle_address, mut oracle_account) = {
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

                (oracle_address.unwrap(), oracle_account.unwrap())
            };

            let price_adapter = match bank.config.oracle_setup {
                OracleSetup::SwitchboardPull => {
                    let mut offsets_data = [0u8; std::mem::size_of::<PullFeedAccountData>()];
                    offsets_data.copy_from_slice(
                        &oracle_account.data[8..std::mem::size_of::<PullFeedAccountData>() + 8],
                    );
                    let swb_feed = load_swb_pull_account_from_bytes(&offsets_data)?;

                    OraclePriceFeedAdapter::SwitchboardPull(SwitchboardPullPriceFeed {
                        feed: Box::new((&swb_feed).into()),
                    })
                }
                OracleSetup::SwitchboardV2 => {
                    let oracle_account_info =
                        (&oracle_address, &mut oracle_account).into_account_info();

                    OraclePriceFeedAdapter::SwitchboardV2(
                        SwitchboardV2PriceFeed::load_checked(&oracle_account_info, i64::MAX, 0)
                            .unwrap(),
                    )
                }
                OracleSetup::PythPushOracle => {
                    let oracle_account_info =
                        (&oracle_address, &mut oracle_account).into_account_info();
                    ward!(
                        OraclePriceFeedAdapter::try_from_bank_config(
                            &bank.config,
                            &[oracle_account_info],
                            &clock_manager::get_clock(&self.clock)?,
                        )
                        .ok(),
                        continue
                    )
                }
                OracleSetup::StakedWithPythPush => {
                    let keys = find_oracle_keys(&bank.config);

                    let mut oracle_account = oracle_map.get(keys.first().unwrap()).unwrap().clone();

                    let mut oracle_account_infos =
                        vec![(keys.first().unwrap(), &mut oracle_account).into_account_info()];

                    // Remove the first key from the list
                    // TODO: why is that?
                    let mut keys = keys.clone();
                    keys.remove(0);

                    let accounts_map = oracle_map
                        .iter()
                        .filter(|(key, _)| keys.contains(key))
                        .map(|(key, address)| (*key, address.clone()))
                        .collect::<Vec<_>>();

                    let mut accounts: Vec<_> = accounts_map
                        .iter()
                        .map(|(_, account)| account.clone())
                        .collect();

                    let remaining_account_infos: Vec<_> = accounts_map
                        .iter()
                        .zip(accounts.iter_mut())
                        .map(|((address, _), account)| (address, account).into_account_info())
                        .collect();

                    oracle_account_infos.extend(remaining_account_infos);

                    oracle_account_infos.sort_by_key(|info| {
                        let address = info.key;
                        keys.iter().position(|key| key == address)
                    });

                    ward!(
                        OraclePriceFeedAdapter::try_from_bank_config(
                            &bank.config,
                            &oracle_account_infos,
                            &clock_manager::get_clock(&self.clock)?,
                        )
                        .ok(),
                        continue
                    )
                }
                _ => {
                    bail!("Unknown oracle setup {:?}", bank.config.oracle_setup);
                }
            };
            let oracle = OracleWrapper::new(oracle_address, price_adapter);

            if bank.config.oracle_setup == OracleSetup::StakedWithPythPush {
                debug!(
                    "Loaded STAKED bank: {:?} with oracle: {:?}",
                    bank_address, oracle_address
                );
            }

            cache
                .oracles
                .try_insert(oracle_account, oracle, *bank_address)?;
        }

        Ok(())
    }

    fn load_mints(&self, cache: &mut Cache) -> anyhow::Result<()> {
        info!("Loading Mints");
        let mint_addresses = cache.banks.get_mints();
        let mint_accounts = self.rpc_client.get_multiple_accounts(&mint_addresses)?;

        for (mint_address, mint_account_opt) in mint_addresses.iter().zip(mint_accounts.iter()) {
            if let Some(mint_account) = mint_account_opt {
                let token_address = associated_token::get_associated_token_address_with_program_id(
                    &cache.signer_pk,
                    mint_address,
                    &mint_account.owner,
                );

                cache
                    .mints
                    .try_insert(*mint_address, mint_account.clone(), token_address)?;
            }
        }

        info!("Fetched {} mints.", cache.mints.len());
        Ok(())
    }

    fn load_tokens(&self, cache: &mut Cache) -> anyhow::Result<()> {
        info!("Fetching Token accounts...");

        let token_addresses = cache.mints.get_tokens();
        let token_accounts = self.rpc_client.get_multiple_accounts(&token_addresses)?;

        let mut new_token_addresses: Vec<Pubkey> = vec![];
        for (token_address, token_account_opt) in token_addresses.iter().zip(token_accounts.iter())
        {
            match token_account_opt {
                Some(token_account) => {
                    let mint_address = cache.mints.try_get_mint_for_token(token_address)?;
                    cache
                        .tokens
                        .try_insert(*token_address, token_account.clone(), mint_address)?;
                }
                None => {
                    new_token_addresses.push(*token_address);
                }
            }
        }
        info!("Fetched {}  Token accounts.", cache.tokens.len()?);

        if !new_token_addresses.is_empty() {
            info!("Creating {} new Token accounts", new_token_addresses.len());

            let mut instructions: Vec<Instruction> = vec![];
            for token_address in &new_token_addresses {
                let mint_account = cache.mints.try_get_mint_for_token(token_address)?;
                let mint = cache.mints.try_get_account(&mint_account)?;
                let signer_pk = cache.signer_pk;
                let ix = spl_associated_token_account::instruction::create_associated_token_account_idempotent(&signer_pk, &signer_pk, &mint_account, &mint.account.owner);
                instructions.push(ix);
            }

            self.create_token_accounts(instructions)?;

            info!("Fetching newly created Token accounts...");
            let new_token_accounts = self
                .rpc_client
                .get_multiple_accounts(&new_token_addresses)?;
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
                }
            }
            info!(
                "Fetched {} newly created Token accounts.",
                new_token_addresses.len()
            );
        }

        Ok(())
    }

    fn create_token_accounts(&self, instructions: Vec<Instruction>) -> Result<()> {
        info!("Registering {} Token accounts...", instructions.len());

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

                info!(
                    "{} new Token accounts were successfully registered. Txn sig: {:?}",
                    instructions.len(),
                    sig
                );

                Ok(())
            })?;

        Ok(())
    }

    pub fn get_accounts_to_track(&self, cache: &Cache) -> HashMap<Pubkey, AccountType> {
        let mut accounts: HashMap<Pubkey, AccountType> = HashMap::new();

        for oracle_pk in cache.oracles.get_addresses() {
            accounts.insert(oracle_pk, AccountType::Oracle);
        }

        for token in cache.mints.get_tokens() {
            accounts.insert(token, AccountType::Token);
        }

        accounts
    }
}
