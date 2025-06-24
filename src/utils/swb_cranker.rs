#![allow(dead_code)]

use crate::config::GeneralConfig;
use anyhow::Result;
use solana_client::{
    nonblocking::rpc_client::RpcClient as NonBlockingRpcClient, rpc_client::RpcClient,
    rpc_config::RpcSendTransactionConfig,
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::InstructionError,
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    transaction::{TransactionError, VersionedTransaction},
};
use std::str::FromStr;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use switchboard_on_demand_client::{
    FetchUpdateManyParams, Gateway, PullFeed, QueueAccountData, SbContext,
};
use tokio::runtime::{Builder, Runtime};

use solana_client::client_error::ClientError;
use solana_client::client_error::ClientErrorKind;

//TODO: parametrize the Swb Program ID.
pub const SWB_PROGRAM_ID: &str = "A43DyUGA7s8eXPxqEjJY6EBu1KKbNgfxF8h17VAHn13w";
pub const SWB_STALE_PRICE_ERROR_CODE: u32 = 6049;

struct ResetFlag {
    flag: Arc<AtomicBool>,
}

impl Drop for ResetFlag {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::SeqCst);
    }
}

pub struct SwbCranker {
    tokio_rt: Runtime,
    rpc_client: RpcClient,
    non_blocking_rpc_client: NonBlockingRpcClient,
    swb_gateway: Gateway,
    payer: Keypair,
}

impl SwbCranker {
    pub fn new(config: &GeneralConfig) -> Result<Self> {
        let payer = Keypair::from_bytes(&config.wallet_keypair)?;

        let tokio_rt = Builder::new_multi_thread()
            .thread_name("SwbCranker")
            .worker_threads(2)
            .enable_all()
            .build()?;

        let rpc_client = RpcClient::new(config.rpc_url.clone());
        let non_blocking_rpc_client = NonBlockingRpcClient::new(config.rpc_url.clone());
        let queue = tokio_rt.block_on(QueueAccountData::load(
            &non_blocking_rpc_client,
            &Pubkey::from_str(SWB_PROGRAM_ID).unwrap(),
        ))?;
        let swb_gateway =
            tokio_rt.block_on(queue.fetch_gateways(&non_blocking_rpc_client))?[0].clone();

        Ok(Self {
            tokio_rt,
            rpc_client,
            non_blocking_rpc_client,
            swb_gateway,
            payer,
        })
    }

    pub fn crank_oracles(&self, swb_oracles: Vec<Pubkey>) -> Result<()> {
        let (crank_ix, crank_lut) = self.tokio_rt.block_on(PullFeed::fetch_update_consensus_ix(
            SbContext::new(),
            &self.non_blocking_rpc_client,
            FetchUpdateManyParams {
                feeds: swb_oracles,
                payer: self.payer.pubkey(),
                gateway: self.swb_gateway.clone(),
                num_signatures: Some(1),
                //                    debug: Some(true),
                ..Default::default()
            },
        ))?;

        let blockhash = self.rpc_client.get_latest_blockhash()?;

        let txn = VersionedTransaction::try_new(
            VersionedMessage::V0(v0::Message::try_compile(
                &self.payer.pubkey(),
                &crank_ix,
                &crank_lut,
                blockhash,
            )?),
            &[&self.payer],
        )?;

        self.rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &txn,
                CommitmentConfig::confirmed(),
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    ..Default::default()
                },
            )?;

        Ok(())
    }
}

pub fn is_stale_swb_price_error(err: &ClientError) -> bool {
    matches!(
        err.kind(),
        ClientErrorKind::TransactionError(TransactionError::InstructionError(
            _,
            InstructionError::Custom(SWB_STALE_PRICE_ERROR_CODE)
        ))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_client::client_error::ClientError;
    use solana_client::client_error::ClientErrorKind;

    #[test]
    fn test_is_stale_swb_price_true_transaction_error() {
        let err = ClientError {
            request: None,
            kind: ClientErrorKind::TransactionError(TransactionError::InstructionError(
                0,
                InstructionError::Custom(6049), // The stale Swb price error code
            )),
        };
        assert!(is_stale_swb_price_error(&err));
    }

    #[test]
    fn test_is_stale_swb_price_false_wrong_custom_code() {
        let err = ClientError {
            request: None,
            kind: ClientErrorKind::TransactionError(TransactionError::InstructionError(
                0,
                InstructionError::Custom(1234),
            )),
        };
        assert!(!is_stale_swb_price_error(&err));
    }

    #[test]
    fn test_is_stale_swb_price_false_other_instruction_error() {
        let err = ClientError {
            request: None,
            kind: ClientErrorKind::TransactionError(TransactionError::InstructionError(
                0,
                InstructionError::InvalidArgument,
            )),
        };
        assert!(!is_stale_swb_price_error(&err));
    }

    #[test]
    fn test_is_stale_swb_price_false_other_transaction_error() {
        let err = ClientError {
            request: None,
            kind: ClientErrorKind::TransactionError(TransactionError::AccountNotFound),
        };
        assert!(!is_stale_swb_price_error(&err));
    }

    #[test]
    fn test_is_stale_swb_price_false_wrong_code() {
        let err = ClientError {
            request: None,
            kind: ClientErrorKind::Custom("Some other error".to_string()),
        };
        assert!(!is_stale_swb_price_error(&err));
    }

    #[test]
    fn test_is_stale_swb_price_false_other_kind() {
        let err = ClientError {
            request: None,
            kind: ClientErrorKind::Io(std::io::Error::new(std::io::ErrorKind::Other, "io error")),
        };
        assert!(!is_stale_swb_price_error(&err));
    }
}
