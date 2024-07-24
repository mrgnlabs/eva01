use env_logger::Builder;
use std::{backtrace::Backtrace, error::Error};

/// Geyser service
mod geyser;

/// IX's for marginfi
mod marginfi_ixs;

/// Responsible for sending transactions for the blockchain
mod sender;

/// Manages token accounts under liquidator account
mod token_account_manager;

/// Liquidator is responsible to liquidate MarginfiAccounts
mod liquidator;

/// Rebalancer is responsible to rebalance the liquidator account
mod rebalancer;

/// Wrappers around marginfi structs
#[warn(clippy::type_complexity)]
mod wrappers;

/// Utilities used by Eva01
mod utils;

/// CLI configuration for the Eva01
mod cli;

/// Configuration strectures for Eva01
mod config;

/// Transactio manager
mod transaction_manager;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Assemble logger, with INFO as default log level
    Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    std::panic::set_hook(Box::new(|panic_info| {
        eprintln!("Panic occurred: {:#?}", panic_info);

        eprintln!("Backtrace: {}", Backtrace::capture());

        std::process::exit(1);
    }));

    // Main entrypoint
    crate::cli::main_entry().await?;

    Ok(())
}
