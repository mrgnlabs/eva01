use crate::{utils::account_update_to_account, ward};
use anchor_lang::AccountDeserialize;
use crossbeam::channel::Sender;
use futures::StreamExt;
use log::{error, info, warn};
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

/// Types of subscribed accounts, easier to distribute
/// OracleAccount -> Rebalancer and liquidator
/// MarginfiAccount -> Rebalaner and liquidator (Should be moved, so the only account
///                    sended to rebalancer is the liquidator account)
/// TokenAccount -> Rebalancer
#[derive(Clone, Debug)]
pub enum AccountType {
    Oracle,
    Marginfi,
    Token,
}

pub struct GeyserServiceConfig {
    pub endpoint: String,
    pub x_token: Option<String>,
}

/// Geyser service is responsible for receiving and distrubuting the
/// messages to the other services.
pub struct GeyserService {}

impl GeyserService {
    pub async fn connect(
        config: GeyserServiceConfig,
        tracked_accounts: HashMap<Pubkey, AccountType>,
        marginfi_program_id: Pubkey,
        marginfi_group_pk: Pubkey,
        liquidator_sender: Sender<GeyserUpdate>,
        rebalancer_sender: Sender<GeyserUpdate>,
    ) -> anyhow::Result<()> {
        info!("Connecting to geyser...");
        let mut client = GeyserGrpcClient::build_from_shared(config.endpoint.clone())?
            .x_token(config.x_token.clone())?
            .connect()
            .await?;
        let tracked_accounts_vec: Vec<Pubkey> = tracked_accounts.keys().cloned().collect();
        let sub_req =
            Self::build_geyser_subscribe_request(&tracked_accounts_vec, &marginfi_program_id);
        let (_, mut stream) = client.subscribe_with_request(Some(sub_req.clone())).await?;

        info!("Connected to geyser");

        while let Some(msg) = stream.next().await {
            if let Err(e) = msg {
                warn!("Reconnecting to Geyser due to error: {:?}", e);
                let (_, new_stream) = client.subscribe_with_request(Some(sub_req.clone())).await?;
                stream = new_stream;
                continue;
            }

            let update_oneof = ward!(msg.unwrap().update_oneof, continue);

            if let subscribe_update::UpdateOneof::Account(account) = update_oneof {
                let account_update = ward!(&account.account, continue);
                let account = ward!(account_update_to_account(account_update).ok(), continue);
                let address = ward!(
                    Pubkey::try_from(account_update.pubkey.clone()).ok(),
                    continue
                );

                if account.owner == marginfi_program_id
                    && account_update.data.len() == MARGIN_ACCOUNT_SIZE
                {
                    let marginfi_account = ward!(
                        MarginfiAccount::try_deserialize(&mut account.data.as_slice()).ok(),
                        continue
                    );

                    if marginfi_account.group != marginfi_group_pk {
                        continue;
                    }

                    Self::send_update(
                        &liquidator_sender,
                        &rebalancer_sender,
                        AccountType::Marginfi,
                        address,
                        &account,
                    );
                }

                if let Some(account_type) = tracked_accounts.get(&address) {
                    Self::send_update(
                        &liquidator_sender,
                        &rebalancer_sender,
                        account_type.clone(),
                        address,
                        &account,
                    );
                }
            }
        }
        Ok(())
    }

    fn send_update(
        liquidator_sender: &Sender<GeyserUpdate>,
        rebalancer_sender: &Sender<GeyserUpdate>,
        account_type: AccountType,
        address: Pubkey,
        account: &Account,
    ) {
        let update = GeyserUpdate {
            account_type,
            address,
            account: account.clone(),
        };

        match update.account_type {
            AccountType::Oracle | AccountType::Marginfi => {
                if let Err(e) = liquidator_sender.send(update.clone()) {
                    error!("Error sending update to the liquidator sender: {:?}", e);
                }
                if let Err(e) = rebalancer_sender.send(update.clone()) {
                    error!("Error sending update to the rebalancer sender: {:?}", e);
                }
            }
            AccountType::Token => {
                if let Err(e) = rebalancer_sender.send(update.clone()) {
                    error!("Error sending update to the rebalancer sender: {:?}", e);
                }
            }
        }
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
