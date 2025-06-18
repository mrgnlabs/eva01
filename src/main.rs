use env_logger::Builder;
use std::{backtrace::Backtrace, error::Error};

/// Prometheus metrics
mod metrics;

/// Geyser service
mod geyser;
mod geyser_processor;

/// IX's for marginfi
mod marginfi_ixs;

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

/// Crossbar client
mod crossbar;

/// Solana Clock manager
mod clock_manager;

mod cache;
mod cache_loader;

fn main() -> Result<(), Box<dyn Error>> {
    // Assemble logger, with INFO as default log level
    Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    std::panic::set_hook(Box::new(|panic_info| {
        eprintln!("Panic occurred: {:#?}", panic_info);

        if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            eprintln!("Payload: {}", s);
        } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            eprintln!("Payload: {}", s);
        } else if let Some(err) = panic_info.payload().downcast_ref::<anyhow::Error>() {
            eprintln!("Payload: {:?}", err);
        } else {
            eprintln!("Payload: (unknown type)");
        }

        eprintln!("Backtrace: {}", Backtrace::capture());

        std::process::exit(1);
    }));

    // Main entrypoint
    crate::cli::main_entry()?;

    Ok(())
}
