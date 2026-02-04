use anyhow::Result;
use log::warn;
use solana_client::{
    client_error::{ClientError, ClientErrorKind},
    nonblocking::rpc_client::RpcClient as NonBlockingRpcClient,
    rpc_client::RpcClient,
    rpc_config::RpcSendTransactionConfig,
    rpc_request::RpcError,
};
use solana_sdk::{
    commitment_config::{CommitmentConfig, CommitmentLevel},
    genesis_config::ClusterType,
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    transaction::VersionedTransaction,
};
use std::str::FromStr;
use switchboard_on_demand_client::{
    CrossbarClient, FetchUpdateManyParams, Gateway, PullFeed, QueueAccountData, SbContext,
};
use tokio::runtime::{Builder, Runtime};

use crate::config::Eva01Config;

//TODO: parametrize the Swb Program ID.
pub const SWB_PROGRAM_ID: &str = "A43DyUGA7s8eXPxqEjJY6EBu1KKbNgfxF8h17VAHn13w";

pub const SWB_STALE_PRICE_ERROR_CODE: &str = "17a1";

const CHUNK_SIZE: usize = 6;

pub struct SwbCranker {
    tokio_rt: Runtime,
    rpc_client: RpcClient,
    non_blocking_rpc_client: NonBlockingRpcClient,
    swb_gateway: Gateway,
    crossbar: Option<CrossbarClient>,
    payer: Keypair,
    unstable_feeds: Vec<Pubkey>,
}

impl SwbCranker {
    pub fn new(config: &Eva01Config) -> Result<Self> {
        let payer = Keypair::from_bytes(&config.wallet_keypair)?;

        let tokio_rt = Builder::new_multi_thread()
            .thread_name("SwbCranker")
            .worker_threads(2)
            .enable_all()
            .build()?;

        let rpc_client =
            RpcClient::new_with_commitment(config.rpc_url.clone(), CommitmentConfig::confirmed());
        let non_blocking_rpc_client = NonBlockingRpcClient::new_with_commitment(
            config.rpc_url.clone(),
            CommitmentConfig::confirmed(),
        );
        let queue = tokio_rt.block_on(QueueAccountData::load(
            &non_blocking_rpc_client,
            &Pubkey::from_str(SWB_PROGRAM_ID).unwrap(),
        ))?;

        // Prefer private gateway from env; fall back to first on-chain gateway
        let (swb_gateway, crossbar) = if let Some(url) = config.crossbar_api_url.as_ref() {
            let crossbar = CrossbarClient::new(url.as_str(), true);
            (
                tokio_rt.block_on(queue.fetch_gateway_from_crossbar(&crossbar))?,
                Some(crossbar),
            )
        } else {
            (
                tokio_rt.block_on(queue.fetch_gateways(&non_blocking_rpc_client))?[0].clone(),
                None,
            )
        };

        Ok(Self {
            tokio_rt,
            rpc_client,
            non_blocking_rpc_client,
            swb_gateway,
            crossbar,
            payer,
            unstable_feeds: config.unstable_swb_feeds.clone(),
        })
    }

    pub fn crank_oracles(&self, swb_oracles: Vec<Pubkey>) -> Result<()> {
        // Run simulations to get more details on potential failures, if crossbar is available.
        if let Some(crossbar) = self.crossbar.as_ref() {
            let result = self
                .tokio_rt
                .block_on(crossbar.simulate_solana_feeds(ClusterType::MainnetBeta, &swb_oracles));
            if let Err(result) = result {
                warn!("SWB Simulation failed: {:?}", result);
            }
        }

        let swb_oracles: Vec<_> = swb_oracles
            .into_iter()
            .filter(|f| !self.unstable_feeds.contains(f))
            .collect();

        for chunk in swb_oracles.chunks(CHUNK_SIZE) {
            self.crank_oracles_internal(chunk.to_vec())?;
        }
        Ok(())
    }

    fn crank_oracles_internal(&self, swb_oracles: Vec<Pubkey>) -> Result<()> {
        let (crank_ix, crank_lut) = self.tokio_rt.block_on(PullFeed::fetch_update_consensus_ix(
            SbContext::new(),
            &self.non_blocking_rpc_client,
            FetchUpdateManyParams {
                feeds: swb_oracles,
                payer: self.payer.pubkey(),
                gateway: self.swb_gateway.clone(),
                num_signatures: Some(1),
                ..Default::default()
            },
        ))?;

        let blockhash = self
            .rpc_client
            .get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())?
            .0;

        let tx = VersionedTransaction::try_new(
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
                &tx,
                CommitmentConfig::confirmed(),
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    preflight_commitment: Some(CommitmentLevel::Processed),
                    ..Default::default()
                },
            )?;

        Ok(())
    }
}

pub fn is_stale_swb_price_error(err: &ClientError) -> bool {
    if let ClientErrorKind::RpcError(RpcError::RpcResponseError { message, .. }) = err.kind() {
        message.contains(SWB_STALE_PRICE_ERROR_CODE)
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_client::client_error::ClientError;
    use solana_client::client_error::ClientErrorKind;
    use solana_client::rpc_request::RpcResponseErrorData;

    #[test]
    fn test_is_stale_swb_price_true_transaction_error() {
        let err = ClientError {
            request: None,
            kind: ClientErrorKind::RpcError(RpcError::RpcResponseError {
                code: -32000,
                message: SWB_STALE_PRICE_ERROR_CODE.to_string(),
                data: RpcResponseErrorData::Empty,
            }),
        };
        assert!(is_stale_swb_price_error(&err));
    }

    #[test]
    fn test_is_stale_swb_price_false_wrong_custom_code() {
        let err = ClientError {
            request: None,
            kind: ClientErrorKind::RpcError(RpcError::RpcResponseError {
                code: -32000,
                message: "12a4".to_string(),
                data: RpcResponseErrorData::Empty,
            }),
        };
        assert!(!is_stale_swb_price_error(&err));
    }

    #[test]
    fn test_is_stale_swb_price_false_other_instruction_error() {
        let err = ClientError {
            request: None,
            kind: ClientErrorKind::RpcError(RpcError::ParseError("Test error".to_string())),
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
