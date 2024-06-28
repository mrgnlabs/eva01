use crate::utils::account_update_to_account;
use crossbeam::channel::Sender;
use futures::StreamExt;
use log::{error, info};
use marginfi::state::marginfi_account::MarginfiAccount;
use solana_program::pubkey::Pubkey;
use solana_sdk::account::Account;
use std::{collections::HashMap, mem::size_of};
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::prelude::*;

const MARGIN_ACCOUNT_SIZE: usize = size_of::<MarginfiAccount>() + 8;

/// Struct that is used to communicate between geyser and other services
/// in the Eva
#[derive(Debug, Clone)]
pub struct GeyserUpdate {
    pub account_type: AccountType,
    pub address: Pubkey,
    pub account: Account,
}

/// Types of subscribed account, easier to distribute
/// OracleAccount -> Rebalancer and liquidator
/// MarginfiAccount -> Rebalaner and liquidator (Should be moved, so the only account
///                    sended to rebalancer is the liquidator account)
/// TokenAccount -> Rebalancer
#[derive(Clone, Debug)]
pub enum AccountType {
    OracleAccount,
    MarginfiAccount,
    TokenAccount,
}

pub struct GeyserServiceConfig {
    pub endpoint: String,
    pub x_token: Option<String>,
}

/// Geyser service is responsible for receiving and distrubute the
/// messages to the needed services. It already separates the messages by
/// liquidator or rebalancer to minizime the possible quantity of messages in
/// cache in the respective services.
pub struct GeyserService {}

impl GeyserService {
    pub async fn connect(
        config: GeyserServiceConfig,
        tracked_accounts: HashMap<Pubkey, AccountType>,
        marginfi_program_id: Pubkey,
        liquidator_sender: Sender<GeyserUpdate>,
        rebalancer_sender: Sender<GeyserUpdate>,
    ) -> anyhow::Result<()> {
        info!("Connecting to geyser");

        let mut client = GeyserGrpcClient::build_from_shared(config.endpoint)?
            .x_token(config.x_token)?
            .connect()
            .await?;

        info!("Connected to geyser");

        let tracked_accounts_vec: Vec<Pubkey> = tracked_accounts.keys().cloned().collect();

        let sub_req =
            Self::build_geyser_subscribe_request(&tracked_accounts_vec, &marginfi_program_id);

        let (_, mut stream) = client.subscribe_with_request(Some(sub_req)).await?;

        while let Some(msg) = stream.next().await {
            match msg {
                Ok(msg) => {
                    if let Some(update_oneof) = msg.update_oneof {
                        if let subscribe_update::UpdateOneof::Account(account) = update_oneof {
                            if let Some(update_account) = &account.account {
                                if let Ok(address) = Pubkey::try_from(update_account.pubkey.clone())
                                {
                                    if let Ok(account) = account_update_to_account(update_account) {
                                        if let Ok(account_owner_pk) =
                                            Pubkey::try_from(account.owner)
                                        {
                                            if account_owner_pk == marginfi_program_id
                                                && update_account.data.len() == MARGIN_ACCOUNT_SIZE
                                            {
                                                let update = GeyserUpdate {
                                                    account_type: AccountType::MarginfiAccount,
                                                    address,
                                                    account: account.clone(),
                                                };
                                                if let Err(e) =
                                                    liquidator_sender.send(update.clone())
                                                {
                                                    error!("Error sending update to the liquidator sender: {:?}", e);
                                                }
                                                if let Err(e) =
                                                    rebalancer_sender.send(update.clone())
                                                {
                                                    error!("Error sending update to the rebalancer sender: {:?}", e);
                                                }
                                            }
                                        }
                                        if let Some(account_type) = tracked_accounts.get(&address) {
                                            let update = GeyserUpdate {
                                                account_type: account_type.clone(),
                                                address,
                                                account: account.clone(),
                                            };

                                            match account_type {
                                                AccountType::OracleAccount => {
                                                    if let Err(e) =
                                                        liquidator_sender.send(update.clone())
                                                    {
                                                        error!("Error sending update to the liquidator sender: {:?}", e);
                                                    }
                                                    if let Err(e) =
                                                        rebalancer_sender.send(update.clone())
                                                    {
                                                        error!("Error sending update to the rebalancer sender: {:?}", e);
                                                    }
                                                }
                                                AccountType::TokenAccount => {
                                                    if let Err(e) =
                                                        rebalancer_sender.send(update.clone())
                                                    {
                                                        error!("Error sending update to the rebalancer sender: {:?}", e);
                                                    }
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Error receiving message from geyser {:?}", e);
                    break;
                }
            }
        }
        Ok(())
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

        request
    }
}
