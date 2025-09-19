use crate::{
    clock_manager, config::GeneralConfig, thread_error, thread_info, thread_trace,
    utils::account_update_to_account, ward,
};
use anchor_lang::AccountDeserialize;
use anyhow::Result;
use crossbeam::channel::Sender;
use futures::StreamExt;
use marginfi_type_crate::types::MarginfiAccount;
use solana_program::pubkey::Pubkey;
use solana_sdk::{account::Account, clock::Clock};
use std::{
    collections::HashMap,
    mem::size_of,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tokio::runtime::{Builder, Runtime};
use yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient};
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
pub struct GeyserService {
    endpoint: String,
    x_token: Option<String>,
    tracked_accounts: HashMap<Pubkey, AccountType>,
    marginfi_program_id: Pubkey,
    marginfi_group_pk: Pubkey,
    geyser_tx: Sender<GeyserUpdate>,
    tokio_rt: Runtime,
    stop: Arc<AtomicBool>,
    clock: Arc<Mutex<Clock>>,
}

impl GeyserService {
    pub fn new(
        config: GeneralConfig,
        tracked_accounts: HashMap<Pubkey, AccountType>,
        geyser_tx: Sender<GeyserUpdate>,
        stop: Arc<AtomicBool>,
        clock: Arc<Mutex<Clock>>,
    ) -> Result<Self> {
        let tokio_rt = Builder::new_multi_thread()
            .thread_name("GeyserService")
            .worker_threads(2)
            .enable_all()
            .build()?;

        let geyser_config = config.get_geyser_service_config();

        Ok(Self {
            endpoint: geyser_config.endpoint,
            x_token: geyser_config.x_token,
            tracked_accounts,
            marginfi_program_id: config.marginfi_program_id,
            marginfi_group_pk: config.marginfi_group_key,
            geyser_tx,
            tokio_rt,
            stop,
            clock,
        })
    }

    pub fn start(&self) -> Result<()> {
        thread_info!("Staring GeyserService.");

        let tracked_accounts_vec: Vec<Pubkey> = self.tracked_accounts.keys().copied().collect();
        let tls_config = ClientTlsConfig::new().with_native_roots();

        while !self.stop.load(Ordering::Relaxed) {
            thread_info!("Connecting to Geyser...");
            let sub_req = Self::build_geyser_subscribe_request(
                &tracked_accounts_vec,
                &self.marginfi_program_id,
            );
            let mut client = self.tokio_rt.block_on(
                GeyserGrpcClient::build_from_shared(self.endpoint.clone())?
                    .x_token(self.x_token.clone())?
                    .tls_config(tls_config.clone())?
                    .connect(),
            )?;

            let (_, mut stream) = self
                .tokio_rt
                .block_on(client.subscribe_with_request(Some(sub_req.clone())))?;

            thread_info!("Entering the GeyserService loop.");
            while let Some(msg) = self.tokio_rt.block_on(stream.next()) {
                match msg {
                    Ok(msg) => {
                        thread_trace!("Received Geyser msg: {:?}", msg);
                        let update_oneof = ward!(msg.update_oneof, continue);
                        if let subscribe_update::UpdateOneof::Account(account) = update_oneof {
                            // Make sure that the message is not too old.
                            let clock = clock_manager::get_clock(&self.clock)?;
                            if account.slot < clock.slot {
                                thread_trace!("Discarding stale message {:?}.", account);
                                continue;
                            }

                            // TODO: need more elaborate message payload check, it is hiding invalid messages.
                            let account_update = ward!(&account.account, continue);
                            let account =
                                ward!(account_update_to_account(account_update).ok(), continue);
                            let address = ward!(
                                Pubkey::try_from(account_update.pubkey.clone()).ok(),
                                continue
                            );

                            if account.owner == self.marginfi_program_id
                                && account_update.data.len() == MARGIN_ACCOUNT_SIZE
                            {
                                let marginfi_account = ward!(
                                    MarginfiAccount::try_deserialize(&mut account.data.as_slice())
                                        .ok(),
                                    continue
                                );

                                if marginfi_account.group != self.marginfi_group_pk {
                                    continue;
                                }

                                self.send_update(AccountType::Marginfi, address, &account);
                            } else if let Some(account_type) = self.tracked_accounts.get(&address) {
                                self.send_update(account_type.clone(), address, &account);
                            }
                        }
                    }
                    Err(error) => {
                        thread_error!("Received error message from Geyser! {:?}", error);
                    }
                }

                // Breaking the loop on stop request
                if self.stop.load(Ordering::Relaxed) {
                    break;
                }
            }
        }
        thread_info!("The GeyserService loop is stopped.");

        Ok(())
    }

    fn send_update(&self, account_type: AccountType, address: Pubkey, account: &Account) {
        let update = GeyserUpdate {
            account_type,
            address,
            account: account.clone(),
        };
        if let Err(e) = self.geyser_tx.send(update) {
            thread_error!("Error channeling update to the Geyser processor! {:?}", e);
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
