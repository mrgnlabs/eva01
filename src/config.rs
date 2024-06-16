use crate::{
    geyser::GeyserServiceConfig,
    utils::{
        fixed_from_float, fixed_to_float, from_option_vec_pubkey_string, from_pubkey_string,
        from_vec_str_to_pubkey, pubkey_to_str, vec_pubkey_to_option_vec_str, vec_pubkey_to_str,
    },
    wrappers::marginfi_account::TxConfig,
};
use fixed::types::I80F48;
use fixed_macro::types::I80F48;
use solana_sdk::{pubkey, pubkey::Pubkey};
use std::{
    error::Error,
    io::{BufWriter, Write},
    path::PathBuf,
};
use toml::ser::to_string_pretty;

#[derive(Debug, serde::Deserialize, serde::Serialize)]
/// Eva01 configuration strecture
pub struct Eva01Config {
    pub general_config: GeneralConfig,
    pub liquidator_config: LiquidatorCfg,
    pub rebalancer_config: RebalancerCfg,
}

impl Eva01Config {
    pub fn try_load_from_file(path: PathBuf) -> Result<Self, Box<dyn Error>> {
        let config_str = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read config file: {:?}", e))?;
        let config = toml::from_str(&config_str)
            .map_err(|e| format!("Failed to parse config file {:?}", e))?;
        Ok(config)
    }

    pub fn try_save_from_config(&self, path: &PathBuf) -> anyhow::Result<()> {
        let toml_str = to_string_pretty(self)?;

        let mut file = BufWriter::new(std::fs::File::create(path)?);
        writeln!(file, "{}", toml_str)?;
        Ok(())
    }
}

// General Config
#[derive(Debug, serde::Deserialize, serde::Serialize, Clone)]
/// General config that can be shared by liquidator, rebalancer and geyser
pub struct GeneralConfig {
    pub rpc_url: String,
    pub yellowstone_endpoint: String,
    pub yellowstone_x_token: Option<String>,
    #[serde(default = "GeneralConfig::default_block_engine_url")]
    pub block_engine_url: String,
    #[serde(
        deserialize_with = "from_pubkey_string",
        serialize_with = "pubkey_to_str"
    )]
    pub signer_pubkey: Pubkey,
    pub keypair_path: String,
    #[serde(
        deserialize_with = "from_pubkey_string",
        serialize_with = "pubkey_to_str"
    )]
    pub liquidator_account: Pubkey,
    #[serde(default = "GeneralConfig::default_compute_unit_price_micro_lamports")]
    pub compute_unit_price_micro_lamports: Option<u64>,
    #[serde(
        deserialize_with = "from_pubkey_string",
        serialize_with = "pubkey_to_str",
        default = "GeneralConfig::default_marginfi_program_id"
    )]
    pub marginfi_program_id: Pubkey,
    #[serde(
        deserialize_with = "from_pubkey_string",
        serialize_with = "pubkey_to_str",
        default = "GeneralConfig::default_marginfi_group_address"
    )]
    pub marginfi_group_address: Pubkey,
    #[serde(
        deserialize_with = "from_option_vec_pubkey_string",
        serialize_with = "vec_pubkey_to_option_vec_str",
        default = "GeneralConfig::default_account_whitelist"
    )]
    pub account_whitelist: Option<Vec<Pubkey>>,
    #[serde(default = "GeneralConfig::default_address_lookup_tables")]
    pub address_lookup_tables: Vec<Pubkey>,
}

impl std::fmt::Display for GeneralConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "GeneralConfig:\n\
                 - RPC URL: {}\n\
                 - Yellowstone Endpoint: {}\n\
                 - Yellowstone X Token: {}\n\
                 - Signer Pubkey: {}\n\
                 - Keypair Path: {}\n\
                 - Liquidator Account: {}\n\
                 - Compute Unit Price Micro Lamports: {}\n\
                 - Marginfi Program ID: {}\n\
                 - Marginfi Group Address: {}\n\
                 - Account Whitelist: {}",
            self.rpc_url,
            self.yellowstone_endpoint,
            self.yellowstone_x_token.as_deref().unwrap_or("None"),
            self.signer_pubkey,
            self.keypair_path,
            self.liquidator_account,
            self.compute_unit_price_micro_lamports.unwrap_or_default(),
            self.marginfi_program_id,
            self.marginfi_group_address,
            self.account_whitelist
                .as_ref()
                .map(|v| v
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<String>>()
                    .join(", "))
                .unwrap_or("None".to_string())
        )
    }
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

    pub fn default_block_engine_url() -> String {
        String::from("https://ny.mainnet.block-engine.jito.wtf")
    }

    pub fn default_address_lookup_tables() -> Vec<Pubkey> {
        vec![
            pubkey!("HGmknUTUmeovMc9ryERNWG6UFZDFDVr9xrum3ZhyL4fC"),
            pubkey!("5FuKF7C1tJji2mXZuJ14U9oDb37is5mmvYLf4KwojoF1"),
        ]
    }

    pub fn get_tx_config(&self) -> TxConfig {
        TxConfig {
            compute_unit_price_micro_lamports: self.compute_unit_price_micro_lamports,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LiquidatorCfg {
    /// Minimun profit on a liquidation to be considered, denominated in USD
    ///
    /// Example:
    /// 0.01 is $0.01
    ///
    /// Default: 0.1
    #[serde(default = "LiquidatorCfg::default_min_profit")]
    pub min_profit: f64,
    /// Maximun liquidation value in USD
    pub max_liquidation_value: Option<f64>,
}

impl LiquidatorCfg {
    pub fn default_min_profit() -> f64 {
        0.1
    }
}

impl std::fmt::Display for LiquidatorCfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Liquidator Config: \n\
                - Minimun profit: {}$\n\
                - Max Liquidation Value: {}$\n",
            self.min_profit,
            self.max_liquidation_value.unwrap_or_default()
        )
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct RebalancerCfg {
    #[serde(
        default = "RebalancerCfg::default_token_account_dust_threshold",
        deserialize_with = "fixed_from_float",
        serialize_with = "fixed_to_float"
    )]
    pub token_account_dust_threshold: I80F48,
    #[serde(
        default = "RebalancerCfg::default_preferred_mints",
        deserialize_with = "from_vec_str_to_pubkey",
        serialize_with = "vec_pubkey_to_str"
    )]
    pub preferred_mints: Vec<Pubkey>,
    #[serde(
        default = "RebalancerCfg::default_swap_mint",
        deserialize_with = "from_pubkey_string",
        serialize_with = "pubkey_to_str"
    )]
    pub swap_mint: Pubkey,
    #[serde(default = "RebalancerCfg::default_jup_swap_api_url")]
    pub jup_swap_api_url: String,
    #[serde(default = "RebalancerCfg::default_compute_unit_price_micro_lamports")]
    pub compute_unit_price_micro_lamports: Option<u64>,
    #[serde(default = "RebalancerCfg::default_slippage_bps")]
    pub slippage_bps: u16,
}

impl RebalancerCfg {
    pub fn default_token_account_dust_threshold() -> I80F48 {
        I80F48!(0.01)
    }

    pub fn default_swap_mint() -> Pubkey {
        pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v")
    }

    pub fn default_preferred_mints() -> Vec<Pubkey> {
        vec![pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v")]
    }

    pub fn default_jup_swap_api_url() -> String {
        "https://quote-api.jup.ag/v6".to_string()
    }

    pub fn default_slippage_bps() -> u16 {
        250
    }

    pub fn default_compute_unit_price_micro_lamports() -> Option<u64> {
        Some(10_000)
    }
}

impl std::fmt::Display for RebalancerCfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Rebalancer Config: \n\
                - Token account dust threshold: {}\n\
                - Swap mint: {}\n\
                - Preferred mints: {}\n\
                - Jup Swap Api URL: {}\n\
                - Slippabe bps: {}\n\
                - Compute unit price micro lamports: {}\n",
            self.token_account_dust_threshold,
            self.swap_mint,
            self.preferred_mints
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<String>>()
                .join(", "),
            self.jup_swap_api_url,
            self.slippage_bps,
            self.compute_unit_price_micro_lamports
                .as_ref()
                .map(|u| u.to_string())
                .unwrap_or("None".to_string())
        )
    }
}
