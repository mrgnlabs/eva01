use std::mem::size_of;
use std::{collections::HashMap, error::Error, sync::Arc};

use backoff::{retry, ExponentialBackoff};
use futures::SinkExt;
use futures::StreamExt;
use log::debug;
use log::error;
use log::info;
use log::warn;
use marginfi::state::marginfi_account::MarginfiAccount;
use marginfi::state::marginfi_group::Bank;
use solana_program::pubkey::Pubkey;
use tonic::service::Interceptor;
use yellowstone_grpc_client::{GeyserGrpcClient, GeyserGrpcClientError};
use yellowstone_grpc_proto::prelude::*;

use crate::utils::account_update_to_account;

use super::engine::StateEngineService;

#[derive(Debug, thiserror::Error)]
pub enum GeyserServiceError {
    #[error("Generic error")]
    GenericError,
    #[error("Geyser client error: {0}")]
    GeyserServiceError(#[from] GeyserGrpcClientError),
    #[error("Error parsing account: {0}")]
    AnyhowError(#[from] anyhow::Error),
}

#[derive(Debug, Default)]
struct GeyserRequestUpdate {
    accounts: Vec<Pubkey>,
}

impl GeyserRequestUpdate {
    fn merge(&mut self, other: GeyserRequestUpdate) {
        self.accounts.extend(other.accounts);
    }

    fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    fn new_empty() -> Self {
        Self {
            accounts: Vec::new(),
        }
    }

    fn to_geyser_request(&self) -> SubscribeRequest {
        let mut request = SubscribeRequest {
            ..Default::default()
        };

        let mut accounts = HashMap::new();

        let accounts_to_track = self.accounts.iter().map(|a| a.to_string()).collect();

        let subscribe_to_static_account_updates = SubscribeRequestFilterAccounts {
            account: accounts_to_track,
            ..Default::default()
        };

        accounts.insert("a".to_string(), subscribe_to_static_account_updates);

        request.accounts = accounts;

        request
    }
}

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
    ) -> Result<(), GeyserServiceError> {
        retry(
            ExponentialBackoff::default(),
            move || -> Result<(), backoff::Error<GeyserServiceError>> {
                let endpoint = config.endpoint.clone();
                let x_token = config.x_token.clone();

                let geyser_client =
                    yellowstone_grpc_client::GeyserGrpcClient::connect(endpoint, x_token, None)
                        .map_err(|e| backoff::Error::transient(e.into()))?;

                info!("Connected to geyser");

                let state_engine = state_engine.clone();

                tokio::spawn(
                    async move { Self::subscribe_and_run(geyser_client, state_engine).await },
                );

                Ok::<(), backoff::Error<GeyserServiceError>>(())
            },
        )
        .map_err(|_| GeyserServiceError::GenericError)?;

        Ok(())
    }

    async fn subscribe_and_run(
        mut geyser_client: GeyserGrpcClient<impl Interceptor>,
        state_engine: Arc<StateEngineService>,
    ) -> Result<(), GeyserServiceError> {
        let sub_req = Self::build_geyser_subscribe_request(&state_engine);
        let (mut subscribe_tx, mut subscribe_rx) = geyser_client.subscribe().await?;

        subscribe_tx.send(sub_req);

        while let Some(msg) = subscribe_rx.next().await {
            let update = match msg {
                Ok(msg) => Self::process_message(&state_engine, msg)?,
                Err(e) => {
                    error!("Error receiving message from geyser: {:?}", e);
                    GeyserRequestUpdate::new_empty()
                }
            };

            if !update.is_empty() {
                subscribe_tx
                    .send(update.to_geyser_request())
                    .await
                    .map_err(|e| {
                        error!("Error sending message to geyser: {:?}", e);
                        GeyserServiceError::GenericError
                    })?
            }
        }

        Ok(())
    }

    fn process_message(
        state_engine: &Arc<StateEngineService>,
        message: SubscribeUpdate,
    ) -> Result<GeyserRequestUpdate, GeyserServiceError> {
        let mut geyser_update_request = GeyserRequestUpdate::new_empty();

        if let Some(update_oneof) = message.update_oneof {
            match update_oneof {
                subscribe_update::UpdateOneof::Account(account) => {
                    if account.is_startup {
                        debug!("Received startup message from geyser, ignoring");
                        return Ok(geyser_update_request);
                    }

                    let mut processed = false;
                    if let Some(account) = &account.account {
                        if let Ok(account_owner_pk) = Pubkey::try_from(account.owner.clone()) {
                            if account_owner_pk == state_engine.get_marginfi_program_id() {
                                let maybe_update =
                                    Self::process_marginfi_account_update(state_engine, &account)?;
                                if let Some(update) = maybe_update {
                                    geyser_update_request.merge(update);
                                }
                                processed = true;
                            }
                        }

                        if let Ok(address) = Pubkey::try_from(account.pubkey.clone()) {
                            if state_engine.is_tracked_oracle(&address) {
                                Self::process_oracle_account_update(state_engine, &account)?;
                                processed = true;
                            }

                            if state_engine.is_tracked_token_account(&address) {
                                Self::process_token_account_update(state_engine, &account)?;
                                processed = true;
                            }
                        }
                    }
                    if !processed {
                        warn!(
                            "None of the updates were processed for account {:?}",
                            account
                        );
                    }
                }
                _ => warn!("Received unknown update {:?} from geyser", update_oneof),
            }
        }

        Ok(geyser_update_request)
    }

    fn process_marginfi_account_update(
        state_engine: &Arc<StateEngineService>,
        account_update: &SubscribeUpdateAccountInfo,
    ) -> Result<Option<GeyserRequestUpdate>, GeyserServiceError> {
        let account_address = Pubkey::try_from(account_update.pubkey.clone()).map_err(|e| {
            error!("Error parsing marginfi account address: {:?}", e);
            GeyserServiceError::GenericError
        })?;
        let account = account_update_to_account(account_update).map_err(|e| {
            error!("Error parsing marginfi account: {:?}", e);
            GeyserServiceError::GenericError
        })?;

        let mut update_request = None;

        let accont_len = account.data.len();

        match accont_len {
            n if n == size_of::<Bank>() + 8 => {
                let new_bank = state_engine.update_bank(&account_address, account)?;
                if new_bank {
                    update_request = Some(GeyserRequestUpdate {
                        accounts: vec![account_address],
                    });
                }
            }
            n if n == size_of::<MarginfiAccount>() => {
                todo!();
            }
            _ => {
                warn!(
                    "Error parsing marginfi account: invalid account size {}",
                    accont_len
                );
            }
        }

        Ok(update_request)
    }
    fn process_oracle_account_update(
        state_engine: &Arc<StateEngineService>,
        account_update: &SubscribeUpdateAccountInfo,
    ) -> Result<(), GeyserServiceError> {
        let oracle_addres = Pubkey::try_from(account_update.pubkey.clone()).map_err(|e| {
            error!("Error parsing oracle address: {:?}", e);
            GeyserServiceError::GenericError
        })?;
        let oracle_account = account_update_to_account(account_update).map_err(|e| {
            error!("Error parsing oracle account: {:?}", e);
            GeyserServiceError::GenericError
        })?;
        state_engine.update_oracle(&oracle_addres, oracle_account);
        Ok(())
    }
    fn process_token_account_update(
        state_engine: &Arc<StateEngineService>,
        account_update: &SubscribeUpdateAccountInfo,
    ) -> Result<(), GeyserServiceError> {
        Ok(())
    }

    fn build_geyser_subscribe_request(state_engine: &Arc<StateEngineService>) -> SubscribeRequest {
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
