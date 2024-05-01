use std::thread::sleep;
use std::time::{Duration, Instant};
use std::{error::Error, sync::Arc};

use log::{error, info, trace};
use serde::Deserialize;
use solana_client::rpc_client::{RpcClient, SerializableTransaction};
use solana_client::rpc_config::{RpcSimulateTransactionConfig, RpcTransactionConfig};
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::transaction::VersionedTransaction;
use solana_sdk::{signature::Signature, transaction::Transaction};

#[derive(Debug, Deserialize)]
pub struct SenderCfg {
    #[serde(default = "SenderCfg::default_spam_times")]
    spam_times: u64,
    #[serde(default = "SenderCfg::default_skip_preflight")]
    skip_preflight: bool,
    #[serde(default = "SenderCfg::default_timeout")]
    timeout: Duration,
}

impl SenderCfg {
    pub const DEFAULT: SenderCfg = SenderCfg {
        spam_times: 12,
        skip_preflight: false,
        timeout: Duration::from_secs(45),
    };

    pub const fn default_spam_times() -> u64 {
        Self::DEFAULT.spam_times
    }

    pub const fn default_skip_preflight() -> bool {
        Self::DEFAULT.skip_preflight
    }

    const fn default_timeout() -> Duration {
        Self::DEFAULT.timeout
    }
}

pub fn aggressive_send_tx(
    rpc: Arc<RpcClient>,
    transaction: &impl SerializableTransaction,
    cfg: SenderCfg,
) -> Result<Signature, Box<dyn Error>> {
    let signature = transaction.get_signature().clone();

    info!("Sending transaction: {}", signature.to_string());

    if !cfg.skip_preflight {
        let res = rpc.simulate_transaction_with_config(
            transaction,
            RpcSimulateTransactionConfig {
                commitment: Some(CommitmentConfig::processed()),
                ..Default::default()
            },
        )?;

        if res.value.err.is_some() {
            error!("Failed to simulate transaction: {:#?}", res.value);
            return Err("Transaction simulation failed".into());
        }
    }

    (0..cfg.spam_times).into_iter().try_for_each(|_| {
        rpc.send_transaction(transaction)?;
        Ok::<_, Box<dyn Error>>(())
    })?;

    let blockhash = transaction.get_recent_blockhash();

    rpc.confirm_transaction_with_spinner(&signature, blockhash, CommitmentConfig::confirmed())?;

    info!("Confirmed transaction: {}", signature.to_string());

    Ok(signature)
}
