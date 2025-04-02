use lazy_static::lazy_static;
use prometheus::{Counter, Encoder, Gauge, Histogram, HistogramOpts, Registry, TextEncoder};
use std::collections::HashMap;
use std::sync::Mutex;

lazy_static! {
    pub static ref REGISTRY: Registry = Registry::new();
    pub static ref LIQUIDATION_ATTEMPTS: Counter = Counter::new(
        "eva01_liquidation_attempts_total",
        "Total number of liquidation attempts"
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
        .register(Box::new(LIQUIDATION_ATTEMPTS.clone()))
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

pub fn update_balance(coin: &str, new_balance: f64) {
    let mut balances = BALANCES.lock().unwrap();

    if let Some(gauge) = balances.get(coin) {
        gauge.set(new_balance);
    } else {
        // If the coin is not already being tracked, create a new Gauge
        let gauge = Gauge::new(
            format!("eva01_balance_{}", coin),
            format!("Balance of {}", coin),
        )
        .unwrap();
        gauge.set(new_balance);

        // Insert it into the registry and the HashMap
        balances.insert(coin.to_string(), gauge.clone());
        crate::metrics::REGISTRY.register(Box::new(gauge)).unwrap();
    }
}
