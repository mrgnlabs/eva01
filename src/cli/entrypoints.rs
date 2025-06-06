use crate::{
    cache::Cache,
    cache_loader::{get_accounts_to_track, CacheLoader},
    clock_manager::{self, ClockManager},
    config::Eva01Config,
    geyser::{GeyserService, GeyserUpdate},
    geyser_processor::GeyserProcessor,
    liquidator::Liquidator,
    metrics::{FAILED_LIQUIDATIONS, LIQUIDATION_ATTEMPTS},
    rebalancer::Rebalancer,
    thread_error, thread_info,
    transaction_checker::TransactionChecker,
    transaction_manager::{TransactionData, TransactionManager},
    utils::swb_cranker::SwbCranker,
    wrappers::liquidator_account::LiquidatorAccount,
};
use log::info;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::{
    collections::HashSet,
    sync::{atomic::AtomicBool, Arc, Mutex, RwLock},
    thread,
};

pub fn run_liquidator(
    mut config: Eva01Config,
    marginfi_group_id: Pubkey,
    preferred_mints: Arc<RwLock<HashSet<Pubkey>>>,
) -> anyhow::Result<()> {
    info!("Starting liquidator for group: {:?}", marginfi_group_id);
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

    info!("Loading Cache...");
    let mut cache = Cache::new(
        config.general_config.signer_pubkey,
        config.general_config.marginfi_program_id,
        marginfi_group_id,
        clock.clone(),
        preferred_mints,
    );

    let cache_loader = CacheLoader::new(
        config.general_config.keypair_path.clone(),
        config.general_config.rpc_url.clone(),
    )?;
    cache_loader.load_cache(&mut cache)?;

    // Check if the preferred asset is in the cache. If not, make the first one the preferred asset.
    let mints = cache.mints.get_mints();
    if !mints
        .iter()
        .any(|&mint| mint == &config.rebalancer_config.swap_mint)
    {
        config.rebalancer_config.swap_mint = *mints[0]; // TODO: come up with a smarter logic for choosing the preferred asset
        info!("Configured preferred asset not found in cache, using the first one from cache as preferred: {}", config.rebalancer_config.swap_mint);
    }

    cache.insert_preferred_mint(config.rebalancer_config.swap_mint);

    let accounts_to_track = get_accounts_to_track(&cache)?;

    let cache = Arc::new(cache);

    info!("Initializing services...");

    // GeyserService -> GeyserProcessor
    // GeyserProcessor -> Liquidator/Rebalancer
    // Liquidator/Rebalancer -> TransactionManager
    let (geyser_tx, geyser_rx) = crossbeam::channel::unbounded::<GeyserUpdate>();
    let run_liquidation = Arc::new(AtomicBool::new(false));
    let run_rebalance = Arc::new(AtomicBool::new(false));
    let (transaction_tx, transaction_rx) = crossbeam::channel::unbounded::<TransactionData>();
    let (jito_tx, jito_rx) = crossbeam::channel::unbounded::<(Pubkey, String)>();

    let mut transaction_manager = TransactionManager::new(config.general_config.clone())?;
    let transaction_checker = TransactionChecker::new(
        config.general_config.block_engine_url.as_str(),
        config.general_config.block_engine_uuid.clone(),
    )?;

    let swb_price_simulator = Arc::new(SwbCranker::new(cache.clone())?);
    let pending_bundles = Arc::new(RwLock::new(HashSet::<Pubkey>::new()));

    let mut newly_created = false;
    let liquidator_account = LiquidatorAccount::new(
        transaction_tx.clone(),
        &config.general_config,
        marginfi_group_id,
        Arc::clone(&pending_bundles),
        cache.clone(),
        &mut newly_created,
    )?;
    let liquidator_account_clone = liquidator_account.clone()?;

    let mut liquidator = Liquidator::new(
        config.general_config.clone(),
        config.liquidator_config.clone(),
        liquidator_account,
        run_liquidation.clone(),
        stop_liquidator.clone(),
        cache.clone(),
        swb_price_simulator.clone(),
    )?;

    let mut rebalancer = Rebalancer::new(
        config.general_config.clone(),
        config.rebalancer_config.clone(),
        liquidator_account_clone,
        run_rebalance.clone(),
        stop_liquidator.clone(),
        cache.clone(),
        swb_price_simulator.clone(),
    )?;

    if newly_created {
        rebalancer.fund_liquidator_account()?;
    }

    let geyser_service = GeyserService::new(
        config.general_config,
        marginfi_group_id,
        accounts_to_track,
        geyser_tx,
        stop_liquidator.clone(),
        clock.clone(),
    )?;

    let geyser_processor = GeyserProcessor::new(
        geyser_rx.clone(),
        run_liquidation.clone(),
        run_rebalance.clone(),
        stop_liquidator.clone(),
        cache.clone(),
    )?;

    info!("Starting services...");

    thread::spawn(move || clock_manager.start());

    let tm_txn_rx = transaction_rx.clone();
    thread::spawn(move || {
        if let Err(e) = transaction_manager.start(jito_tx, tm_txn_rx) {
            thread_error!("TransactionManager failed! {:?}", e);
            panic!("Fatal error in TransactionManager!");
        }
    });
    let tc_jito_rx = jito_rx.clone();
    thread::spawn(move || {
        if let Err(e) = transaction_checker.start(tc_jito_rx, pending_bundles) {
            thread_error!("TransactionChecker failed! {:?}", e);
            panic!("Fatal error in TransactionChecker!");
        }
    });

    thread::spawn(move || {
        if let Err(e) = rebalancer.start() {
            thread_error!("Rebalancer failed! {:?}", e);
            panic!("Fatal error in Rebalancer!");
        }
    });

    thread::spawn(move || {
        if let Err(e) = liquidator.start() {
            thread_error!("The Liquidator service failed! {:?}", e);
            panic!("Fatal error in the Liquidator service!");
        }
    });

    thread::spawn(move || {
        if let Err(e) = geyser_processor.start() {
            thread_error!("GeyserProcessor failed! {:?}", e);
            panic!("Fatal error in GeyserProcessor!");
        }
    });
    thread::spawn(move || {
        if let Err(e) = geyser_service.start() {
            thread_error!("GeyserService failed! {:?}", e);
            panic!("Fatal error in GeyserService!");
        }
    });

    thread_info!("Entering the Main loop.");
    let monitor_geyser_rx = geyser_rx.clone();
    let monitor_jito_rx = jito_rx.clone();
    let monitor_transaction_rx = transaction_rx.clone();
    while !stop_liquidator.load(std::sync::atomic::Ordering::SeqCst) {
        thread_info!(
            "Stats: Channel depth [Geyser, Txn, Jito] -> [{}, {}, {}]",
            monitor_geyser_rx.len(),
            monitor_transaction_rx.len(),
            monitor_jito_rx.len()
        );
        thread_info!(
            "Stats: Liqudations [attempts, failed] -> [{},{}]",
            LIQUIDATION_ATTEMPTS.get(),
            FAILED_LIQUIDATIONS.get()
        );
        thread::sleep(std::time::Duration::from_secs(5));
    }
    thread_info!("The Main loop stopped.");

    Ok(())
}

pub fn wizard_setup() -> anyhow::Result<()> {
    crate::cli::setup::setup()?;
    Ok(())
}
