use crate::{
    cache::Cache,
    cache_loader::{get_accounts_to_track, CacheLoader},
    clock_manager::{self, ClockManager},
    config::Eva01Config,
    geyser::{GeyserService, GeyserUpdate},
    geyser_processor::GeyserProcessor,
    liquidator::Liquidator,
    metrics::{FAILED_LIQUIDATIONS, LIQUIDATION_ATTEMPTS},
    thread_error, thread_info,
    wrappers::liquidator_account::LiquidatorAccount,
};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{pubkey::Pubkey, signature::Keypair, signer::Signer};
use std::{
    collections::HashSet,
    sync::{atomic::AtomicBool, Arc, Mutex, RwLock},
    thread,
};

pub fn run_liquidator(
    mut config: Eva01Config,
    marginfi_group_id: Pubkey,
    preferred_mints: Arc<RwLock<HashSet<Pubkey>>>,
    stop_liquidator: Arc<AtomicBool>,
    run_initial_rebalancing: bool,
) -> anyhow::Result<()> {
    thread_info!("Starting liquidator for group: {:?}", marginfi_group_id);
    // TODO: re-enable. https://linear.app/marginfi/issue/LIQ-13/reenable-grafana-metrics-reporting
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

    let wallet_pubkey = Keypair::from_bytes(&config.general_config.wallet_keypair)?.pubkey();
    thread_info!("Liquidator public key: {}", wallet_pubkey);

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

    thread_info!("Loading Cache...");
    let mut cache = Cache::new(
        wallet_pubkey,
        config.general_config.marginfi_program_id,
        marginfi_group_id,
        clock.clone(),
        preferred_mints,
    );

    let cache_loader = CacheLoader::new(
        &config.general_config.wallet_keypair,
        config.general_config.rpc_url.clone(),
        config.general_config.clone().address_lookup_tables,
    )?;
    cache_loader.load_cache(&mut cache)?;

    // Check if the preferred asset is in the cache. If not, make the first one the preferred asset.
    let mints = cache.mints.get_mints();
    if !mints
        .iter()
        .any(|mint| mint == &config.rebalancer_config.swap_mint)
    {
        config.rebalancer_config.swap_mint = mints[0]; // TODO: come up with a smarter logic for choosing the preferred asset
        thread_info!("Configured preferred asset not found in cache, using the first one from cache as preferred: {}", config.rebalancer_config.swap_mint);
    }

    cache.insert_preferred_mint(config.rebalancer_config.swap_mint);

    let accounts_to_track = get_accounts_to_track(&cache)?;

    thread_info!("Initializing services...");

    // GeyserService -> GeyserProcessor
    // GeyserProcessor -> Liquidator/Rebalancer
    // Liquidator/Rebalancer -> TransactionManager
    let (geyser_tx, geyser_rx) = crossbeam::channel::unbounded::<GeyserUpdate>();
    let run_liquidation = Arc::new(AtomicBool::new(false));

    let cache = Arc::new(cache);

    let liquidator_account = Arc::new(LiquidatorAccount::new(
        &config.general_config.clone(),
        marginfi_group_id,
        config.rebalancer_config.swap_mint,
        cache.clone(),
    )?);

    let mut liquidator = Liquidator::new(
        config.clone(),
        liquidator_account.clone(),
        run_liquidation.clone(),
        stop_liquidator.clone(),
        cache.clone(),
    )?;

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
        stop_liquidator.clone(),
        cache,
    )?;

    thread_info!("Starting services...");

    let cloned_stop = stop_liquidator.clone();
    thread::spawn(move || clock_manager.start(cloned_stop));

    thread::spawn(move || {
        if let Err(e) = liquidator.start(run_initial_rebalancing) {
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
    while !stop_liquidator.load(std::sync::atomic::Ordering::SeqCst) {
        thread_info!("Stats: Geyser Channel depth [{}]", monitor_geyser_rx.len(),);
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
