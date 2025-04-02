use crate::{
    config::Eva01Config,
    geyser::{GeyserService, GeyserUpdate},
    liquidator::Liquidator,
    rebalancer::Rebalancer,
    transaction_manager::{TransactionData, TransactionManager},
};
use log::{error, info};
use solana_sdk::pubkey::Pubkey;
use std::{
    collections::HashMap,
    sync::{atomic::AtomicBool, Arc},
};

pub async fn run_liquidator(config: Eva01Config) -> anyhow::Result<()> {
    // register_metrics();

    // let metrics_route = warp::path("metrics").map(move || {
    //     warp::reply::with_header(
    //         metrics_handler(),
    //         "Content-Type",
    //         "text/plain; version=0.0.4",
    //     )
    // });

    // // Start the metrics server in a separate task
    // tokio::spawn(async move {
    //     warp::serve(metrics_route).run(([0, 0, 0, 0], 8080)).await;
    // });

    info!("Starting eva01 liquidator! {:#?}", &config);

    // Geyser -> Liquidator
    // Geyser -> Rebalancer
    // Liquidator/Rebalancer -> TransactionManager
    let (liquidator_tx, liquidator_rx) = crossbeam::channel::unbounded::<GeyserUpdate>();
    let (rebalancer_tx, rebalancer_rx) = crossbeam::channel::unbounded::<GeyserUpdate>();
    let (transaction_tx, transaction_rx) = crossbeam::channel::unbounded::<TransactionData>();
    let (ack_tx, ack_rx) = crossbeam::channel::unbounded::<Pubkey>();

    // This is to stop liquidator when rebalancer asks for it
    let stop_liquidator = Arc::new(AtomicBool::new(false));

    let mut transaction_manager =
        TransactionManager::new(transaction_rx, ack_tx, config.general_config.clone()).await?;

    let mut liquidator = Liquidator::new(
        config.general_config.clone(),
        config.liquidator_config.clone(),
        liquidator_rx.clone(),
        transaction_tx.clone(),
        ack_rx.clone(),
        stop_liquidator.clone(),
    )
    .await;

    let mut rebalancer = Rebalancer::new(
        config.general_config.clone(),
        config.rebalancer_config.clone(),
        transaction_tx.clone(),
        ack_rx,
        rebalancer_rx.clone(),
        stop_liquidator.clone(),
    )
    .await?;

    info!("Loading data for liquidator...");
    liquidator.load_data().await?;

    info!("Loading data for rebalancer...");
    rebalancer.load_data(liquidator.get_banks_and_map()).await?;

    let mut accounts_to_track = HashMap::new();
    for (key, value) in liquidator.get_accounts_to_track() {
        accounts_to_track.insert(key, value);
    }
    for (key, value) in rebalancer.get_accounts_to_track() {
        accounts_to_track.insert(key, value);
    }

    tokio::task::spawn(async move {
        if let Err(e) = GeyserService::connect(
            config.general_config.get_geyser_service_config(),
            accounts_to_track,
            config.general_config.marginfi_program_id,
            config.general_config.marginfi_group_address,
            liquidator_tx,
            rebalancer_tx,
        )
        .await
        {
            error!("Failed to connect to geyser service: {:?}", e);
        }
    });

    tokio::task::spawn(async move {
        transaction_manager.start().await;
    });

    tokio::task::spawn(async move {
        if let Err(e) = rebalancer.start().await {
            error!("Rebalancer error: {:?}", e);
        }
    });

    liquidator.start().await?;

    Ok(())
}

pub async fn wizard_setup() -> anyhow::Result<()> {
    crate::cli::setup::setup().await?;
    Ok(())
}
