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
    wrappers::liquidator_account::LiquidatorAccount,
};
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

    // TODO: refactor this fake cache. Currently it's only needed to construct the LiquidatorAccount object.
    // After construction it's replaced with the actual cache.
    // This way we ensure that the potentially created liquidator (MarginFi) account is picked up when the cache is loaded.
    let dummy_unused_cache = Cache::new(
        config.general_config.signer_pubkey,
        config.general_config.marginfi_program_id,
        marginfi_group_id,
        clock.clone(),
        Arc::clone(&preferred_mints),
    );

    // If there is no liquidator account for this group, we'll create it.
    let mut new_liquidator_account: Option<Pubkey> = None;
    let mut liquidator_account = LiquidatorAccount::new(
        &config.general_config,
        marginfi_group_id,
        Arc::new(dummy_unused_cache),
        &mut new_liquidator_account,
    )?;

    thread_info!("Loading Cache...");
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
        config.general_config.clone().address_lookup_tables,
    )?;
    cache_loader.load_cache(&mut cache, &new_liquidator_account)?;

    // Check if the preferred asset is in the cache. If not, make the first one the preferred asset.
    let mints = cache.mints.get_mints();
    if !mints
        .iter()
        .any(|&mint| mint == &config.rebalancer_config.swap_mint)
    {
        config.rebalancer_config.swap_mint = *mints[0]; // TODO: come up with a smarter logic for choosing the preferred asset
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
    let run_rebalance = Arc::new(AtomicBool::new(false));

    let cache = Arc::new(cache);
    liquidator_account.cache = Arc::clone(&cache);
    let liquidator_account_clone = liquidator_account.clone()?;
    let mut liquidator = Liquidator::new(
        config.general_config.clone(),
        config.liquidator_config.clone(),
        liquidator_account,
        run_liquidation.clone(),
        stop_liquidator.clone(),
        Arc::clone(&cache),
    )?;

    let mut rebalancer = Rebalancer::new(
        config.general_config.clone(),
        config.rebalancer_config.clone(),
        liquidator_account_clone,
        run_rebalance.clone(),
        stop_liquidator.clone(),
        Arc::clone(&cache),
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
        run_rebalance.clone(),
        stop_liquidator.clone(),
        cache,
    )?;

    thread_info!("Starting services...");

    thread::spawn(move || clock_manager.start());

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

    if new_liquidator_account.is_some() {
        rebalancer.fund_liquidator_account()?;
    }

    thread::spawn(move || {
        if let Err(e) = rebalancer.start() {
            thread_error!("Rebalancer failed! {:?}", e);
            panic!("Fatal error in Rebalancer!");
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
