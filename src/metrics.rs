use lazy_static::lazy_static;
use prometheus::{Counter, Histogram, HistogramOpts, Registry};

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
}
