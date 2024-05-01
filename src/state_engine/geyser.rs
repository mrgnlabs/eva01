use std::mem::size_of;
use std::time::Instant;
use std::{collections::HashMap, sync::Arc};

use anchor_spl::token_2022::spl_token_2022::state;
use backoff::{retry, ExponentialBackoff};
use futures::channel::mpsc::SendError;
use futures::SinkExt;
use futures::StreamExt;
use log::error;
use log::info;
use log::warn;
use log::{debug, trace};
use marginfi::state::marginfi_account::MarginfiAccount;
use marginfi::state::marginfi_group::Bank;
use solana_program::pubkey::Pubkey;
use tokio::spawn;
use tokio::task::JoinHandle;
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
    #[error("Error sending message to geyser: {0}")]
    SendError(#[from] SendError),
}

const BANK_SIZE: usize = size_of::<Bank>() + 8;
const MARGIN_ACCOUNT_SIZE: usize = size_of::<MarginfiAccount>() + 8;

enum ProcessMessageRespose {
    Update(GeyserRequestUpdate),
    Pong,
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

        debug!("Sending SubscribeRequest for accounts: {:?}", self.accounts);

        let accounts_to_track = self.accounts.iter().map(|a| a.to_string()).collect();

        let subscribe_to_static_account_updates = SubscribeRequestFilterAccounts {
            account: accounts_to_track,
            ..Default::default()
        };

        accounts.insert(
            "static_accounts".to_string(),
            subscribe_to_static_account_updates,
        );

        request.accounts = accounts;

        request
    }
}

pub struct GeyserServiceConfig {
    pub endpoint: String,
    pub x_token: Option<String>,
}

pub struct GeyserService {}

impl GeyserService {
    pub async fn connect(
        config: GeyserServiceConfig,
        state_engine: Arc<StateEngineService>,
    ) -> Result<JoinHandle<Result<(), GeyserServiceError>>, GeyserServiceError> {
        let handle = retry(
            ExponentialBackoff::default(),
            move || -> Result<JoinHandle<Result<(), GeyserServiceError>>, backoff::Error<GeyserServiceError>> {
                let endpoint = config.endpoint.clone();
                let x_token = config.x_token.clone();

                let geyser_client =
                    yellowstone_grpc_client::GeyserGrpcClient::connect(endpoint, x_token, None)
                        .map_err(|e| backoff::Error::transient(e.into()))?;

                info!("Connected to geyser");

                let state_engine = state_engine.clone();

                let handle = tokio::spawn(async move {
                    Self::subscribe_and_run(geyser_client, state_engine).await?;
                    error!("Geyser service ended");

                    Ok(())
                });

                Ok::<_, backoff::Error<GeyserServiceError>>(handle)
            },
        )
        .map_err(|_| GeyserServiceError::GenericError)?;

        Ok(handle)
    }

    async fn subscribe_and_run(
        mut geyser_client: GeyserGrpcClient<impl Interceptor + 'static>,
        state_engine: Arc<StateEngineService>,
    ) -> Result<(), GeyserServiceError> {
        debug!("Subscribing to geyser");
        let sub_req = Self::build_geyser_subscribe_request(&state_engine);
        let (mut subscribe_tx, mut subscribe_rx) = geyser_client.subscribe().await?;

        subscribe_tx.send(sub_req.clone()).await.map_err(|e| {
            error!("Error sending message to geyser: {:?}", e);
            GeyserServiceError::GenericError
        })?;

        let state_engine_clone = state_engine.clone();

        let handle = tokio::task::spawn(async move {
            let mut ping_id = 1;

            loop {
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;

                debug!("Sending ping to geyser server");

                if let Err(e) = subscribe_tx
                    .send(SubscribeRequest {
                        ping: Some(SubscribeRequestPing { id: ping_id }),
                        ..Default::default()
                    })
                    .await
                {
                    error!("Error sending message to geyser: {:?}", e);
                }

                ping_id += 1;
            }

            error!("Ping loop ended");
        });

        while let Some(msg) = subscribe_rx.next().await {
            let start = Instant::now();
            // if last_heartbeat.elapsed() > std::time::Duration::from_secs(5) {
            //     debug!("Sending heartbeat to geyser");
            //     let sub_req = Self::build_geyser_subscribe_request(&state_engine);
            //     let _ = subscribe_tx.send(sub_req).await.map_err(|e| {
            //         error!("Error sending message to geyser: {:?}", e);
            //     });
            //     last_heartbeat = Instant::now();
            // }

            let update = match msg {
                Ok(msg) => Self::process_message(&state_engine, msg)?,
                Err(e) => {
                    error!("Error receiving message from geyser: {:?}", e);
                    false
                }
            };

            // if update {
            //     let sub_req = Self::build_geyser_subscribe_request(&state_engine);
            //     if let Err(e) = subscribe_tx.send(sub_req.clone()).await {
            //         error!("Error sending message to geyser: {:?}", e);
            //     }
            // }

            trace!("Processed message in {:?}", start.elapsed());
        }

        handle.await;

        error!("Geyser subscription ended");

        Ok(())
    }

    fn process_message(
        state_engine: &Arc<StateEngineService>,
        message: SubscribeUpdate,
    ) -> Result<bool, GeyserServiceError> {
        let mut geyser_update_request = false;

        trace!("Received message from geyser: {:?}", message.filters);

        if let Some(update_oneof) = message.update_oneof {
            match update_oneof {
                subscribe_update::UpdateOneof::Account(account) => {
                    if account.is_startup {
                        debug!("Received startup message from geyser, ignoring");
                        return Ok(false);
                    }

                    let mut processed = false;
                    if let Some(account) = &account.account {
                        if let Ok(account_owner_pk) = Pubkey::try_from(account.owner.clone()) {
                            if account_owner_pk == state_engine.get_marginfi_program_id() {
                                let maybe_update =
                                    Self::process_marginfi_account_update(state_engine, &account)?;
                                if let Some(update) = maybe_update {
                                    geyser_update_request = true;
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

                            if state_engine.is_tracked_sol_account(&address) {
                                // Self::process_token_account_update(state_engine, &account)?;
                                processed = true;
                            }
                        }
                    }
                    if !processed {
                        warn!(
                            "None of the updates were processed for account {:?}",
                            account
                        );
                    } else {
                        // state_engine.trigger_update_signal();
                    }
                }
                subscribe_update::UpdateOneof::Ping(_) => return Ok(false),
                subscribe_update::UpdateOneof::Pong(_) => return Ok(false),
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

        debug!("Processing marginfi account update: {:?}", account_address);

        let mut update_request = None;

        let accont_len = account.data.len();

        match accont_len {
            BANK_SIZE => {
                debug!("Processing marginfi bank account update");
                let new_bank = state_engine.update_bank(&account_address, account)?;

                if new_bank {
                    update_request = Some(GeyserRequestUpdate {
                        accounts: vec![account_address],
                    });
                }
            }
            MARGIN_ACCOUNT_SIZE => {
                debug!("Processing marginfi account update");
                state_engine.update_marginfi_account(&account_address, &account)?;
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
        if let Err(e) = state_engine.update_oracle(&oracle_addres, oracle_account) {
            warn!("Error updating oracle account: {:?}", e);
        } else {
            debug!("Oracle account updated");
        }

        Ok(())
    }
    fn process_token_account_update(
        state_engine: &Arc<StateEngineService>,
        account_update: &SubscribeUpdateAccountInfo,
    ) -> Result<(), GeyserServiceError> {
        debug!("Processing token account update");

        let account_address = Pubkey::try_from(account_update.pubkey.clone()).map_err(|e| {
            error!("Error parsing token account address: {:?}", e);
            GeyserServiceError::GenericError
        })?;

        let account = account_update_to_account(account_update).map_err(|e| {
            error!("Error parsing token account: {:?}", e);
            GeyserServiceError::GenericError
        })?;

        trace!("Token account update: {:?}", account_address);

        if let Err(e) = state_engine.update_token_account(&account_address, account) {
            warn!("Error updating token account: {:?}", e);
        } else {
            debug!("Token account updated");
        }

        Ok(())
    }

    fn process_sol_account_update(
        state_engine: &Arc<StateEngineService>,
        account_update: &SubscribeUpdateAccountInfo,
    ) -> Result<(), GeyserServiceError> {
        debug!("Processing sol account update");

        let account_address = Pubkey::try_from(account_update.pubkey.clone()).map_err(|e| {
            error!("Error parsing marginfi account address: {:?}", e);
            GeyserServiceError::GenericError
        })?;

        let account = account_update_to_account(account_update).map_err(|e| {
            error!("Error parsing marginfi account: {:?}", e);
            GeyserServiceError::GenericError
        })?;

        if let Err(e) = state_engine.update_sol_account(account_address, account) {
            warn!("Error updating sol account: {:?}", e);
        } else {
            debug!("Sol account updated");
        }

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

        req.insert(
            "static_accounts".to_string(),
            subscribe_to_static_account_updates,
        );
        req.insert(
            "marginfi_accounts".to_string(),
            marginfi_account_subscription,
        );

        request.accounts = req;

        debug!("Sending SubscribeRequest: {:#?}", request);

        request
    }
}
