use std::{collections::HashMap, sync::Arc};

use backoff::{retry, ExponentialBackoff};
use log::info;
use solana_rpc_client_api::filter::Memcmp;
use tonic::service::Interceptor;
use yellowstone_grpc_client::{GeyserGrpcClient, GeyserGrpcClientError};
use yellowstone_grpc_proto::prelude::*;

use super::engine::StateEngineService;

pub struct GeyserServiceConfig {
    pub endpoint: String,
    pub x_token: Option<String>,
}

const MARGINFI_ACCOUNT_GROUP_PK_OFFSET: usize = 8;
const BANK_GROUP_PK_OFFSET: usize = 8;

pub struct GeyserService {}

impl GeyserService {
    pub async fn connect(
        config: GeyserServiceConfig,
        state_engine: Arc<StateEngineService>,
    ) -> Result<(), Box<GeyserGrpcClientError>> {
        retry(ExponentialBackoff::default(), move || {
            let endpoint = config.endpoint.clone();
            let x_token = config.x_token.clone();

            let mut geyser_client =
                yellowstone_grpc_client::GeyserGrpcClient::connect(endpoint, x_token, None)
                    .map_err(|e| backoff::Error::transient(e))?;

            info!("Connected to geyser");

            tokio::spawn(async move {
                let sub = geyser_client
                    .subscribe_once2(SubscribeRequest {
                        ..Default::default()
                    })
                    .await;
            });

            Ok::<(), backoff::Error<GeyserGrpcClientError>>(())
        });

        Ok(())
    }

    async fn subscribe_and_run(
        mut geyser_client: GeyserGrpcClient<impl Interceptor>,
        state_engine: Arc<StateEngineService>,
    ) -> Result<(), Box<GeyserGrpcClientError>> {
        let sub = geyser_client
            .subscribe_once2(Self::build_geyser_subscribe_request(state_engine))
            .await?;

        Ok(())
    }

    fn build_geyser_subscribe_request(state_engine: Arc<StateEngineService>) -> SubscribeRequest {
        let mut request = SubscribeRequest {
            ..Default::default()
        };

        let accounts_to_track = state_engine.get_accounts_to_track();

        let subscribe_to_static_account_updates = SubscribeRequestFilterAccounts {
            account: accounts_to_track.iter().map(|a| a.to_string()).collect(),
            ..Default::default()
        };

        let marginfi_account_subscription = SubscribeRequestFilterAccounts {
            owner: vec![state_engine.get_marginfi_program_id().to_string()],
            ..Default::default()
        };

        let mut req = HashMap::new();

        req.insert("a".to_string(), subscribe_to_static_account_updates);
        req.insert("b".to_string(), marginfi_account_subscription);

        request.accounts = req;

        request
    }
}
