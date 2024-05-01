use std::thread::sleep;
use std::time::{Duration, Instant};
use std::{error::Error, sync::Arc};

use log::{error, info, trace};
use solana_client::rpc_client::{RpcClient, SerializableTransaction};
use solana_client::rpc_config::RpcTransactionConfig;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::transaction::VersionedTransaction;
use solana_sdk::{signature::Signature, transaction::Transaction};

pub struct SenderCfg {
    spam_times: u64,
    skip_preflight: bool,
    timeout: Duration,
}

impl SenderCfg {
    pub const DEFAULT: SenderCfg = SenderCfg {
        spam_times: 12,
        skip_preflight: false,
        timeout: Duration::from_secs(30),
    };
}

pub fn aggressive_send_tx(
    rpc: Arc<RpcClient>,
    transaction: &impl SerializableTransaction,
    cfg: SenderCfg,
) -> Result<Signature, Box<dyn Error>> {
    let signature = transaction.get_signature().clone();

    info!("Sending transaction: {}", signature.to_string());

    if !cfg.skip_preflight {
        let res = rpc.simulate_transaction(transaction)?;

        if res.value.err.is_some() {
            error!("Failed to simulate transaction: {:#?}", res.value);
            return Err("Transaction simulation failed".into());
        }
    }

    (0..cfg.spam_times).into_iter().try_for_each(|_| {
        rpc.send_transaction(transaction)?;
        Ok::<_, Box<dyn Error>>(())
    })?;

    let start = Instant::now();

    let mut confirmed = false;
    while start.elapsed() < cfg.timeout {
        let res =
            rpc.get_signature_status_with_commitment(&signature, CommitmentConfig::confirmed())?;

        trace!("Transaction status: {:?} {:#?}", start.elapsed(), res);

        match res {
            Some(Ok(_)) => {
                confirmed = true;
                break;
            }
            Some(Err(e)) => {
                error!("Transaction failed: {:#?}", e);

                return Err("Transaction failed".into());
            }
            _ => {}
        }

        sleep(Duration::from_secs(2));
    }

    if !confirmed {
        return Err("Transaction not confirmed".into());
    }

    info!("Confirmed transaction: {}", signature.to_string());

    Ok(signature)
}
