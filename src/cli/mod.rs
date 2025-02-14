use crate::config::Eva01Config;
use clap::Parser;
use setup::setup_from_cfg;

/// Main Clap app for the CLI
pub mod app;

/// Entrypoints for the Eva
pub mod entrypoints;

/// A wizard-like setup menu for creating the liquidator configuration
pub mod setup;

use lazy_static::lazy_static;
use prometheus::{Counter, Encoder, Registry, TextEncoder};
use warp::Filter;
use tokio::task;

lazy_static! {
    static ref REQUEST_COUNTER: Counter =
        Counter::new("eva01_requests_total", "Total number of requests received").unwrap();
}

/// Starts the metrics server asynchronously
async fn start_metrics_server() {
    let registry = Registry::new();
    registry
        .register(Box::new(REQUEST_COUNTER.clone()))
        .unwrap();

    let metrics_route = warp::path("metrics").map(move || {
        let encoder = TextEncoder::new();
        let mut buffer = Vec::new();
        let metric_families = registry.gather();
        encoder.encode(&metric_families, &mut buffer).unwrap();
        let response = String::from_utf8(buffer).unwrap();
        warp::reply::with_header(response, "Content-Type", "text/plain; version=0.0.4")
    });

    let hello = warp::path!("hello").map(|| {
        REQUEST_COUNTER.inc();
        "Hello, world!"
    });

    let routes = metrics_route.or(hello);

    println!("Starting eva01 metrics server on port 8080...");
    warp::serve(routes).run(([0, 0, 0, 0], 8080)).await;
}

/// Main entrypoint for Eva
pub async fn main_entry() -> anyhow::Result<()> {
    let args = app::Args::parse();

    // Start the metrics server in a separate task
    task::spawn(start_metrics_server());

    // Proceed with the main program
    match args.cmd {
        app::Commands::Run { path } => {
            let config = Eva01Config::try_load_from_file(path)?;
            entrypoints::run_liquidator(config).await?;
        }
        app::Commands::Setup => {
            entrypoints::wizard_setup().await?;
        }
        app::Commands::SetupFromCli(cfg) => setup_from_cfg(cfg).await?,
    }

    Ok(())
}
