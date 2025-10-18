use std::{
    sync::{atomic::AtomicBool, Arc},
    thread::{self},
};

use crate::{
    config::Eva01Config,
    utils::healthcheck::{HealthCheckServer, HealthState},
};
use log::info;
use std::sync::Mutex;

/// Entrypoints for the Eva
pub mod entrypoints;

/// Main entrypoint for Eva
pub fn main_entry(stop: Arc<AtomicBool>) -> anyhow::Result<()> {
    let config = Eva01Config::new()?;
    info!("Starting eva01 liquidator! {}", &config);

    let health_state = Arc::new(Mutex::new(HealthState::Initializing));
    let healthcheck_server = HealthCheckServer::new(
        config.general_config.healthcheck_port,
        health_state.clone(),
        stop.clone(),
    );
    thread::spawn(move || healthcheck_server.start());

    *health_state
        .lock()
        .expect("Failed to acquire the health_state lock") = HealthState::Running;

    entrypoints::run_liquidator(config, stop.clone())
}
