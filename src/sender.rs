use std::{error::Error, sync::Arc};

use log::info;
use solana_client::rpc_client::{RpcClient, SerializableTransaction};
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::{signature::Signature, transaction::Transaction};

pub struct SenderCfg {
    spam_times: u64,
    skip_preflight: bool,
}

impl SenderCfg {
    pub const DEFAULT: SenderCfg = SenderCfg {
        spam_times: 12,
        skip_preflight: true,
    };
}

pub fn aggressive_send_tx(
    rpc: Arc<RpcClient>,
    transaction: Transaction,
    cfg: SenderCfg,
) -> Result<Signature, Box<dyn Error>> {
    let signature = transaction.get_signature().clone();

    info!("Sending transaction: {}", signature.to_string());

    if !cfg.skip_preflight {
        rpc.simulate_transaction(&transaction)?;
    }

    (0..cfg.spam_times).into_iter().try_for_each(|_| {
        rpc.send_transaction(&transaction)?;
        Ok::<_, Box<dyn Error>>(())
    })?;

    info!("Confirming transaction: {}", signature.to_string());

    rpc.confirm_transaction_with_commitment(&signature, CommitmentConfig::processed())?;

    Ok(signature)
}
