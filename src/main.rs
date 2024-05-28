use crate::{
    geyser::{GeyserService, GeyserServiceConfig, GeyserUpdate},
    liquidator::LiquidatorCfg,
    rebalancer::RebalancerCfg,
    utils::{from_option_vec_pubkey_string, from_pubkey_string},
    wrappers::marginfi_account::TxConfig,
};
use env_logger::Builder;
use log::info;
use solana_sdk::{pubkey, pubkey::Pubkey};
use std::{error::Error, path::PathBuf};

/// Geyser service
mod geyser;

/// IX's for marginfi
mod marginfi_ixs;

/// Responsible for sending transactions for the blockchain
mod sender;

/// Manages token accounts under liquidator account
mod token_account_manager;

/// Liquidator is responsible to liquidate MarginfiAccounts
mod liquidator;

/// Rebalancer is responsible to rebalance the liquidator account
mod rebalancer;

/// Wrappers around marginfi structs
mod wrappers;

/// Utilities used by Eva01
mod utils;

/// CLI configuration for the Eva01
mod cli;

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
    pub fn try_load_from_file(path: PathBuf) -> Result<Self, Box<dyn Error>> {
        let config_str = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read config file: {:?}", e))?;
        let config = toml::from_str(&config_str)
            .map_err(|e| format!("Failed to parse config file {:?}", e))?;
        Ok(config)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Assemble logger
    Builder::from_default_env().init();

    // Main entrypoint
    crate::cli::main_entry().await;

    Ok(())
}
