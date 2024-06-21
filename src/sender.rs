use crate::{
    config::GeneralConfig, wrappers::marginfi_account::TxConfig,
};
use log::{error, info};
use serde::Deserialize;
use solana_client::rpc_client::{RpcClient, SerializableTransaction};
use solana_client::rpc_config::RpcSimulateTransactionConfig;
use solana_sdk::signature::{read_keypair_file, Signature};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use std::time::Duration;
use std::{error::Error, sync::Arc};

#[derive(Debug, Deserialize)]
pub struct SenderCfg {
    #[serde(default = "SenderCfg::default_spam_times")]
    spam_times: u64,
    #[serde(default = "SenderCfg::default_skip_preflight")]
    skip_preflight: bool,
    #[serde(default = "SenderCfg::default_timeout")]
    timeout: Duration,
    #[serde(default = "SenderCfg::default_transaction_type")]
    transaction_type: TransactionType,
}

impl SenderCfg {
    pub const DEFAULT: SenderCfg = SenderCfg {
        spam_times: 12,
        skip_preflight: false,
        timeout: Duration::from_secs(45),
        transaction_type: TransactionType::Aggressive,
    };

    pub const PASSIVE: SenderCfg = SenderCfg {
        spam_times: 0, // In passive mode no transaction is spammed
        skip_preflight: false,
        timeout: Duration::from_secs(45),
        transaction_type: TransactionType::Passive,
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

    const fn default_transaction_type() -> TransactionType {
        TransactionType::Aggressive
    }
}

pub struct TransactionSender;

#[derive(Debug, Deserialize)]
pub enum TransactionType {
    Aggressive,
    Passive,
}

impl TransactionSender {
    pub fn send_ix(
        rpc_client: Arc<RpcClient>,
        ix: Instruction,
        signer: Arc<Keypair>,
        tx_config: Option<TxConfig>,
        cfg: SenderCfg,
    ) -> Result<Signature, Box<dyn Error>> {
        let recent_blockhash = rpc_client.get_latest_blockhash()?;

        let mut ixs = vec![ix];

        if let Some(config) = tx_config {
            let mut compute_budget_price_ix =
                ComputeBudgetInstruction::set_compute_unit_price(1000);

            if let Some(price) = config.compute_unit_price_micro_lamports {
                compute_budget_price_ix = ComputeBudgetInstruction::set_compute_unit_price(price);
            }

            ixs.push(compute_budget_price_ix);
        }

        let compute_budget_price_ix = ComputeBudgetInstruction::set_compute_unit_limit(500000);
        ixs.push(compute_budget_price_ix);

        let tx = Transaction::new_signed_with_payer(
            &ixs,
            Some(&signer.pubkey()),
            &[signer.as_ref()],
            recent_blockhash,
        );

        match cfg.transaction_type {
            TransactionType::Passive => Self::passive_send_tx(rpc_client, &tx, cfg),
            TransactionType::Aggressive => Self::passive_send_tx(rpc_client, &tx, cfg),
        }
    }

    pub fn passive_send_tx(
        rpc: Arc<RpcClient>,
        transaction: &impl SerializableTransaction,
        cfg: SenderCfg,
    ) -> Result<Signature, Box<dyn Error>> {
        let signature = *transaction.get_signature();

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

        rpc.send_transaction(transaction)?;

        let blockhash = transaction.get_recent_blockhash();

        rpc.confirm_transaction_with_spinner(&signature, blockhash, CommitmentConfig::confirmed())?;

        info!("Confirmed transaction: {}", signature.to_string());

        Ok(signature)
    }

    pub fn aggressive_send_tx(
        rpc: Arc<RpcClient>,
        transaction: &impl SerializableTransaction,
        cfg: SenderCfg,
    ) -> Result<Signature, Box<dyn Error>> {
        let signature = *transaction.get_signature();

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

        (0..cfg.spam_times).try_for_each(|_| {
            rpc.send_transaction(transaction)?;
            Ok::<_, Box<dyn Error>>(())
        })?;

        let blockhash = transaction.get_recent_blockhash();

        rpc.confirm_transaction_with_spinner(&signature, blockhash, CommitmentConfig::confirmed())?;

        info!("Confirmed transaction: {}", signature.to_string());

        Ok(signature)
    }
}
