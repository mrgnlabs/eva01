mod accounts;

use std::sync::{atomic::AtomicUsize, Arc};

use accounts::MarginfiAccountsCache;
use anchor_lang::Discriminator;
use backoff::ExponentialBackoff;
use log::info;
use marginfi::state::marginfi_account::MarginfiAccount;
use rayon::{iter::ParallelIterator, slice::ParallelSlice};
use solana_account_decoder::{UiAccountEncoding, UiDataSliceConfig};
use solana_client::{
    rpc_client::RpcClient,
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType},
};
use solana_sdk::{account::Account, bs58, pubkey::Pubkey};

use crate::{utils::BatchLoadingConfig, wrappers::marginfi_account::MarginfiAccountWrapper};

pub struct Cache {
    pub marginfi_program_id: Pubkey,
    pub marginfi_group_address: Pubkey,
    marginfi_accounts: MarginfiAccountsCache,
}

impl Cache {
    pub fn new(marginfi_program_id: Pubkey, marginfi_group_address: Pubkey) -> Self {
        let marginfi_accounts = MarginfiAccountsCache::new();

        Self {
            marginfi_program_id,
            marginfi_group_address,
            marginfi_accounts,
        }
    }

    pub fn get_marginfi_accounts(&self) -> &MarginfiAccountsCache {
        &self.marginfi_accounts
    }
}

pub struct CacheLoader {
    rpc_client: RpcClient,
}

impl CacheLoader {
    pub fn new(rpc_url: String) -> Self {
        let rpc_client = RpcClient::new(&rpc_url);

        Self { rpc_client }
    }

    pub fn load_marginfi_accounts(&self, cache: &mut Cache) -> anyhow::Result<()> {
        info!("Loading marginfi accounts, this may take a few minutes, please be patient!");
        let start = std::time::Instant::now();
        let marginfi_accounts_pubkeys = self.load_marginfi_account_addresses(
            &cache.marginfi_program_id,
            &cache.marginfi_group_address,
        )?;

        let mut marginfi_accounts = self.batch_get_multiple_accounts(
            &marginfi_accounts_pubkeys,
            BatchLoadingConfig {
                max_batch_size: 100,
                max_concurrent_calls: 32,
            },
        )?;

        info!("Fetched {} marginfi accounts", marginfi_accounts.len());

        for (address, account) in marginfi_accounts_pubkeys
            .iter()
            .zip(marginfi_accounts.iter_mut())
        {
            let account = account.as_ref().unwrap();
            let marginfi_account = bytemuck::from_bytes::<MarginfiAccount>(&account.data[8..]);
            let maw = MarginfiAccountWrapper {
                address: *address,
                lending_account: marginfi_account.lending_account,
            };
            cache.marginfi_accounts.insert(maw);
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
    /// Batch load accounts from the RPC client using the getMultipleAccounts RPC call.
    ///
    /// - `max_batch_size`: The maximum number of accounts to load in a single RPC call.
    /// - `max_concurrent_calls`: The maximum number of concurrent RPC calls.
    ///
    /// This function will perform multiple RPC calls concurrently, up to `max_concurrent_calls`.
    /// If the number of pending RPC calls exceeds `max_concurrent_calls`, the function will
    /// await until some calls complete before initiating more, to respect the concurrency limit.
    /// Additionally, logs progress information including the number of accounts being fetched,
    /// the size of each chunk, and the current progress using trace and debug logs.
    pub fn batch_get_multiple_accounts(
        &self,
        addresses: &[Pubkey],
        BatchLoadingConfig {
            max_batch_size,
            max_concurrent_calls,
        }: BatchLoadingConfig,
    ) -> anyhow::Result<Vec<Option<Account>>> {
        let batched_addresses = addresses.chunks(max_batch_size * max_concurrent_calls);
        let total_addresses = addresses.len();
        let total_batches = batched_addresses.len();

        let mut accounts = Vec::new();
        let fetched_accounts = Arc::new(AtomicUsize::new(0));

        for (batch_index, batch) in batched_addresses.enumerate() {
            let batch_size = batch.len();

            log::debug!(
                "Fetching batch {} / {} with {} addresses.",
                batch_index + 1,
                total_batches,
                batch_size
            );

            let mut batched_accounts = batch
                .par_chunks(max_batch_size)
                .map(|chunk| -> anyhow::Result<Vec<_>> {
                    let chunk = chunk.to_vec();
                    let chunk_size = chunk.len();

                    log::trace!(" - Fetching chunk of size {}", chunk_size);

                    let chunk_res = backoff::retry(ExponentialBackoff::default(), move || {
                        let chunk = chunk.clone();

                        //                        let rpc_client = RpcClient::new(&self.rpc_url);
                        self.rpc_client
                            .get_multiple_accounts_with_config(
                                &chunk,
                                RpcAccountInfoConfig {
                                    encoding: Some(UiAccountEncoding::Base64Zstd),
                                    ..Default::default()
                                },
                            )
                            .map_err(backoff::Error::transient)
                    })?
                    .value;

                    let fetched_chunk_size = chunk_res.len();

                    fetched_accounts
                        .fetch_add(fetched_chunk_size, std::sync::atomic::Ordering::Relaxed);

                    log::debug!(
                        " - Fetched chunk with {} accounts. Progress: {} / {}",
                        fetched_chunk_size,
                        fetched_accounts.load(std::sync::atomic::Ordering::Relaxed),
                        total_addresses
                    );

                    Ok(chunk_res)
                })
                .collect::<anyhow::Result<Vec<_>>>()?
                .iter()
                .flatten()
                .cloned()
                .collect::<Vec<_>>();

            accounts.append(&mut batched_accounts);
        }

        log::debug!(
            "Finished fetching all accounts. Total accounts fetched: {}",
            fetched_accounts.load(std::sync::atomic::Ordering::Relaxed)
        );

        Ok(accounts)
    }
}
