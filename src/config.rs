use crate::{
    geyser::GeyserServiceConfig,
    utils::{
        fixed_from_float, fixed_to_float, from_option_vec_pubkey_string, from_pubkey_string,
        from_vec_str_to_pubkey, pubkey_to_str, vec_pubkey_to_option_vec_str, vec_pubkey_to_str,
    },
};
use fixed::types::I80F48;
use fixed_macro::types::I80F48;
use solana_sdk::{pubkey, pubkey::Pubkey};
use std::{
    io::{BufWriter, Write},
    path::PathBuf,
};
use toml::ser::to_string_pretty;

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct Eva01Config {
    pub general_config: GeneralConfig,
    pub liquidator_config: LiquidatorCfg,
    pub rebalancer_config: RebalancerCfg,
}

impl Eva01Config {
    pub fn try_load_from_file(path: PathBuf) -> anyhow::Result<Self> {
        let config_str = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("Failed to read config file ({:?}): {:#?}", &path, e))?;
        let config: Eva01Config = toml::from_str(&config_str)
            .map_err(|e| anyhow::anyhow!("Failed to parse config file ({:?}): {:#?}", &path, e))?;
        config.general_config.validate()?;
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
    #[serde(
        deserialize_with = "from_pubkey_string",
        serialize_with = "pubkey_to_str"
    )]
    pub signer_pubkey: Pubkey,
    pub keypair_path: PathBuf,
    #[serde(default = "GeneralConfig::default_compute_unit_price_micro_lamports")]
    pub compute_unit_price_micro_lamports: Option<u64>,
    #[serde(default = "GeneralConfig::default_compute_unit_limit")]
    pub compute_unit_limit: u32,
    #[serde(
        deserialize_with = "from_pubkey_string",
        serialize_with = "pubkey_to_str",
        default = "GeneralConfig::default_marginfi_program_id"
    )]
    pub marginfi_program_id: Pubkey,
    #[serde(
        deserialize_with = "from_option_vec_pubkey_string",
        serialize_with = "vec_pubkey_to_option_vec_str",
        default
    )]
    pub marginfi_groups_whitelist: Option<Vec<Pubkey>>,
    #[serde(
        deserialize_with = "from_option_vec_pubkey_string",
        serialize_with = "vec_pubkey_to_option_vec_str",
        default
    )]
    pub marginfi_groups_blacklist: Option<Vec<Pubkey>>,
    #[serde(
        deserialize_with = "from_option_vec_pubkey_string",
        serialize_with = "vec_pubkey_to_option_vec_str",
        default = "GeneralConfig::default_account_whitelist"
    )]
    pub account_whitelist: Option<Vec<Pubkey>>,
    #[serde(
        default = "GeneralConfig::default_address_lookup_tables",
        deserialize_with = "from_vec_str_to_pubkey",
        serialize_with = "vec_pubkey_to_str"
    )]
    pub address_lookup_tables: Vec<Pubkey>,
    #[serde(default = "GeneralConfig::default_sol_clock_refresh_interval")]
    pub solana_clock_refresh_interval: u64,
    /// Minimun profit on a liquidation to be considered, denominated in USD
    ///
    /// Example:
    /// 0.01 is $0.01
    ///
    /// Default: 0.1
    #[serde(default = "GeneralConfig::default_min_profit")]
    pub min_profit: f64,
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
                 - Keypair Path: {:?}\n\
                 - Compute Unit Price Micro Lamports: {}\n\
                 - Compute Unit Limit: {}\n\
                 - Minimun profit: {}$\n\
                 - Marginfi Program ID: {}\n\
                 - Marginfi Groups Whitelist: {}\n\
                 - Marginfi Groups Blacklist: {}\n\
                 - Account Whitelist: {}",
            self.rpc_url,
            self.yellowstone_endpoint,
            self.yellowstone_x_token.as_deref().unwrap_or("None"),
            self.signer_pubkey,
            self.keypair_path,
            self.compute_unit_price_micro_lamports.unwrap_or_default(),
            self.compute_unit_limit,
            self.min_profit,
            self.marginfi_program_id,
            self.marginfi_groups_whitelist
                .as_ref()
                .map(|v| v
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<String>>()
                    .join(", "))
                .unwrap_or("None".to_string()),
            self.marginfi_groups_blacklist
                .as_ref()
                .map(|v| v
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<String>>()
                    .join(", "))
                .unwrap_or("None".to_string()),
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
    pub fn validate(&self) -> anyhow::Result<()> {
        let whitelist_set = self.marginfi_groups_whitelist.is_some()
            && !self.marginfi_groups_whitelist.as_ref().unwrap().is_empty();
        let blacklist_set = self.marginfi_groups_blacklist.is_some();

        match (whitelist_set, blacklist_set) {
            (true, false) | (false, true) => Ok(()),
            (true, true) => Err(anyhow::anyhow!("Only one of marginfi_groups_whitelist or marginfi_groups_blacklist must be set.")),
            (false, false) => Err(anyhow::anyhow!("One of marginfi_groups_whitelist or marginfi_groups_blacklist must be set. marginfi_groups_whitelist must not be empty.")),
        }
    }

    pub fn get_geyser_service_config(&self) -> GeyserServiceConfig {
        GeyserServiceConfig {
            endpoint: self.yellowstone_endpoint.clone(),
            x_token: self.yellowstone_x_token.clone(),
        }
    }

    pub fn default_marginfi_program_id() -> Pubkey {
        pubkey!("stag8sTKds2h4KzjUw3zKTsxbqvT4XKHdaR9X9E6Rct")
        //marginfi::id()
    }

    pub fn default_account_whitelist() -> Option<Vec<Pubkey>> {
        None
    }

    pub fn default_compute_unit_price_micro_lamports() -> Option<u64> {
        Some(10_000)
    }

    pub fn default_compute_unit_limit() -> u32 {
        200_000
    }

    pub fn default_address_lookup_tables() -> Vec<Pubkey> {
        vec![
            pubkey!("HGmknUTUmeovMc9ryERNWG6UFZDFDVr9xrum3ZhyL4fC"),
            pubkey!("5FuKF7C1tJji2mXZuJ14U9oDb37is5mmvYLf4KwojoF1"),
        ]
    }

    pub fn default_sol_clock_refresh_interval() -> u64 {
        10 // 10 seconds
    }

    pub fn default_min_profit() -> f64 {
        0.1
    }
}

#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
pub struct LiquidatorCfg {
    /// Maximun liquidation value in USD
    pub max_liquidation_value: Option<f64>,
    #[serde(default = "LiquidatorCfg::default_isolated_banks")]
    pub isolated_banks: bool,
}

impl LiquidatorCfg {
    pub fn default_isolated_banks() -> bool {
        false
    }
}

impl std::fmt::Display for LiquidatorCfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Liquidator Config: \n\
                - Max Liquidation Value: {}$\n",
            self.max_liquidation_value.unwrap_or_default()
        )
    }
}

#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
pub struct RebalancerCfg {
    #[serde(
        default = "RebalancerCfg::default_token_account_dust_threshold",
        deserialize_with = "fixed_from_float",
        serialize_with = "fixed_to_float"
    )]
    pub token_account_dust_threshold: I80F48,
    #[serde(
        default = "RebalancerCfg::default_swap_mint",
        deserialize_with = "from_pubkey_string",
        serialize_with = "pubkey_to_str"
    )]
    pub swap_mint: Pubkey,
    #[serde(default = "RebalancerCfg::default_jup_swap_api_url")]
    pub jup_swap_api_url: String,
    #[serde(default = "RebalancerCfg::default_slippage_bps")]
    pub slippage_bps: u16,
    #[serde(default = "RebalancerCfg::default_compute_unit_price_micro_lamports")]
    pub compute_unit_price_micro_lamports: Option<u64>,
}

impl RebalancerCfg {
    pub fn default_token_account_dust_threshold() -> I80F48 {
        I80F48!(0.01)
    }

    pub fn default_compute_unit_price_micro_lamports() -> Option<u64> {
        Some(10_000)
    }

    pub fn default_swap_mint() -> Pubkey {
        pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v") // USDC
    }

    pub fn default_jup_swap_api_url() -> String {
        "https://quote-api.jup.ag/v6".to_string()
    }

    pub fn default_slippage_bps() -> u16 {
        250
    }
}

impl std::fmt::Display for RebalancerCfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Rebalancer Config: \n\
                - Token account dust threshold: {}\n\
                - Swap mint: {}\n\
                - Jup Swap Api URL: {}\n\
                - Slippabe bps: {}\n\
                - Compute unit price micro lamports: {}\n",
            self.token_account_dust_threshold,
            self.jup_swap_api_url,
            self.slippage_bps,
            self.swap_mint,
            self.compute_unit_price_micro_lamports
                .as_ref()
                .map(|u| u.to_string())
                .unwrap_or("None".to_string()),
        )
    }
}
