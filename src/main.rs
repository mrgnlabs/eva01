use crate::{
    geyser::{GeyserService, GeyserServiceConfig, GeyserUpdate},
    liquidator::{Liquidator, LiquidatorCfg},
    rebalancer::{Rebalancer, RebalancerCfg},
    utils::{from_option_vec_pubkey_string, from_pubkey_string},
    wrappers::marginfi_account::TxConfig,
};
use env_logger::Builder;
use log::info;
use solana_sdk::{pubkey, pubkey::Pubkey};
use std::error::Error;
use structopt::StructOpt;

mod geyser;
mod marginfi_ixs;
mod sender;
mod token_account_manager;

/// Liquidator is responsible to liquidate MarginfiAccounts
mod liquidator;

/// Rebalancer is responsible to rebalance the liquidator account
mod rebalancer;

/// Wrappers around marginfi structs
mod wrappers;

/// Utilities used by Eva01
mod utils;

#[derive(structopt::StructOpt)]
#[structopt(name = "eva01", about = "Eva01 CLI Tool")]
pub struct Eva01 {
    /// The command to run
    #[structopt(subcommand)]
    command: Eva01Command,
    /// Aditional arguments
    #[structopt(flatten)]
    opts: Eva01Opts,
}

#[derive(structopt::StructOpt)]
pub enum Eva01Command {
    /// Run Liquidator with all accounts
    Run,
    /// Run Liquidator with filtered accounts
    RunFilter { accounts: Vec<Pubkey> },
}

#[derive(structopt::StructOpt)]
pub struct Eva01Opts {
    /// Path to the configuration file
    #[structopt(short, long)]
    config_path: String,
}

#[derive(Debug, serde::Deserialize)]
/// Eva01 configuration strecture
struct Eva01Config {
    general_config: GeneralConfig,
    liquidator_config: LiquidatorCfg,
    rebalancer_config: RebalancerCfg,
}

#[derive(Debug, serde::Deserialize, serde::Serialize, Clone)]
/// General config that can be shared by liquidator, rebalancer and geyser
struct GeneralConfig {
    pub rpc_url: String,
    pub yellowstone_endpoint: String,
    pub yellowstone_x_token: Option<String>,
    #[serde(deserialize_with = "from_pubkey_string")]
    pub signer_pubkey: Pubkey,
    pub keypair_path: String,
    #[serde(deserialize_with = "from_pubkey_string")]
    pub liquidator_account: Pubkey,
    #[serde(default = "GeneralConfig::default_compute_unit_price_micro_lamports")]
    pub compute_unit_price_micro_lamports: Option<u64>,
    #[serde(
        deserialize_with = "from_pubkey_string",
        default = "GeneralConfig::default_marginfi_program_id"
    )]
    pub marginfi_program_id: Pubkey,
    #[serde(
        deserialize_with = "from_pubkey_string",
        default = "GeneralConfig::default_marginfi_group_address"
    )]
    pub marginfi_group_address: Pubkey,
    #[serde(
        deserialize_with = "from_option_vec_pubkey_string",
        default = "GeneralConfig::default_account_whitelist"
    )]
    pub account_whitelist: Option<Vec<Pubkey>>,
}

impl GeneralConfig {
    pub fn get_geyser_service_config(&self) -> GeyserServiceConfig {
        GeyserServiceConfig {
            endpoint: self.yellowstone_endpoint.clone(),
            x_token: self.yellowstone_x_token.clone(),
        }
    }

    pub fn default_marginfi_program_id() -> Pubkey {
        marginfi::id()
    }

    pub fn default_marginfi_group_address() -> Pubkey {
        pubkey!("4qp6Fx6tnZkY5Wropq9wUYgtFxXKwE6viZxFHg3rdAG8")
    }

    pub fn default_account_whitelist() -> Option<Vec<Pubkey>> {
        None
    }

    pub fn default_compute_unit_price_micro_lamports() -> Option<u64> {
        Some(10_000)
    }

    pub fn get_tx_config(&self) -> TxConfig {
        TxConfig {
            compute_unit_price_micro_lamports: self.compute_unit_price_micro_lamports,
        }
    }
}

impl Eva01Config {
    pub fn try_load_from_file(path: &str) -> Result<Self, Box<dyn Error>> {
        let config_str = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read config file: {:?}", e))?;
        let config = toml::from_str(&config_str)
            .map_err(|e| format!("Failed to parse config file {:?}", e))?;
        Ok(config)
    }
}

// TODO: Remove this tokio::main and respective feature on Cargo.toml
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Assemble logger
    Builder::from_default_env().init();

    let eva01_opts = Eva01::from_args();
    let config = Eva01Config::try_load_from_file(&eva01_opts.opts.config_path)?;
    info!("Starting eva");

    let (liquidator_tx, liquidator_rx) = crossbeam::channel::unbounded::<GeyserUpdate>();
    let (rebalancer_tx, rebalancer_rx) = crossbeam::channel::unbounded::<GeyserUpdate>();

    let mut liquidator = Liquidator::new(
        config.general_config.clone(),
        config.liquidator_config.clone(),
        liquidator_rx.clone(),
    );

    let mut rebalancer = Rebalancer::new(
        config.general_config.clone(),
        config.rebalancer_config.clone(),
        rebalancer_rx.clone(),
    )?;

    let _ = liquidator.load_data().await;

    let _ = rebalancer.load_data(liquidator.get_banks_and_map());

    let geyser_handle = GeyserService::connect(
        config.general_config.get_geyser_service_config(),
        liquidator.get_accounts_to_track(),
        config.general_config.marginfi_program_id.clone(),
        liquidator_tx,
        rebalancer_tx,
    )
    .await?;

    tokio::task::spawn(async {
        geyser_handle.await;
    });

    tokio::task::spawn(async move {
        rebalancer.start().await;
    });

    liquidator.start().await;

    Ok(())
}
