use crate::{processor::EvaLiquidator, state_engine::engine::StateEngineConfig};
use env_logger::Builder;
use log::debug;
use solana_sdk::pubkey::Pubkey;
use state_engine::engine::StateEngineService;
use std::{backtrace, error::Error};
use structopt::StructOpt;

mod processor;
mod state_engine;
mod utils;

#[derive(structopt::StructOpt)]
#[structopt(name = "eva01", about = "Eva01")]
pub struct Eva01 {
    #[structopt(subcommand)]
    command: Eva01Command,
    #[structopt(flatten)]
    opts: Eva01Opts,
}

#[derive(structopt::StructOpt)]
pub enum Eva01Command {
    Run,
    RunFilter { accounts: Vec<Pubkey> },
}

#[derive(structopt::StructOpt)]
pub struct Eva01Opts {
    #[structopt(short, long)]
    config_path: String,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct Eva01Config {
    state_engine_config: StateEngineConfig,
    liquidator_config: processor::EvaLiquidatorCfg,
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

    set_panic_hook();

    let eva01_opts = Eva01::from_args();
    let config = Eva01Config::try_load_from_file(&eva01_opts.opts.config_path)?;

    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    debug!("config: {:#?}", config);

    // Assemble stateful engine service
    debug!("starting eva");

    let (state_engine, update_rx) = StateEngineService::new(config.state_engine_config.clone())?;

    let handle = EvaLiquidator::start(state_engine.clone(), update_rx)?;

    tokio_rt.block_on(async move {
        state_engine.start().await.unwrap();
    });

    handle.join().unwrap()?;

    Ok(())
}

/// Set panic hook to stop if any sub thread panics
fn set_panic_hook() {
    std::panic::set_hook(Box::new(|panic_info| {
        // Print the panic information
        eprintln!("Panic occurred: {:?}", panic_info);

        // Perform any necessary cleanup or logging here

        // Terminate the program
        std::process::exit(1);
    }));
}
