use crate::{
    cache::{Cache, CacheLoader},
    clock_manager::{self, ClockManager},
    config::Eva01Config,
    geyser::{AccountType, GeyserService, GeyserUpdate},
    liquidator::Liquidator,
    metrics::{FAILED_LIQUIDATIONS, LIQUIDATION_ATTEMPTS},
    rebalancer::Rebalancer,
    thread_info,
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
    let (jito_tx, jito_rx) = crossbeam::channel::unbounded::<(Pubkey, String)>();

    let mut transaction_manager = TransactionManager::new(config.general_config.clone())?;
    let transaction_checker =
        TransactionChecker::new(config.general_config.block_engine_url.as_str())?;

    let pending_bundles = Arc::new(RwLock::new(HashSet::<Pubkey>::new()));
    let mut liquidator = Liquidator::new(
        config.general_config.clone(),
        config.liquidator_config.clone(),
        liquidator_rx.clone(),
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
        rebalancer_rx.clone(),
        stop_liquidator.clone(),
        clock.clone(),
    )?;

    info!("Loading data.");
    let mut cache = Cache::new(
        config.general_config.marginfi_program_id,
        config.general_config.marginfi_group_address,
    );

    let cache_loader = CacheLoader::new(config.general_config.rpc_url.clone());
    cache_loader.load_marginfi_accounts(&mut cache)?;

    rebalancer.load_data(liquidator.get_banks_and_map())?;

    info!("Fetching accounts to track.");
    let mut accounts_to_track = HashMap::new();
    for (key, value) in liquidator.get_accounts_to_track() {
        accounts_to_track.insert(key, value);
    }
    for (key, value) in rebalancer.get_accounts_to_track() {
        accounts_to_track.insert(key, value);
    }
    accounts_to_track.insert(
        config.general_config.liquidator_account,
        AccountType::Marginfi,
    );

    let geyser_service = GeyserService::new(
        config.general_config,
        accounts_to_track,
        liquidator_tx,
        rebalancer_tx,
    )?;

    info!("Starting services.");

    thread::spawn(move || clock_manager.start());

    thread::spawn(move || {
        if let Err(e) = geyser_service.start() {
            panic!("Geyser service failed! {:?}", e);
        }
    });

    let tm_txn_rx = transaction_rx.clone();
    thread::spawn(move || {
        if let Err(e) = transaction_manager.start(jito_tx, tm_txn_rx) {
            panic!("TransactionManager failed! {:?}", e);
        }
    });
    let tc_jito_rx = jito_rx.clone();
    thread::spawn(move || {
        if let Err(e) = transaction_checker.start(tc_jito_rx, pending_bundles) {
            panic!("TransactionChecker failed! {:?}", e);
        }
    });

    thread::spawn(move || {
        if let Err(e) = rebalancer.start() {
            panic!("Rebalancer failed! {:?}", e);
        }
    });

    thread::spawn(move || {
        if let Err(e) = liquidator.start() {
            panic!("Liquidator failed! {:?}", e);
        }
    });

    thread_info!("Entering main loop.");
    let monitor_liquidator_rx = liquidator_rx.clone();
    let monitor_rebalancer_rx = rebalancer_rx.clone();
    let monitor_jito_rx = jito_rx.clone();
    let monitor_transaction_rx = transaction_rx.clone();
    while !stop_liquidator.load(std::sync::atomic::Ordering::SeqCst) {
        thread_info!(
            "Stats: Channel depth [Liquidation, Rebalancer, Txn, Jito] -> [{}, {}, {}, {}]",
            monitor_liquidator_rx.len(),
            monitor_rebalancer_rx.len(),
            monitor_transaction_rx.len(),
            monitor_jito_rx.len()
        );
        thread_info!(
            "Stats: Liqudations [attemps, failed] -> [{},{}]",
            LIQUIDATION_ATTEMPTS.get(),
            FAILED_LIQUIDATIONS.get()
        );
        thread::sleep(std::time::Duration::from_secs(5));
    }
    thread_info!("Main loop stopped.");

    Ok(())
}

pub fn wizard_setup() -> anyhow::Result<()> {
    crate::cli::setup::setup()?;
    Ok(())
}
