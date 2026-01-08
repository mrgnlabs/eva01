use crate::{
    clock_manager, config::Eva01Config, metrics::ERROR_COUNT, utils::account_update_to_account,
    ward,
};
use anchor_lang::AccountDeserialize;
use anyhow::Result;
use crossbeam::channel::Sender;
use futures::StreamExt;
use log::{error, info, trace, warn};
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
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::runtime::{Builder, Runtime};
use yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient};
use yellowstone_grpc_proto::prelude::*;

const MARGIN_ACCOUNT_SIZE: usize = size_of::<MarginfiAccount>() + 8;
const RATE_LIMIT_LOG_INTERVAL_SECS: u64 = 60;
const MAX_ERRORS_PER_MINUTE: u64 = 5;

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

/// Rate-limited logger that logs messages at most once per minute.
/// Tracks error count per minute and returns true if the error rate exceeds the threshold.
struct RateLimitedLogger {
    // Tracks the last minute bucket we logged for (None = never logged)
    last_logged_minute: Arc<Mutex<Option<u64>>>,
    // Tracks errors per minute bucket: (minute_bucket, error_count)
    error_tracker: Arc<Mutex<(u64, u64)>>,
}

impl RateLimitedLogger {
    fn new() -> Self {
        Self {
            last_logged_minute: Arc::new(Mutex::new(None)),
            error_tracker: Arc::new(Mutex::new((0, 0))),
        }
    }

    /// Logs the error message if enough time has passed since the last log.
    /// Tracks errors per minute and returns true if we should break the connection
    /// (more than MAX_ERRORS_PER_MINUTE errors in a minute).
    fn log_error_and_check_break(&self, message: &str) -> bool {
        // Increment error counter for metrics
        ERROR_COUNT.inc();

        let now = Instant::now();
        let window_duration = Duration::from_secs(RATE_LIMIT_LOG_INTERVAL_SECS);

        // Calculate current minute bucket (seconds since epoch / 60)
        let current_minute_bucket = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            / RATE_LIMIT_LOG_INTERVAL_SECS;

        // Update error count for current minute and check threshold
        let (should_break, error_count) = {
            let mut tracker = self.error_tracker.lock().unwrap();
            let (minute_bucket, error_count) = *tracker;

            // If we're in a new minute, reset the counter
            let error_count = if minute_bucket != current_minute_bucket {
                *tracker = (current_minute_bucket, 1);
                1
            } else {
                *tracker = (minute_bucket, error_count + 1);
                error_count + 1
            };

            let should_break = error_count > MAX_ERRORS_PER_MINUTE;
            (should_break, error_count)
        };

        // Check if we should log (rate-limited to once per minute bucket)
        // Log when we enter a new minute bucket, or immediately on first error
        let should_log = {
            let mut last_logged_minute = self.last_logged_minute.lock().unwrap();

            let should_log = match *last_logged_minute {
                Some(last_minute) => current_minute_bucket != last_minute,
                None => true, // First log: log immediately
            };

            if should_log {
                *last_logged_minute = Some(current_minute_bucket);
                true
            } else {
                false
            }
        };

        if should_log {
            if error_count > 1 {
                warn!("{} ({} errors in the current minute)", message, error_count);
            } else {
                warn!("{}", message);
            }
        }

        should_break
    }
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
    error_logger: RateLimitedLogger,
}

impl GeyserService {
    pub fn new(
        config: Eva01Config,
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

        Ok(Self {
            endpoint: config.yellowstone_endpoint,
            x_token: config.yellowstone_x_token,
            tracked_accounts,
            marginfi_program_id: config.marginfi_program_id,
            marginfi_group_pk: config.marginfi_group_key,
            geyser_tx,
            tokio_rt,
            stop,
            clock,
            error_logger: RateLimitedLogger::new(),
        })
    }

    pub fn start(&self) -> Result<()> {
        info!("Staring GeyserService.");

        let tracked_accounts_vec: Vec<Pubkey> = self.tracked_accounts.keys().copied().collect();
        let tls_config = ClientTlsConfig::new().with_native_roots();

        while !self.stop.load(Ordering::Relaxed) {
            info!("Connecting to Geyser...");
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

            info!("Entering the GeyserService loop.");
            while let Some(msg) = self.tokio_rt.block_on(stream.next()) {
                match msg {
                    Ok(msg) => {
                        trace!("Received Geyser msg: {:?}", msg);
                        let update_oneof = ward!(msg.update_oneof, continue);
                        if let subscribe_update::UpdateOneof::Account(account) = update_oneof {
                            // Make sure that the message is not too old.
                            let clock = clock_manager::get_clock(&self.clock)?;
                            if account.slot < clock.slot {
                                trace!("Discarding stale message {:?}.", account);
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
                        // Log with rate limiting and check if we should break
                        let should_break = self.error_logger.log_error_and_check_break(&format!(
                            "Received error message from Geyser, reconnecting: {:?}",
                            error
                        ));

                        // Break the inner loop to force reconnection
                        // The outer loop will reconnect automatically
                        if should_break {
                            warn!(
                                "Too many errors (>{}) in one minute, forcing reconnection",
                                MAX_ERRORS_PER_MINUTE
                            );
                        }
                        break;
                    }
                }

                // Breaking the loop on stop request
                if self.stop.load(Ordering::Relaxed) {
                    break;
                }
            }
        }
        info!("The GeyserService loop is stopped.");

        Ok(())
    }

    fn send_update(&self, account_type: AccountType, address: Pubkey, account: &Account) {
        let update = GeyserUpdate {
            account_type,
            address,
            account: account.clone(),
        };
        if let Err(e) = self.geyser_tx.send(update) {
            error!("Error channeling update to the Geyser processor! {:?}", e);
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
