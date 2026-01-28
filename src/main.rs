use anchor_lang::declare_program;
use env_logger::Builder;
use signal_hook::consts::{SIGINT, SIGTERM};
use std::{
    backtrace::Backtrace,
    error::Error,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
#[cfg(not(feature = "pretty_logs"))]
use {env_logger::Target, log::Level, std::io::Write as _};

mod cache;
mod cache_loader;
mod cli;
mod clock_manager;
mod config;
mod drift_ixs;
mod geyser;
mod geyser_processor;
mod kamino_ixs;
mod liquidator;
mod marginfi_ixs;
mod metrics;
mod rebalancer;
mod utils;
#[warn(clippy::type_complexity)]
mod wrappers;

declare_program!(kamino_lending);
declare_program!(kamino_farms);

#[allow(clippy::too_many_arguments)]
mod drift_idl {
    anchor_lang::declare_program!(drift);
}

use drift_idl::*;

fn main() -> Result<(), Box<dyn Error>> {
    init_logging();

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

    // Register signal handlers for graceful shutdown
    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGINT, stop.clone()).unwrap();
    signal_hook::flag::register(SIGTERM, stop.clone()).unwrap();

    let stop_hook = Arc::clone(&stop);
    ctrlc::set_handler(move || {
        stop_hook.store(true, Ordering::SeqCst);
        println!("Received stop signal");
    })
    .expect("Error setting Ctrl-C handler");

    // Main entrypoint
    crate::cli::main_entry(stop)?;

    Ok(())
}

fn init_logging() {
    let mut b = Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    #[cfg(feature = "pretty_logs")]
    b.init();
    #[cfg(not(feature = "pretty_logs"))]
    b.target(Target::Stdout)
        .format(|buf, record| {
            use serde_json::json;

            let sev = match record.level() {
                Level::Error => "ERROR",
                Level::Warn => "WARNING",
                Level::Info => "INFO",
                Level::Debug => "DEBUG",
                Level::Trace => "DEBUG",
            };

            let line = json!({
                "logging.googleapis.com/severity": sev,
                "message": record.args().to_string(),
                "target": record.target(),
            });

            writeln!(buf, "{}", line)
        })
        .init();
}
