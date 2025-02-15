use lazy_static::lazy_static;
use prometheus::{Counter, Encoder, Gauge, Histogram, HistogramOpts, Registry, TextEncoder};
use std::collections::HashMap;
use tokio::sync::Mutex;

lazy_static! {
    pub static ref REGISTRY: Registry = Registry::new();
    pub static ref SUCCESSFUL_LIQUIDATIONS: Counter = Counter::new(
        "eva01_successful_liquidations_total",
        "Total number of successful liquidations"
    )
    .unwrap();
    pub static ref FAILED_LIQUIDATIONS: Counter = Counter::new(
        "eva01_failed_liquidations_total",
        "Total number of failed liquidation attempts"
    )
    .unwrap();
    pub static ref ERROR_COUNT: Counter =
        Counter::new("eva01_errors_total", "Total number of errors encountered").unwrap();
    pub static ref LIQUIDATION_LATENCY: Histogram = Histogram::with_opts(HistogramOpts::new(
        "eva01_liquidation_latency_seconds",
        "Time taken for liquidations in seconds"
    ))
    .unwrap();
    pub static ref BALANCES: Mutex<HashMap<String, Gauge>> = Mutex::new(HashMap::new());
}

pub fn register_metrics() {
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

pub fn metrics_handler() -> String {
    let encoder = TextEncoder::new();
    let mut buffer = Vec::new();
    let metric_families = REGISTRY.gather();
    encoder.encode(&metric_families, &mut buffer).unwrap();
    String::from_utf8(buffer).unwrap()
}
