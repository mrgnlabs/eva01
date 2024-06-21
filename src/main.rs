use env_logger::Builder;
use std::error::Error;

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
    // Assemble logger
    Builder::from_default_env().init();

    // Main entrypoint
    crate::cli::main_entry().await?;

    Ok(())
}
