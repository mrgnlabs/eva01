use backoff::{retry, ExponentialBackoff};
use futures::{SinkExt, StreamExt};
use log::{debug, error};
use solana_program::pubkey::Pubkey;
use std::collections::HashMap;
use tokio::task::JoinHandle;
use tonic::service::Interceptor;
use yellowstone_grpc_client::{GeyserGrpcClient, GeyserGrpcClientError};
use yellowstone_grpc_proto::prelude::*;
use crossbeam::channel::Sender;
use futures::channel::mpsc::SendError;

#[derive(Clone, Debug)]
pub enum AccountType {
    OracleAccount,
    MarginfiAccount
}

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



#[derive(Debug)]
pub struct GeyserUpdate {
    pub account_type: AccountType,
    pub address: Pubkey,
    pub account: SubscribeUpdateAccountInfo
}

pub struct GeyserServiceConfig {
    pub endpoint: String,
    pub x_token: Option<String>,
}

pub struct GeyserService {}

impl GeyserService {

    pub async fn connect(
        config: GeyserServiceConfig,
        tracked_accounts: HashMap<Pubkey, AccountType>,
        marginfi_program_id: Pubkey,
        sender: Sender<GeyserUpdate>
    ) -> Result<JoinHandle<Result<(), GeyserServiceError>>, GeyserServiceError> {
        let handle = retry(
            ExponentialBackoff::default(),
            move || -> Result<JoinHandle<Result<(), GeyserServiceError>>, backoff::Error<GeyserServiceError>> { 
                let endpoint = config.endpoint.clone();
                let x_token = config.x_token.clone();

                let geyser_client = yellowstone_grpc_client::GeyserGrpcClient::connect(endpoint, x_token, None)
                    .map_err(|e| backoff::Error::transient(e.into()))?;
               

                let tracked_accounts_cl = tracked_accounts.clone();
                let marginfi_program_id_cl = marginfi_program_id.clone();
                let sender = sender.clone();
                let handle = tokio::spawn(async move {
                    Self::subscribe_and_run(tracked_accounts_cl, marginfi_program_id_cl, geyser_client, sender).await?;
                    Ok(())
                });


                Ok::<_, backoff::Error<GeyserServiceError>>(handle)
            },
        )
        .map_err(|_| GeyserServiceError::GenericError)?;
        Ok(handle)
    }

    async fn subscribe_and_run(
        tracked_accounts: HashMap<Pubkey, AccountType>, 
        marginfi_program_id: Pubkey,
        mut geyser_client: GeyserGrpcClient<impl Interceptor + 'static>,
        sender: Sender<GeyserUpdate>
    ) -> anyhow::Result<()> {

        let tracked_accounts_vec: Vec<Pubkey> = tracked_accounts.keys().cloned().collect();
        let sub_req =
            Self::build_geyser_subscribe_request(&tracked_accounts_vec, &marginfi_program_id);

        let (mut subscribe_tx, mut subscribe_rx) = geyser_client.subscribe().await?;

        subscribe_tx.send(sub_req.clone()).await.map_err(|e| {
            error!("Error sending message to geyser client: {:?}", e);
            GeyserServiceError::GenericError
        })?;

        // Stars a thread to handle ping's to the geyser protocol to
        // prevent disconnection
        let ping_service_handle = tokio::task::spawn(async move {
            let mut ping_id = 1;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;

                if let Err(e) = subscribe_tx
                    .send(SubscribeRequest {
                        ping: Some(SubscribeRequestPing { id: ping_id }),
                        ..Default::default()
                    })
                    .await
                {
                    error!("ERror sending message to geyser: {:?}", e);
                }
                ping_id += 1;
            }
        });
        while let Some(msg) = subscribe_rx.next().await {
            match msg {
                Ok(msg) => {
                    if let Some(update_oneof) = msg.update_oneof {
                        if let subscribe_update::UpdateOneof::Account(account) = update_oneof {
                            if let Some(account) = &account.account {
                                if let Ok(address) = Pubkey::try_from(account.pubkey.clone()) {
                                    if let Ok(account_owner_pk) = Pubkey::try_from(account.owner.clone()) {
                                        if account_owner_pk == marginfi_program_id {
                                            let _ = sender.send(GeyserUpdate { 
                                                account_type: AccountType::MarginfiAccount, 
                                                address, 
                                                account: account.clone() });
                                        }
                                    }
                                    if let Some(account_type) = tracked_accounts.get(&address) {
                                        let _ = sender.send(GeyserUpdate {
                                            account_type: account_type.clone(),
                                            address,
                                            account: account.clone()
                                        });
                                    }
                                }
                            }
                        }
                    }
                }, Err(e) => {
                    error!("Error receiving message from geyser: {:?}", e);
                }
            }
        }
        let _ = ping_service_handle.await;

        error!("Geyser subscription ended!");

        todo!();
    }


    /// Builds a geyser subscription request payload
    fn build_geyser_subscribe_request(
        tracked_accounts: &[Pubkey],
        marginfi_program_id: &Pubkey,
    ) -> SubscribeRequest {
        let mut request = SubscribeRequest {
            ..Default::default()
        };

        let subscribe_to_static_account_updates = SubscribeRequestFilterAccounts {
            account: tracked_accounts.iter().map(|a| a.to_string()).collect(),
            ..Default::default()
        };

        let marginfi_account_subscription = SubscribeRequestFilterAccounts {
            owner: vec![marginfi_program_id.to_string()],
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
