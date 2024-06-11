use crate::{
    config::Eva01Config,
    geyser::{GeyserService, GeyserUpdate},
    liquidator::Liquidator,
    rebalancer::Rebalancer,
};
use log::info;
use std::collections::HashMap;

pub async fn run_liquidator(config: Eva01Config) -> anyhow::Result<()> {
    info!("Starting eva01 liquidator!");

    // Create two channels
    // Geyser -> Liquidator
    // Geyser -> Rebalancer
    let (liquidator_tx, liquidator_rx) = crossbeam::channel::unbounded::<GeyserUpdate>();
    let (rebalancer_tx, rebalancer_rx) = crossbeam::channel::unbounded::<GeyserUpdate>();

    // Create the liquidator
    let mut liquidator = Liquidator::new(
        config.general_config.clone(),
        config.liquidator_config.clone(),
        liquidator_rx.clone(),
    )
    .await;

    // Create the rebalancer
    let mut rebalancer = Rebalancer::new(
        config.general_config.clone(),
        config.rebalancer_config.clone(),
        rebalancer_rx.clone(),
    )
    .await?;

    liquidator.load_data().await?;
    rebalancer.load_data(liquidator.get_banks_and_map())?;

    let mut accounts_to_track = HashMap::new();
    for (key, value) in liquidator.get_accounts_to_track() {
        accounts_to_track.insert(key, value);
    }
    for (key, value) in rebalancer.get_accounts_to_track() {
        accounts_to_track.insert(key, value);
    }

    let geyser_handle = GeyserService::connect(
        config.general_config.get_geyser_service_config(),
        accounts_to_track,
        config.general_config.marginfi_program_id,
        liquidator_tx,
        rebalancer_tx,
    )
    .await?;

    tokio::task::spawn(async move {
        let _ = geyser_handle.await;
    });

    tokio::task::spawn(async move {
        let _ = rebalancer.start().await;
    });

    liquidator.start().await?;

    Ok(())
}

pub async fn wizard_setup() -> anyhow::Result<()> {
    crate::cli::setup::setup().await?;
    Ok(())
}
