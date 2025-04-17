use crate::{
    clock_manager::{self, ClockManager},
    config::Eva01Config,
    geyser::{GeyserService, GeyserUpdate},
    liquidator::Liquidator,
    rebalancer::Rebalancer,
    transaction_checker::TransactionChecker,
    transaction_manager::{TransactionData, TransactionManager},
};
use log::info;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::{
    collections::{HashMap, HashSet},
    sync::{atomic::AtomicBool, Arc, Mutex, RwLock},
    thread,
};
use tokio::runtime::Builder;

pub fn run_liquidator(config: Eva01Config) -> anyhow::Result<()> {
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

    // This is to stop liquidator when rebalancer asks for it
    let stop_liquidator = Arc::new(AtomicBool::new(false));

    // Solana Clock
    let clock = {
        let rpc_client = RpcClient::new(config.general_config.rpc_url.clone());
        Arc::new(Mutex::new(clock_manager::fetch_clock(&rpc_client)?))
    };
    let mut clock_manager = ClockManager::new(
        clock.clone(),
        config.general_config.rpc_url.clone(),
        config.general_config.solana_clock_refresh_interval,
    )?;

    // Geyser -> Liquidator
    // Geyser -> Rebalancer
    // Liquidator/Rebalancer -> TransactionManager
    let (liquidator_tx, liquidator_rx) = crossbeam::channel::unbounded::<GeyserUpdate>();
    let (rebalancer_tx, rebalancer_rx) = crossbeam::channel::unbounded::<GeyserUpdate>();
    let (transaction_tx, transaction_rx) = crossbeam::channel::unbounded::<TransactionData>();

    let mut transaction_manager = TransactionManager::new(config.general_config.clone())?;
    let transaction_checker =
        TransactionChecker::new(config.general_config.block_engine_url.as_str())?;

    let pending_bundles = Arc::new(RwLock::new(HashSet::<Pubkey>::new()));
    let mut liquidator = Liquidator::new(
        config.general_config.clone(),
        config.liquidator_config.clone(),
        liquidator_rx,
        transaction_tx.clone(),
        Arc::clone(&pending_bundles),
        stop_liquidator.clone(),
        clock.clone(),
    )?;

    let mut rebalancer = Rebalancer::new(
        config.general_config.clone(),
        config.rebalancer_config.clone(),
        transaction_tx,
        Arc::clone(&pending_bundles),
        rebalancer_rx,
        stop_liquidator.clone(),
        clock.clone(),
    )?;

    info!("Loading data for liquidator...");
    liquidator.load_data()?;

    info!("Loading data for rebalancer...");
    rebalancer.load_data(liquidator.get_banks_and_map())?;

    // Fetch accounts to track
    let mut accounts_to_track = HashMap::new();
    for (key, value) in liquidator.get_accounts_to_track() {
        accounts_to_track.insert(key, value);
    }
    for (key, value) in rebalancer.get_accounts_to_track() {
        accounts_to_track.insert(key, value);
    }

    thread::spawn(move || clock_manager.start());

    // Start Geyser service
    // TODO: encapsulate the Tokio runtime in the Geyser service
    let tokio_rt = Builder::new_multi_thread()
        .thread_name("geyser")
        .worker_threads(2)
        .enable_all()
        .build()?;
    tokio_rt.spawn(async move {
        if let Err(e) = GeyserService::start(
            config.general_config.get_geyser_service_config(),
            accounts_to_track,
            config.general_config.marginfi_program_id,
            config.general_config.marginfi_group_address,
            liquidator_tx,
            rebalancer_tx,
        )
        .await
        {
            panic!("Geyser service failed! {:?}", e);
        }
    });

    let (jito_tx, jito_rx) = crossbeam::channel::unbounded::<(Pubkey, String)>();
    thread::spawn(move || {
        if let Err(e) = transaction_manager.start(jito_tx, transaction_rx) {
            panic!("TransactionManager failed! {:?}", e);
        }
    });
    thread::spawn(move || {
        if let Err(e) = transaction_checker.start(jito_rx, pending_bundles) {
            panic!("TransactionChecker failed! {:?}", e);
        }
    });

    thread::spawn(move || {
        if let Err(e) = rebalancer.start() {
            panic!("Rebalancer failed! {:?}", e);
        }
    });

    if let Err(e) = liquidator.start() {
        panic!("Liquidator failed! {:?}", e);
    }

    Ok(())
}

pub fn wizard_setup() -> anyhow::Result<()> {
    crate::cli::setup::setup()?;
    Ok(())
}
