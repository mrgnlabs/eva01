use env_logger::Builder;
use log::{debug, LevelFilter};
use solana_sdk::pubkey::Pubkey;
use state_engine::engine::StateEngineService;
use std::str::FromStr;
use tokio::task;

use crate::state_engine::engine::StateEngineConfig;

mod state_engine;
mod utils;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Assemble logger
    let mut builder = Builder::new();
    builder.filter(None, LevelFilter::Debug).init();

    // Gather configuration
    let config = StateEngineConfig {
        rpc_url: std::env::var("RPC_URL").expect("Expected RPC_URL to be set in env"),
        yellowstone_endpoint: std::env::var("YELLOWSTONE_ENDPOINT")
            .expect("Expected YELLOWSTONE_ENDPOINT to be set in env"),
        yellowstone_x_token: std::env::var("YELLOWSTONE_X_TOKEN").ok(),
        marginfi_program_id: Pubkey::from_str(
            &std::env::var("MARGINFI_PROGRAM_ID")
                .expect("Expected MARGINFI_PROGRAM_ID to be set in env"),
        )
        .unwrap(),
        marginfi_group_address: Pubkey::from_str(
            &std::env::var("MARGINFI_GROUP_ADDRESS")
                .expect("Expected MARGINFI_GROUP_ADDRESS to be set in env"),
        )
        .unwrap(),
        signer_pubkey: Pubkey::from_str(
            &std::env::var("SIGNER_PUBKEY").expect("Expected SIGNER_PUBKEY to be set in env"),
        )
        .unwrap(),
    };

    debug!("config: {:#?}", config);

    // Assemble stateful engine service
    debug!("starting eva");
    let data_handle = task::spawn(async { StateEngineService::start_and_run(config).await });

    data_handle.await?.unwrap();

    Ok(())
}
