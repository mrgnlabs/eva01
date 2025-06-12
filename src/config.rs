use crate::geyser::GeyserServiceConfig;
use fixed::types::I80F48;
use solana_sdk::pubkey::Pubkey;
use std::{path::PathBuf, str::FromStr};

#[derive(Clone, Debug)]
pub struct Eva01Config {
    pub general_config: GeneralConfig,
    pub liquidator_config: LiquidatorCfg,
    pub rebalancer_config: RebalancerCfg,
}

impl Eva01Config {
    pub fn new() -> anyhow::Result<Self> {
        //General configuration
        let rpc_url = std::env::var("RPC_URL")
            .map_err(|_| anyhow::anyhow!("RPC_URL environment variable is not set"))?;

        let yellowstone_endpoint = std::env::var("YELLOWSTONE_ENDPOINT")
            .map_err(|_| anyhow::anyhow!("YELLOWSTONE_ENDPOINT environment variable is not set"))?;
        let yellowstone_x_token = std::env::var("YELLOWSTONE_X_TOKEN").ok();

        let signer_pubkey = Pubkey::from_str(
            &std::env::var("SIGNER_PUBKEY")
                .map_err(|_| anyhow::anyhow!("SIGNER_PUBKEY environment variable is not set"))?,
        )
        .map_err(|e| anyhow::anyhow!("Invalid SIGNER_PUBKEY: {:#?}", e))?;

        let keypair_path = PathBuf::from(
            std::env::var("KEYPAIR_PATH")
                .map_err(|_| anyhow::anyhow!("KEYPAIR_PATH environment variable is not set"))?,
        );

        let compute_unit_price_micro_lamports: u64 =
            std::env::var("COMPUTE_UNIT_PRICE_MICRO_LAMPORTS")
                .map_err(|_| {
                    anyhow::anyhow!(
                        "COMPUTE_UNIT_PRICE_MICRO_LAMPORTS environment variable is not set"
                    )
                })?
                .parse()
                .map_err(|e| {
                    anyhow::anyhow!("Invalid COMPUTE_UNIT_PRICE_MICRO_LAMPORTS number: {:#?}", e)
                })?;
        let compute_unit_limit: u32 = std::env::var("COMPUTE_UNIT_LIMIT")
            .map_err(|_| anyhow::anyhow!("COMPUTE_UNIT_LIMIT environment variable is not set"))?
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid COMPUTE_UNIT_LIMIT number: {:#?}", e))?;

        let marginfi_program_id =
            Pubkey::from_str(&std::env::var("MARGINFI_PROGRAM_ID").map_err(|_| {
                anyhow::anyhow!("MARGINFI_PROGRAM_ID environment variable is not set")
            })?)
            .map_err(|e| anyhow::anyhow!("Invalid MARGINFI_PROGRAM_ID: {:#?}", e))?;

        let marginfi_groups_whitelist: Vec<Pubkey> =
            parse_pubkey_list("MARGINFI_GROUPS_WHITELIST")?;
        let marginfi_groups_blacklist: Vec<Pubkey> =
            parse_pubkey_list("MARGINFI_GROUPS_BLACKLIST")?;

        let address_lookup_tables: Vec<Pubkey> =
            parse_pubkey_list("ADDRESS_LOOKUP_TABLES").unwrap_or_else(|_| vec![]);

        let solana_clock_refresh_interval: u64 = std::env::var("SOLANA_CLOCK_REFERSH_INTERVAL")
            .map_err(|_| {
                anyhow::anyhow!("SOLANA_CLOCK_REFERSH_INTERVAL environment variable is not set")
            })?
            .parse()
            .map_err(|e| {
                anyhow::anyhow!("Invalid SOLANA_CLOCK_REFERSH_INTERVAL number: {:#?}", e)
            })?;

        let general_config = GeneralConfig {
            rpc_url,
            yellowstone_endpoint,
            yellowstone_x_token,
            signer_pubkey,
            keypair_path,
            compute_unit_price_micro_lamports,
            compute_unit_limit,
            marginfi_program_id,
            marginfi_groups_whitelist,
            marginfi_groups_blacklist,
            address_lookup_tables,
            solana_clock_refresh_interval,
        };

        general_config.validate()?;

        // Liquidator process configuration
        let min_profit: f64 = std::env::var("MIN_PROFIT")
            .map_err(|_| anyhow::anyhow!("MIN_PROFIT environment variable is not set"))?
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid MIN_PROFIT number: {:#?}", e))?;
        let isolated_banks: bool = std::env::var("ISOLATED_BANKS")
            .map_err(|_| anyhow::anyhow!("ISOLATED_BANKS environment variable is not set"))?
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid ISOLATED_BANKS boolean: {:#?}", e))?;

        let liquidator_config = LiquidatorCfg {
            min_profit,
            isolated_banks,
        };

        // Rebalancer process configuration
        let token_account_dust_threshold: f64 = std::env::var("TOKEN_ACCOUNT_DUST_THRESHOLD")
            .map_err(|_| {
                anyhow::anyhow!("TOKEN_ACCOUNT_DUST_THRESHOLD environment variable is not set")
            })?
            .parse()
            .map_err(|e| {
                anyhow::anyhow!("Invalid TOKEN_ACCOUNT_DUST_THRESHOLD number: {:#?}", e)
            })?;

        let preferred_mints: Vec<Pubkey> = parse_pubkey_list("PREFERRED_MINTS")?;

        let swap_mint = Pubkey::from_str(
            &std::env::var("SWAP_MINT")
                .map_err(|_| anyhow::anyhow!("SWAP_MINT environment variable is not set"))?,
        )?;

        let jup_swap_api_url = std::env::var("JUP_SWAP_API_URL")
            .map_err(|_| anyhow::anyhow!("JUP_SWAP_API_URL environment variable is not set"))?;

        let slippage_bps: u16 = std::env::var("SLIPPAGE_BPS")
            .map_err(|_| anyhow::anyhow!("SLIPPAGE_BPS environment variable is not set"))?
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid SLIPPAGE_BPS number: {:#?}", e))?;

        let rebalancer_config = RebalancerCfg {
            token_account_dust_threshold: I80F48::from_num(token_account_dust_threshold),
            preferred_mints,
            swap_mint,
            jup_swap_api_url,
            slippage_bps,
        };

        Ok(Eva01Config {
            general_config,
            liquidator_config,
            rebalancer_config,
        })
    }
}

impl std::fmt::Display for Eva01Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Eva01Config:\n\
             - General Config: {}\n\
             - Liquidator Config: {}\n\
             - Rebalancer Config: {}",
            self.general_config, self.liquidator_config, self.rebalancer_config
        )
    }
}

/// General config that can be shared by liquidator, rebalancer and geyser
#[derive(Debug, Clone)]
pub struct GeneralConfig {
    pub rpc_url: String,
    pub yellowstone_endpoint: String,
    pub yellowstone_x_token: Option<String>,
    // TODO: replace keypair path with the keypair object and the signer pubkey with get_signer_pubkey() method
    pub signer_pubkey: Pubkey,
    pub keypair_path: PathBuf,
    pub compute_unit_price_micro_lamports: u64,
    pub compute_unit_limit: u32,
    pub marginfi_program_id: Pubkey,
    pub marginfi_groups_whitelist: Vec<Pubkey>,
    pub marginfi_groups_blacklist: Vec<Pubkey>,
    pub address_lookup_tables: Vec<Pubkey>,
    pub solana_clock_refresh_interval: u64,
}

impl GeneralConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        match (!self.marginfi_groups_whitelist.is_empty(), !self.marginfi_groups_blacklist.is_empty()) {
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
                 - Marginfi Program ID: {}\n\
                 - Marginfi Groups Whitelist: {:?}\n\
                 - Marginfi Groups Blacklist: {:?}\n",
            self.rpc_url,
            self.yellowstone_endpoint,
            self.yellowstone_x_token.as_deref().unwrap_or("None"),
            self.signer_pubkey,
            self.keypair_path,
            self.compute_unit_price_micro_lamports,
            self.compute_unit_limit,
            self.marginfi_program_id,
            self.marginfi_groups_whitelist,
            self.marginfi_groups_blacklist,
        )
    }
}

#[derive(Debug, Default, Clone)]
pub struct LiquidatorCfg {
    /// Minimun profit on a liquidation to be considered, denominated in USD. Example: 0.01 means $0.01
    pub min_profit: f64,
    pub isolated_banks: bool,
}

impl std::fmt::Display for LiquidatorCfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Liquidator Config: \n\
                - Minimun profit: {}$\n\
                - Isolated banks: {}$\n",
            self.min_profit, self.isolated_banks
        )
    }
}

#[derive(Debug, Default, Clone)]
pub struct RebalancerCfg {
    pub token_account_dust_threshold: I80F48,
    pub preferred_mints: Vec<Pubkey>,
    pub swap_mint: Pubkey,
    pub jup_swap_api_url: String,
    pub slippage_bps: u16,
}

impl std::fmt::Display for RebalancerCfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Rebalancer Config: \n\
                - Token account dust threshold: {}\n\
                - Swap mint: {}\n\
                - Preferred mints: {:?}\n\
                - Jup Swap Api URL: {}\n\
                - Slippage bps: {}\n",
            self.token_account_dust_threshold,
            self.swap_mint,
            self.preferred_mints,
            self.jup_swap_api_url,
            self.slippage_bps,
        )
    }
}

fn parse_pubkey_list(env_var: &str) -> anyhow::Result<Vec<Pubkey>> {
    match std::env::var_os(env_var) {
        Some(val) => {
            let value = val.to_string_lossy();
            if value.trim().is_empty() {
                Ok(vec![])
            } else {
                value
                    .split(',')
                    .map(|s| {
                        Pubkey::from_str(s.trim()).map_err(|e| {
                            anyhow::anyhow!("Invalid pubkey in the {} list: {:#?}", env_var, e)
                        })
                    })
                    .collect()
            }
        }
        None => Ok(vec![]),
    }
}

#[cfg(test)]
mod tests {
    use solana_sdk::signature::Keypair;
    use tempfile::tempdir;

    use super::*;
    use solana_sdk::signer::Signer;

    use std::env;
    use std::fs::File;
    use std::io::Write;

    fn set_env(key: &str, value: &str) {
        env::set_var(key, value);
    }

    fn unset_env(key: &str) {
        env::remove_var(key);
    }

    fn setup_general_env() -> (
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
    ) {
        let keypair = Keypair::new();
        let dir = tempdir().unwrap();
        let keypair_path = dir.path().join("keypair.json");
        let mut file = File::create(&keypair_path).unwrap();
        file.write_all(&keypair.to_bytes()).unwrap();
        file.flush().unwrap();

        let rpc_url = "http://dummy:1234";
        let yellowstone_endpoint = "http://dummy:1234";
        let yellowstone_x_token = "token";
        let signer_pubkey = keypair.pubkey().to_string();
        let keypair_path_str = keypair_path.to_str().unwrap();
        let compute_unit_price_micro_lamports = "1000";
        let compute_unit_limit = "200000";
        let marginfi_program_id = Pubkey::new_unique().to_string();
        let marginfi_groups_whitelist = Pubkey::new_unique().to_string();
        let marginfi_groups_blacklist = "";
        let address_lookup_tables = Pubkey::new_unique().to_string();
        let solana_clock_refresh_interval = "1000";
        let min_profit = "0.01";
        let isolated_banks = "true";

        set_env("RPC_URL", rpc_url);
        set_env("YELLOWSTONE_ENDPOINT", yellowstone_endpoint);
        set_env("YELLOWSTONE_X_TOKEN", yellowstone_x_token);
        set_env("SIGNER_PUBKEY", &signer_pubkey);
        set_env("KEYPAIR_PATH", keypair_path_str);
        set_env(
            "COMPUTE_UNIT_PRICE_MICRO_LAMPORTS",
            compute_unit_price_micro_lamports,
        );
        set_env("COMPUTE_UNIT_LIMIT", compute_unit_limit);
        set_env("MARGINFI_PROGRAM_ID", &marginfi_program_id);
        set_env("MARGINFI_GROUPS_WHITELIST", &marginfi_groups_whitelist);
        set_env("MARGINFI_GROUPS_BLACKLIST", &marginfi_groups_blacklist);
        set_env("ADDRESS_LOOKUP_TABLES", &address_lookup_tables);
        set_env(
            "SOLANA_CLOCK_REFRESH_INTERVAL",
            solana_clock_refresh_interval,
        );
        set_env("MIN_PROFIT", min_profit);
        set_env("ISOLATED_BANKS", isolated_banks);

        (
            keypair_path_str.to_string(),
            rpc_url.to_string(),
            yellowstone_endpoint.to_string(),
            yellowstone_x_token.to_string(),
            signer_pubkey.to_string(),
            compute_unit_price_micro_lamports.to_string(),
            compute_unit_limit.to_string(),
            marginfi_program_id.to_string(),
            marginfi_groups_whitelist.to_string(),
            marginfi_groups_blacklist.to_string(),
            address_lookup_tables.to_string(),
            solana_clock_refresh_interval.to_string(),
            min_profit.to_string(),
            isolated_banks.to_string(),
        )
    }

    fn setup_rebalancer_env() {
        set_env("TOKEN_ACCOUNT_DUST_THRESHOLD", "0.0001");
        set_env("PREFERRED_MINTS", &Pubkey::new_unique().to_string());
        set_env("SWAP_MINT", &Pubkey::new_unique().to_string());
        set_env("JUP_SWAP_API_URL", "https://dummy/swap");
        set_env("SLIPPAGE_BPS", "50");
    }

    #[test]
    fn test_parse_pubkey_list_empty() {
        unset_env("TEST_PUBKEY_LIST");
        let result = parse_pubkey_list("TEST_PUBKEY_LIST").unwrap();
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_parse_pubkey_list_valid() {
        set_env(
            "TEST_PUBKEY_LIST",
            format!(
                "{},{}",
                &Pubkey::new_unique().to_string(),
                &Pubkey::new_unique().to_string()
            )
            .as_str(),
        );
        let result = parse_pubkey_list("TEST_PUBKEY_LIST").unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_parse_pubkey_list_invalid() {
        set_env("TEST_PUBKEY_LIST", "invalid_pubkey");
        let result = parse_pubkey_list("TEST_PUBKEY_LIST");
        assert!(result.is_err());
    }

    #[test]
    fn test_general_config_validate_whitelist_only() {
        let mut config = GeneralConfig {
            rpc_url: "".to_string(),
            yellowstone_endpoint: "".to_string(),
            yellowstone_x_token: None,
            signer_pubkey: Pubkey::default(),
            keypair_path: PathBuf::new(),
            compute_unit_price_micro_lamports: 0,
            compute_unit_limit: 0,
            marginfi_program_id: Pubkey::default(),
            marginfi_groups_whitelist: vec![Pubkey::default()],
            marginfi_groups_blacklist: vec![],
            address_lookup_tables: vec![],
            solana_clock_refresh_interval: 0,
        };
        assert!(config.validate().is_ok());

        config.marginfi_groups_whitelist = vec![];
        config.marginfi_groups_blacklist = vec![Pubkey::default()];
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_general_config_validate_both_set() {
        let config = GeneralConfig {
            rpc_url: "".to_string(),
            yellowstone_endpoint: "".to_string(),
            yellowstone_x_token: None,
            signer_pubkey: Pubkey::default(),
            keypair_path: PathBuf::new(),
            compute_unit_price_micro_lamports: 0,
            compute_unit_limit: 0,
            marginfi_program_id: Pubkey::default(),
            marginfi_groups_whitelist: vec![Pubkey::default()],
            marginfi_groups_blacklist: vec![Pubkey::default()],
            address_lookup_tables: vec![],
            solana_clock_refresh_interval: 0,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_general_config_validate_none_set() {
        let config = GeneralConfig {
            rpc_url: "".to_string(),
            yellowstone_endpoint: "".to_string(),
            yellowstone_x_token: None,
            signer_pubkey: Pubkey::default(),
            keypair_path: PathBuf::new(),
            compute_unit_price_micro_lamports: 0,
            compute_unit_limit: 0,
            marginfi_program_id: Pubkey::default(),
            marginfi_groups_whitelist: vec![],
            marginfi_groups_blacklist: vec![],
            address_lookup_tables: vec![],
            solana_clock_refresh_interval: 0,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_eva01_config_new_success() {
        let _ = setup_general_env();
        setup_rebalancer_env();
        let config = Eva01Config::new();
        assert!(config.is_ok());
    }

    #[test]
    fn test_eva01_config_new_missing_env() {
        unset_env("RPC_URL");
        let result = Eva01Config::new();
        assert!(result.is_err());
    }

    #[test]
    fn test_display_impls() {
        let _ = setup_general_env();
        setup_rebalancer_env();
        let config = Eva01Config::new().unwrap();
        let s = format!("{}", config);
        assert!(s.contains("Eva01Config:"));
        assert!(s.contains("General Config:"));
        assert!(s.contains("Liquidator Config:"));
        assert!(s.contains("Rebalancer Config:"));
    }

    #[test]
    fn test_get_geyser_service_config() {
        let _ = setup_general_env();
        setup_rebalancer_env();
        let config = Eva01Config::new().unwrap();
        let geyser_cfg = config.general_config.get_geyser_service_config();
        assert_eq!(
            geyser_cfg.endpoint,
            config.general_config.yellowstone_endpoint
        );
        assert_eq!(
            geyser_cfg.x_token,
            config.general_config.yellowstone_x_token
        );
    }
}
