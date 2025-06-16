use std::{
    collections::HashSet,
    sync::{Arc, RwLock},
    thread::{self, sleep},
    time::Duration,
};

use crate::{cli::setup::get_active_arena_pools, config::Eva01Config};
use log::{error, info};
use setup::marginfi_groups_by_program;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;

/// Entrypoints for the Eva
pub mod entrypoints;

/// A wizard-like setup menu for creating the liquidator configuration
pub mod setup;

/// Main entrypoint for Eva
pub fn main_entry() -> anyhow::Result<()> {
    let config = Eva01Config::new()?;
    let preferred_mints = Arc::new(RwLock::new(HashSet::new()));
    info!("Starting eva01 liquidator! {:#?}", &config);

    // Now this is a multi-threaded liquidator logic which will monitor new groups creation
    // and spawn a new liquidator for each except the ones listed in the blacklist.
    // Note that it is not allowed to have both whitelist and blacklist at the same time!

    let mut whitelist = config.general_config.marginfi_groups_whitelist.clone();
    if !whitelist.is_empty() {
        // The last group will be started in the main thread to decrease total thread count
        let last_group = whitelist.pop().unwrap();
        for group in whitelist {
            start_liquidator_in_separate_thread(&config, group, Arc::clone(&preferred_mints));
        }
        return entrypoints::run_liquidator(config, last_group, Arc::clone(&preferred_mints));
    }

    let blacklist = config.general_config.marginfi_groups_blacklist.clone();
    if !blacklist.is_empty() {
        // This is a set of MarginFi groups (pubkeys) for which we have already spawned a liquidator.
        // Initially, we fill it with the groups from the blacklist so that we don't spawn liquidators for them.
        let mut active_groups = blacklist.into_iter().collect::<HashSet<Pubkey>>();

        let rpc_client = RpcClient::new(config.general_config.rpc_url.clone());
        loop {
            let marginfi_groups =
                if let Some(api_key) = config.general_config.marginfi_api_key.as_ref() {
                    get_active_arena_pools(
                        config.general_config.marginfi_api_url.as_ref().unwrap(),
                        api_key,
                        config.general_config.marginfi_api_arena_threshold.unwrap(),
                    )?
                } else {
                    marginfi_groups_by_program(
                        &rpc_client,
                        config.general_config.marginfi_program_id,
                        true,
                    )?
                };

            for group in marginfi_groups {
                if active_groups.contains(&group) {
                    continue;
                }

                start_liquidator_in_separate_thread(&config, group, Arc::clone(&preferred_mints));
                active_groups.insert(group);
            }

            sleep(Duration::from_secs(60));
        }
    }

    Ok(())
}

fn start_liquidator_in_separate_thread(
    config: &Eva01Config,
    group: Pubkey,
    preferred_mints: Arc<RwLock<HashSet<Pubkey>>>,
) {
    let config = config.clone();
    thread::spawn(move || {
        if let Err(e) = entrypoints::run_liquidator(config, group, preferred_mints) {
            error!("Liquidator for group {:?} failed: {:?}", group, e);
            panic!("Fatal error in Liquidator!");
        }
    });
}
