use std::{
    collections::HashSet,
    thread::{self, sleep},
    time::Duration,
};

use crate::config::Eva01Config;
use clap::Parser;
use log::{error, info};
use setup::marginfi_groups_by_program;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;

/// Main Clap app for the CLI
pub mod app;

/// Entrypoints for the Eva
pub mod entrypoints;

/// A wizard-like setup menu for creating the liquidator configuration
pub mod setup;

/// Main entrypoint for Eva
pub fn main_entry() -> anyhow::Result<()> {
    let args = app::Args::parse();

    match args.cmd {
        app::Commands::Run { path } => {
            let mut config = Eva01Config::try_load_from_file(path)?;
            info!("Starting eva01 liquidator! {:#?}", &config);

            if let Some(mut whitelist) = config.general_config.marginfi_groups_whitelist.take() {
                // The last group will be started in the main thread to decrease total thread count
                let last_group = whitelist.pop().unwrap();
                whitelist.into_iter().for_each(|group| {
                    start_liquidator_in_separate_thread(&config, group);
                });
                return entrypoints::run_liquidator(config, last_group);
            }

            // Now this is a multi-threaded liquidator logic which will monitor new groups creation
            // and spawn a new liquidator for each except the ones listed in the blacklist.
            let blacklist = config
                .general_config
                .marginfi_groups_blacklist
                .take()
                .unwrap();

            // This is a set of MarginFi groups (pubkeys) for which we have already spawned a liquidator.
            // Initially, we fill it with the groups from the blacklist so that we don't spawn liquidators for them.
            let mut active_groups = blacklist.into_iter().collect::<HashSet<Pubkey>>();

            let rpc_client = RpcClient::new(config.general_config.rpc_url.clone());
            loop {
                let marginfi_groups = marginfi_groups_by_program(
                    &rpc_client,
                    config.general_config.marginfi_program_id,
                    true,
                )?;

                for group in marginfi_groups {
                    if active_groups.contains(&group) || active_groups.len() == 2 {
                        continue;
                    }

                    start_liquidator_in_separate_thread(&config, group);
                    active_groups.insert(group);
                }

                sleep(Duration::from_secs(60));
            }
        }
        app::Commands::Setup => {
            entrypoints::wizard_setup()?;
        }
    }

    Ok(())
}

fn start_liquidator_in_separate_thread(config: &Eva01Config, group: Pubkey) {
    let config = config.clone();
    thread::spawn(move || {
        if let Err(e) = entrypoints::run_liquidator(config, group) {
            error!("Liquidator for group {:?} failed: {:?}", group, e);
            panic!("Fatal error in Liquidator!");
        }
    });
}
