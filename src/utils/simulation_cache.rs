use anyhow::{anyhow, Context, Result};
use log::{debug, warn};
use solana_account_decoder::{UiAccount, UiAccountEncoding};
use solana_client::{
    client_error::{ClientError, ClientErrorKind},
    rpc_client::RpcClient,
    rpc_config::{RpcSimulateTransactionAccountsConfig, RpcSimulateTransactionConfig},
    rpc_request::RpcError,
};
use solana_sdk::{
    account::Account,
    address_lookup_table::AddressLookupTableAccount,
    commitment_config::CommitmentConfig,
    instruction::Instruction,
    message::{v0::Message, VersionedMessage},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    transaction::{TransactionError, VersionedTransaction},
};
use std::{thread, time::Duration};

const SIMULATION_LOG_LINE_LIMIT: usize = 40;
const SIMULATION_LOG_CHAR_LIMIT: usize = 8_000;

#[derive(Clone, Debug)]
pub struct SimulatedInstructionEntry {
    pub kind: &'static str,
    pub address: Pubkey,
    pub instruction: Instruction,
}

#[derive(Debug, Clone)]
pub struct SimulatedBatchRunSummary {
    pub skipped_entries: Vec<SimulatedInstructionEntry>,
    pub refreshed_batches: usize,
    pub preferred_batch_size: usize,
    pub resized_down: bool,
}

enum SimulateBatchError {
    TooLarge(anyhow::Error),
    TooManyAccountLocks(anyhow::Error),
    Other(anyhow::Error),
}

pub fn decode_and_apply_simulated_accounts<F>(
    addresses: &[Pubkey],
    simulated_accounts: &[Option<UiAccount>],
    source: &str,
    mut apply: F,
) -> Result<()>
where
    F: FnMut(&Pubkey, Account) -> Result<()>,
{
    if simulated_accounts.len() != addresses.len() {
        return Err(anyhow!(
            "{} returned {} accounts, expected {}",
            source,
            simulated_accounts.len(),
            addresses.len()
        ));
    }

    for (address, ui_account_opt) in addresses.iter().zip(simulated_accounts.iter()) {
        let Some(ui_account) = ui_account_opt else {
            return Err(anyhow!("{} returned null account for {}", source, address));
        };

        let account = ui_account
            .decode::<Account>()
            .ok_or_else(|| anyhow!("Failed to decode simulated account for {}", address))?;

        apply(address, account)?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn simulate_instruction_batches<F>(
    rpc_client: &RpcClient,
    signer: &Keypair,
    luts: &[AddressLookupTableAccount],
    cu_limit_ix: &Instruction,
    entries: &[SimulatedInstructionEntry],
    initial_batch_size: usize,
    max_rpc_retries: usize,
    rpc_retry_base_delay: Duration,
    mut apply: F,
) -> Result<SimulatedBatchRunSummary>
where
    F: FnMut(&Pubkey, Account) -> Result<()>,
{
    if entries.is_empty() {
        return Ok(SimulatedBatchRunSummary {
            skipped_entries: vec![],
            refreshed_batches: 0,
            preferred_batch_size: 1,
            resized_down: false,
        });
    }

    let mut skipped_entries: Vec<SimulatedInstructionEntry> = vec![];
    let mut refreshed_batches = 0usize;
    let mut offset = 0usize;
    let mut preferred_batch_size = initial_batch_size.min(entries.len()).max(1);
    let mut resized_down = false;

    while offset < entries.len() {
        let remaining = entries.len() - offset;
        let mut batch_size = preferred_batch_size.min(remaining).max(1);

        loop {
            let batch_entries = entries[offset..offset + batch_size].to_vec();
            match simulate_instruction_batch(
                rpc_client,
                signer,
                luts,
                cu_limit_ix,
                batch_entries,
                max_rpc_retries,
                rpc_retry_base_delay,
                &mut skipped_entries,
                &mut apply,
            ) {
                Ok(batch_refreshed_any) => {
                    if batch_refreshed_any {
                        refreshed_batches += 1;
                    }
                    offset += batch_size;
                    break;
                }
                Err(SimulateBatchError::TooLarge(_err)) if batch_size > 1 => {
                    let new_batch_size = (batch_size / 2).max(1);
                    debug!(
                        "Integrations refresh simulation tx too large with {} instructions; retrying with {} instructions",
                        batch_size, new_batch_size
                    );
                    batch_size = new_batch_size;
                    preferred_batch_size = new_batch_size;
                    resized_down = true;
                }
                Err(SimulateBatchError::TooManyAccountLocks(_err)) if batch_size > 1 => {
                    let new_batch_size = (batch_size / 2).max(1);
                    debug!(
                        "Integrations refresh simulation hit TooManyAccountLocks with {} instructions; retrying with {} instructions",
                        batch_size, new_batch_size
                    );
                    batch_size = new_batch_size;
                    preferred_batch_size = new_batch_size;
                    resized_down = true;
                }
                Err(SimulateBatchError::TooLarge(err)) => {
                    let failed_entry = entries[offset].clone();
                    warn!(
                        "Skipping integration refresh instruction {} for {} because tx is too large even as a single instruction: {}",
                        failed_entry.kind,
                        failed_entry.address,
                        err
                    );
                    skipped_entries.push(failed_entry);
                    offset += 1;
                    break;
                }
                Err(SimulateBatchError::TooManyAccountLocks(err)) => {
                    let failed_entry = entries[offset].clone();
                    warn!(
                        "Skipping integration refresh instruction {} for {} because TooManyAccountLocks persisted even as a single instruction: {}",
                        failed_entry.kind,
                        failed_entry.address,
                        err
                    );
                    skipped_entries.push(failed_entry);
                    offset += 1;
                    break;
                }
                Err(SimulateBatchError::Other(err)) => {
                    let batch_summary =
                        format_instruction_entries(&entries[offset..offset + batch_size], 10);
                    return Err(err).with_context(|| {
                        format!(
                            "Integrations refresh simulation failed for batch [{}..{}) (size {}, entries: {})",
                            offset,
                            offset + batch_size,
                            batch_size,
                            batch_summary,
                        )
                    });
                }
            }
        }
    }

    if refreshed_batches == 0 {
        return Err(anyhow!(
            "Integrations refresh simulation failed for all integrations; skipped entries: {}",
            format_skipped_instruction_entries(&skipped_entries)
        ));
    }

    Ok(SimulatedBatchRunSummary {
        skipped_entries,
        refreshed_batches,
        preferred_batch_size,
        resized_down,
    })
}

#[allow(clippy::too_many_arguments)]
fn simulate_instruction_batch<F>(
    rpc_client: &RpcClient,
    signer: &Keypair,
    luts: &[AddressLookupTableAccount],
    cu_limit_ix: &Instruction,
    batch_entries: Vec<SimulatedInstructionEntry>,
    max_rpc_retries: usize,
    rpc_retry_base_delay: Duration,
    skipped_entries: &mut Vec<SimulatedInstructionEntry>,
    apply: &mut F,
) -> std::result::Result<bool, SimulateBatchError>
where
    F: FnMut(&Pubkey, Account) -> Result<()>,
{
    let mut entries = batch_entries;

    loop {
        if entries.is_empty() {
            return Ok(false);
        }

        let integration_addresses: Vec<Pubkey> =
            entries.iter().map(|entry| entry.address).collect();
        let entry_summary = format_instruction_entries(&entries, 10);
        let mut ixs: Vec<Instruction> = Vec::with_capacity(entries.len() + 1);
        ixs.push(cu_limit_ix.clone());
        ixs.extend(entries.iter().map(|entry| entry.instruction.clone()));

        let recent_blockhash = retry_transient_rpc(
            "getLatestBlockhash for integrations refresh simulation",
            max_rpc_retries,
            rpc_retry_base_delay,
            || rpc_client.get_latest_blockhash(),
        )
        .map_err(|err| SimulateBatchError::Other(err.into()))?;

        let signer_pk = signer.pubkey();
        let msg = Message::try_compile(&signer_pk, &ixs, luts, recent_blockhash)
            .map_err(|err| SimulateBatchError::Other(err.into()))?;
        let tx = VersionedTransaction::try_new(VersionedMessage::V0(msg), &[signer])
            .map_err(|err| SimulateBatchError::Other(err.into()))?;

        let simulation = retry_transient_rpc(
            "simulateTransaction for integrations refresh simulation",
            max_rpc_retries,
            rpc_retry_base_delay,
            || {
                rpc_client.simulate_transaction_with_config(
                    &tx,
                    RpcSimulateTransactionConfig {
                        sig_verify: false,
                        replace_recent_blockhash: true,
                        commitment: Some(CommitmentConfig::confirmed()),
                        accounts: Some(RpcSimulateTransactionAccountsConfig {
                            encoding: Some(UiAccountEncoding::Base64),
                            addresses: integration_addresses
                                .iter()
                                .map(|pk| pk.to_string())
                                .collect(),
                        }),
                        ..Default::default()
                    },
                )
            },
        )
        .map_err(|err| {
            if is_tx_too_large_client(&err) {
                SimulateBatchError::TooLarge(err.into())
            } else if is_tx_too_many_account_locks_client(&err) {
                SimulateBatchError::TooManyAccountLocks(err.into())
            } else {
                SimulateBatchError::Other(err.into())
            }
        })?;

        if let Some(err) = simulation.value.err.clone() {
            if matches!(err, TransactionError::TooManyAccountLocks) {
                let logs = format_simulation_logs(
                    simulation.value.logs.as_deref(),
                    SIMULATION_LOG_LINE_LIMIT,
                    SIMULATION_LOG_CHAR_LIMIT,
                );
                return Err(SimulateBatchError::TooManyAccountLocks(anyhow!(
                    "Integrations refresh simulation hit TooManyAccountLocks for batch of {} entries [{}]; logs={}",
                    entries.len(),
                    entry_summary,
                    logs
                )));
            }

            if let TransactionError::InstructionError(ix_index, instruction_error) = &err {
                let ix_index = usize::from(*ix_index);
                if ix_index > 0 && ix_index <= entries.len() {
                    let failed_entry = entries.remove(ix_index - 1);
                    warn!(
                        "Skipping failing integration refresh instruction {} for {} (tx instruction index {}, error {:?})",
                        failed_entry.kind,
                        failed_entry.address,
                        ix_index,
                        instruction_error
                    );
                    skipped_entries.push(failed_entry);
                    continue;
                }
            }

            let logs = format_simulation_logs(
                simulation.value.logs.as_deref(),
                SIMULATION_LOG_LINE_LIMIT,
                SIMULATION_LOG_CHAR_LIMIT,
            );
            return Err(SimulateBatchError::Other(anyhow!(
                "Integrations refresh simulation failed with transaction error: {:?}; batch_size={}; entries=[{}]; logs={}",
                err,
                entries.len(),
                entry_summary,
                logs
            )));
        }

        let simulated_accounts = simulation.value.accounts.ok_or_else(|| {
            SimulateBatchError::Other(anyhow!(
                "Integrations refresh simulation did not return post-simulation accounts"
            ))
        })?;

        if simulated_accounts.len() != integration_addresses.len() {
            return Err(SimulateBatchError::Other(anyhow!(
                "Integrations refresh simulation returned {} accounts, expected {}",
                simulated_accounts.len(),
                integration_addresses.len()
            )));
        }

        decode_and_apply_simulated_accounts(
            &integration_addresses,
            &simulated_accounts,
            "simulateTransaction integrations refresh",
            |address, account| apply(address, account),
        )
        .map_err(SimulateBatchError::Other)?;

        return Ok(true);
    }
}

fn retry_transient_rpc<T, F>(
    operation_name: &str,
    max_retries: usize,
    retry_base_delay: Duration,
    mut operation: F,
) -> std::result::Result<T, ClientError>
where
    F: FnMut() -> std::result::Result<T, ClientError>,
{
    let max_attempts = max_retries.max(1);
    let mut attempt: usize = 1;
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(err) => {
                if attempt >= max_attempts || !is_transient_rpc_client_error(&err) {
                    return Err(err);
                }

                let multiplier = 1_u32.checked_shl((attempt - 1) as u32).unwrap_or(u32::MAX);
                let backoff_duration = retry_base_delay.saturating_mul(multiplier);
                warn!(
                    "Transient RPC error during {} (attempt {}/{}): {}. Retrying in {:?}",
                    operation_name, attempt, max_attempts, err, backoff_duration
                );
                thread::sleep(backoff_duration);
                attempt += 1;
            }
        }
    }
}

pub fn format_skipped_instruction_entries(entries: &[SimulatedInstructionEntry]) -> String {
    if entries.is_empty() {
        return "none".to_string();
    }

    entries
        .iter()
        .map(|entry| format!("{}:{}", entry.kind, entry.address))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn format_instruction_entries(
    entries: &[SimulatedInstructionEntry],
    max_entries: usize,
) -> String {
    if entries.is_empty() {
        return "none".to_string();
    }

    let mut out = entries
        .iter()
        .take(max_entries)
        .map(|entry| format!("{}:{}", entry.kind, entry.address))
        .collect::<Vec<_>>();

    if entries.len() > max_entries {
        out.push(format!(
            "...<{} additional entries>",
            entries.len() - max_entries
        ));
    }

    out.join(", ")
}

fn format_simulation_logs(logs: Option<&[String]>, max_lines: usize, max_chars: usize) -> String {
    let Some(logs) = logs else {
        return "none".to_string();
    };
    if logs.is_empty() {
        return "none".to_string();
    }

    let mut lines: Vec<String> = logs.iter().take(max_lines).cloned().collect();
    if logs.len() > max_lines {
        lines.push(format!(
            "...<{} additional log lines truncated>",
            logs.len() - max_lines
        ));
    }

    let mut joined = lines.join(" | ");
    if joined.len() > max_chars {
        joined.truncate(max_chars);
        joined.push_str("...<truncated>");
    }

    joined
}

pub fn is_tx_too_large_client(err: &ClientError) -> bool {
    match err.kind() {
        ClientErrorKind::RpcError(rpc) => match rpc {
            RpcError::RpcResponseError { code, message, .. } => {
                *code == -32602 && message.contains("too large")
            }
            RpcError::RpcRequestError(msg) | RpcError::ForUser(msg) => {
                // Some nodes may proxy this as a plain string
                msg.contains("too large")
            }
            _ => false,
        },
        _ => false,
    }
}

fn is_tx_too_many_account_locks_client(err: &ClientError) -> bool {
    match err.kind() {
        ClientErrorKind::RpcError(rpc) => match rpc {
            RpcError::RpcResponseError { message, .. } => {
                message.contains("TooManyAccountLocks")
                    || message
                        .to_ascii_lowercase()
                        .contains("too many account locks")
            }
            RpcError::RpcRequestError(msg) | RpcError::ForUser(msg) => {
                msg.contains("TooManyAccountLocks")
                    || msg.to_ascii_lowercase().contains("too many account locks")
            }
            _ => false,
        },
        _ => false,
    }
}

pub fn is_transient_rpc_anyhow_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        if let Some(client_error) = cause.downcast_ref::<ClientError>() {
            return is_transient_rpc_client_error(client_error);
        }

        is_transient_rpc_message(&cause.to_string())
    })
}

fn is_transient_rpc_client_error(err: &ClientError) -> bool {
    match err.kind() {
        ClientErrorKind::Io(io_err) => matches!(
            io_err.kind(),
            std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::WouldBlock
                | std::io::ErrorKind::Interrupted
        ),
        ClientErrorKind::RpcError(rpc_err) => match rpc_err {
            RpcError::RpcResponseError { code, message, .. } => {
                matches!(*code, 408 | 429 | 500 | 502 | 503 | 504 | -32005)
                    || is_transient_rpc_message(message)
            }
            RpcError::RpcRequestError(message) | RpcError::ForUser(message) => {
                is_transient_rpc_message(message)
            }
            RpcError::ParseError(message) => is_transient_rpc_message(message),
        },
        _ => is_transient_rpc_message(&err.to_string()),
    }
}

fn is_transient_rpc_message(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    normalized.contains("connection closed before message completed")
        || normalized.contains("operation timed out")
        || normalized.contains("request timeout")
        || normalized.contains("408 request timeout")
        || normalized.contains("429 too many requests")
        || normalized.contains("502 bad gateway")
        || normalized.contains("503 service unavailable")
        || normalized.contains("504 gateway timeout")
        || normalized.contains("temporarily unavailable")
        || normalized.contains("connection reset")
        || normalized.contains("broken pipe")
        || normalized.contains("deadline has elapsed")
        || normalized.contains("timeout")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_transient_rpc_anyhow_error_true_for_timeout_io() {
        let err = ClientError {
            request: None,
            kind: ClientErrorKind::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "operation timed out",
            )),
        };
        let anyhow_err = anyhow!(err);
        assert!(is_transient_rpc_anyhow_error(&anyhow_err));
    }

    #[test]
    fn test_is_transient_rpc_anyhow_error_false_for_non_transient() {
        let err = ClientError {
            request: None,
            kind: ClientErrorKind::Custom("deterministic failure".to_string()),
        };
        let anyhow_err = anyhow!(err);
        assert!(!is_transient_rpc_anyhow_error(&anyhow_err));
    }
}
