use env_logger::Builder;
use log::{debug, LevelFilter};
use solana_sdk::pubkey::Pubkey;
use state_engine::engine::StateEngineService;
use std::{error::Error, str::FromStr};
use structopt::StructOpt;
use tokio::task;

use crate::state_engine::engine::StateEngineConfig;

mod state_engine;
mod utils;

#[derive(structopt::StructOpt)]
pub struct Eva01 {
    #[structopt(short, long)]
    config_path: String,
    #[structopt(subcommand)]
    command: Eva01Command,
}

#[derive(structopt::StructOpt)]
pub enum Eva01Command {
    Run,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct Eva01Config {
    state_engine_config: StateEngineConfig,
}

impl Eva01Config {
    pub fn try_load_from_file(path: &str) -> Result<Self, Box<dyn Error>> {
        let config_str = std::fs::read_to_string(path)?;
        let config = toml::from_str(&config_str)?;
        Ok(config)
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    // Assemble logger
    Builder::from_default_env().init();

    let eva01_opts = Eva01::from_args();
    let config = Eva01Config::try_load_from_file(&eva01_opts.config_path)?;

    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    debug!("config: {:#?}", config);

    // Assemble stateful engine service
    debug!("starting eva");

    tokio_rt.block_on(async {
        StateEngineService::start_and_run(config.state_engine_config.clone())
            .await
            .unwrap();
    });

    Ok(())
}
