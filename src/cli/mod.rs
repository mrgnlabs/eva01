use std::collections::HashMap;

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
use prometheus::{Counter, Encoder, Gauge, Histogram, HistogramOpts, Registry, TextEncoder};
use tokio::sync::Mutex;
use warp::Filter;

lazy_static! {
    static ref REGISTRY: Registry = Registry::new();
    static ref SUCCESSFUL_LIQUIDATIONS: Counter = Counter::new(
        "eva01_successful_liquidations_total",
        "Total number of successful liquidations"
    )
    .unwrap();
    static ref FAILED_LIQUIDATIONS: Counter = Counter::new(
        "eva01_failed_liquidations_total",
        "Total number of failed liquidation attempts"
    )
    .unwrap();
    static ref ERROR_COUNT: Counter =
        Counter::new("eva01_errors_total", "Total number of errors encountered").unwrap();
    static ref LIQUIDATION_LATENCY: Histogram = Histogram::with_opts(HistogramOpts::new(
        "eva01_liquidation_latency_seconds",
        "Time taken for liquidations in seconds"
    ))
    .unwrap();
    static ref BALANCES: Mutex<HashMap<String, Gauge>> = Mutex::new(HashMap::new());
}

fn register_metrics() {
    REGISTRY
        .register(Box::new(SUCCESSFUL_LIQUIDATIONS.clone()))
        .unwrap();
    REGISTRY
        .register(Box::new(FAILED_LIQUIDATIONS.clone()))
        .unwrap();
    REGISTRY.register(Box::new(ERROR_COUNT.clone())).unwrap();
    REGISTRY
        .register(Box::new(LIQUIDATION_LATENCY.clone()))
        .unwrap();
}

fn metrics_handler() -> String {
    let encoder = TextEncoder::new();
    let mut buffer = Vec::new();
    let metric_families = REGISTRY.gather();
    encoder.encode(&metric_families, &mut buffer).unwrap();
    String::from_utf8(buffer).unwrap()
}

/// Main entrypoint for Eva
pub async fn main_entry() -> anyhow::Result<()> {
    let args = app::Args::parse();
    register_metrics();

    let metrics_route = warp::path("metrics").map(move || {
        warp::reply::with_header(
            metrics_handler(),
            "Content-Type",
            "text/plain; version=0.0.4",
        )
    });

    // Start the metrics server in a separate task
    tokio::spawn(async move {
        warp::serve(metrics_route).run(([0, 0, 0, 0], 8080)).await;
    });

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
