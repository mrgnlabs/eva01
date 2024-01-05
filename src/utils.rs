use std::sync::Arc;

use backoff::ExponentialBackoff;
use solana_account_decoder::UiAccountEncoding;
use solana_client::{nonblocking::rpc_client::RpcClient, rpc_config::RpcAccountInfoConfig};
use solana_program::pubkey::Pubkey;
use solana_sdk::account::Account;

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
pub async fn batch_get_mutliple_accounts(
    rpc_client: Arc<RpcClient>,
    addresses: Vec<Pubkey>,
    max_batch_size: usize,
    max_concurrent_calls: usize,
) -> anyhow::Result<Vec<Option<Account>>> {
    let batched_addresses = addresses.chunks(max_batch_size * max_concurrent_calls);
    let total_addresses = addresses.len();
    let total_batches = batched_addresses.len();

    let mut accounts = Vec::new();
    let mut fetched_accounts = 0;

    for (batch_index, batch) in batched_addresses.enumerate() {
        let mut batched_accounts = Vec::new();
        let mut handles = Vec::new();
        let batch_size = batch.len();

        log::trace!(
            "Fetching batch {}/{} with {} addresses.",
            batch_index + 1,
            total_batches,
            batch_size
        );

        for chunk in batch.chunks(max_batch_size) {
            let rpc_client = rpc_client.clone();
            let chunk = chunk.to_vec();
            let chunk_size = chunk.len();

            log::trace!(" - Fetching chunk of size {}", chunk_size);

            let handle = backoff::future::retry(ExponentialBackoff::default(), move || {
                let rpc_client = rpc_client.clone();
                let chunk = chunk.clone();
                async move {
                    rpc_client
                        .get_multiple_accounts_with_config(
                            &chunk,
                            RpcAccountInfoConfig {
                                encoding: Some(UiAccountEncoding::Base64Zstd),
                                ..Default::default()
                            },
                        )
                        .await
                        .map_err(backoff::Error::transient)
                }
            });

            handles.push(handle);
        }

        for handle in handles {
            let mut batched_accounts_chunk = handle.await?.value;
            let fetched_chunk_size = batched_accounts_chunk.len();
            fetched_accounts += fetched_chunk_size;

            log::trace!(
                " - Fetched chunk with {} accounts. Progress: {}/{}",
                fetched_chunk_size,
                fetched_accounts,
                total_addresses
            );

            batched_accounts.append(&mut batched_accounts_chunk);
        }

        accounts.append(&mut batched_accounts);
    }

    log::debug!(
        "Finished fetching all accounts. Total accounts fetched: {}",
        fetched_accounts
    );

    Ok(accounts)
}
