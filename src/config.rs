use crate::geyser::GeyserServiceConfig;
use fixed::types::I80F48;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

#[derive(Clone, Debug)]
pub struct Eva01Config {
    pub general_config: GeneralConfig,
    pub liquidator_config: LiquidatorCfg,
    pub rebalancer_config: RebalancerCfg,
}

impl Eva01Config {
    pub fn new() -> anyhow::Result<Self> {
        //General configuration
        let rpc_url = std::env::var("RPC_URL").expect("RPC_URL environment variable is not set");

        let yellowstone_endpoint = std::env::var("YELLOWSTONE_ENDPOINT")
            .expect("YELLOWSTONE_ENDPOINT environment variable is not set");
        let yellowstone_x_token = std::env::var("YELLOWSTONE_X_TOKEN").ok();

        let wallet_keypair_env = std::env::var("WALLET_KEYPAIR")
            .expect("WALLET_KEYPAIR environment variable is not set");
        let wallet_keypair: Vec<u8> =
            serde_json::from_str(&wallet_keypair_env).expect("Invalid WALLET_KEYPAIR JSON format");

        let compute_unit_price_micro_lamports: u64 =
            std::env::var("COMPUTE_UNIT_PRICE_MICRO_LAMPORTS")
                .expect("COMPUTE_UNIT_PRICE_MICRO_LAMPORTS environment variable is not set")
                .parse()
                .expect("Invalid COMPUTE_UNIT_PRICE_MICRO_LAMPORTS number");
        let compute_unit_limit: u32 = std::env::var("COMPUTE_UNIT_LIMIT")
            .expect("COMPUTE_UNIT_LIMIT environment variable is not set")
            .parse()
            .expect("Invalid COMPUTE_UNIT_LIMIT number");

        let marginfi_program_id = Pubkey::from_str(
            &std::env::var("MARGINFI_PROGRAM_ID")
                .expect("MARGINFI_PROGRAM_ID environment variable is not set"),
        )
        .expect("Invalid MARGINFI_PROGRAM_ID Pubkey");

        let marginfi_api_url = std::env::var("MARGINFI_API_URL").ok();
        let marginfi_api_key = std::env::var("MARGINFI_API_KEY").ok();
        let marginfi_api_arena_threshold: Option<u64> =
            std::env::var("MARGINFI_API_ARENA_THRESHOLD")
                .ok()
                .and_then(|s| s.parse().ok());

        let marginfi_groups_whitelist: Vec<Pubkey> =
            parse_pubkey_list("MARGINFI_GROUPS_WHITELIST")?;
        let marginfi_groups_blacklist: Vec<Pubkey> =
            parse_pubkey_list("MARGINFI_GROUPS_BLACKLIST")?;

        let address_lookup_tables: Vec<Pubkey> =
            parse_pubkey_list("ADDRESS_LOOKUP_TABLES").unwrap_or_else(|_| vec![]);

        let solana_clock_refresh_interval: u64 = std::env::var("SOLANA_CLOCK_REFRESH_INTERVAL")
            .expect("SOLANA_CLOCK_REFRESH_INTERVAL environment variable is not set")
            .parse()
            .expect("Invalid SOLANA_CLOCK_REFRESH_INTERVAL number");

        let min_profit: f64 = std::env::var("MIN_PROFIT")
            .expect("MIN_PROFIT environment variable is not set")
            .parse()
            .expect("Invalid MIN_PROFIT number");

        let healthcheck_port: u16 = std::env::var("HEALTHCHECK_PORT")
            .expect("HEALTHCHECK_PORT environment variable is not set")
            .parse()
            .expect("Invalid HEALTHCHECK_PORT number");

        let general_config = GeneralConfig {
            rpc_url,
            yellowstone_endpoint,
            yellowstone_x_token,
            wallet_keypair,
            compute_unit_price_micro_lamports,
            compute_unit_limit,
            marginfi_program_id,
            marginfi_api_url,
            marginfi_api_key,
            marginfi_api_arena_threshold,
            marginfi_groups_whitelist,
            marginfi_groups_blacklist,
            address_lookup_tables,
            solana_clock_refresh_interval,
            min_profit,
            healthcheck_port,
        };

        general_config.validate()?;

        // Liquidator process configuration
        let isolated_banks: bool = std::env::var("ISOLATED_BANKS")
            .expect("ISOLATED_BANKS environment variable is not set")
            .parse()
            .expect("Invalid ISOLATED_BANKS boolean");

        let liquidator_config = LiquidatorCfg { isolated_banks };

        // Rebalancer process configuration
        let token_account_dust_threshold: f64 = std::env::var("TOKEN_ACCOUNT_DUST_THRESHOLD")
            .expect("TOKEN_ACCOUNT_DUST_THRESHOLD environment variable is not set")
            .parse()
            .expect("Invalid TOKEN_ACCOUNT_DUST_THRESHOLD number");

        let swap_mint = Pubkey::from_str(
            &std::env::var("SWAP_MINT").expect("SWAP_MINT environment variable is not set"),
        )
        .expect("Invalid SWAP_MINT Pubkey");

        let jup_swap_api_url = std::env::var("JUP_SWAP_API_URL")
            .expect("JUP_SWAP_API_URL environment variable is not set");

        let slippage_bps: u16 = std::env::var("SLIPPAGE_BPS")
            .expect("SLIPPAGE_BPS environment variable is not set")
            .parse()
            .expect("Invalid SLIPPAGE_BPS number: {:#?}");

        let rebalancer_config = RebalancerCfg {
            token_account_dust_threshold: I80F48::from_num(token_account_dust_threshold),
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
    pub wallet_keypair: Vec<u8>,
    pub compute_unit_price_micro_lamports: u64,
    pub compute_unit_limit: u32,
    pub marginfi_program_id: Pubkey,
    pub marginfi_api_url: Option<String>,
    pub marginfi_api_key: Option<String>,
    pub marginfi_api_arena_threshold: Option<u64>,
    pub marginfi_groups_whitelist: Vec<Pubkey>,
    pub marginfi_groups_blacklist: Vec<Pubkey>,
    pub address_lookup_tables: Vec<Pubkey>,
    pub solana_clock_refresh_interval: u64,
    pub min_profit: f64,
    pub healthcheck_port: u16,
}

impl GeneralConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        match (
            !self.marginfi_groups_whitelist.is_empty(),
            !self.marginfi_groups_blacklist.is_empty(),
        ) {
            (true, false) | (false, true) => Ok(()),
            (true, true) => Err(anyhow::anyhow!(
                "Only one of marginfi_groups_whitelist or marginfi_groups_blacklist must be set."
            )),
            (false, false) => Err(anyhow::anyhow!(
                "Either marginfi_groups_whitelist or marginfi_groups_blacklist must be set - not both."
            )),
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
                 - Yellowstone Endpoint: {}\n\
                 - Compute Unit Price Micro Lamports: {}\n\
                 - Compute Unit Limit: {}\n\
                 - Minimun profit: {}$\n\
                 - Marginfi Program ID: {}\n\
                 - Marginfi API URL: {}\n\
                 - Marginfi API Key: {}\n\
                 - Marginfi API Arena Threshold: {}\n\
                 - Marginfi Groups Whitelist: {:?}\n\
                 - Marginfi Groups Blacklist: {:?}\n
                 - Address Lookup Tables: {:?}\n\
                 - Solana Clock Refresh Interval: {}\n\
                    - Healthcheck Port: {}",
            self.yellowstone_endpoint,
            self.compute_unit_price_micro_lamports,
            self.compute_unit_limit,
            self.min_profit,
            self.marginfi_program_id,
            self.marginfi_api_url.as_deref().unwrap_or("None"),
            self.marginfi_api_key.as_deref().unwrap_or("None"),
            self.marginfi_api_arena_threshold.unwrap_or_default(),
            self.marginfi_groups_whitelist,
            self.marginfi_groups_blacklist,
            self.address_lookup_tables,
            self.solana_clock_refresh_interval,
            self.healthcheck_port,
        )
    }
}

#[derive(Debug, Default, Clone)]
pub struct LiquidatorCfg {
    /// Minimun profit on a liquidation to be considered, denominated in USD. Example: 0.01 means $0.01
    pub isolated_banks: bool,
}

impl std::fmt::Display for LiquidatorCfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Liquidator Config: \n\
                - Isolated banks: {}$\n",
            self.isolated_banks
        )
    }
}

#[derive(Debug, Default, Clone)]
pub struct RebalancerCfg {
    pub token_account_dust_threshold: I80F48,
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
                - Jup Swap Api URL: {}\n\
                - Slippage bps: {}\n",
            self.token_account_dust_threshold,
            self.swap_mint,
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
    use serial_test::serial;
    use solana_sdk::signature::Keypair;

    use super::*;

    use std::env;

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
        let keypair = serde_json::to_string(&Keypair::new().to_bytes().to_vec()).unwrap();

        let rpc_url = "http://dummy:1234";
        let yellowstone_endpoint = "http://dummy:1234";
        let yellowstone_x_token = "token";
        let keypair = keypair;
        let compute_unit_price_micro_lamports = "1000";
        let compute_unit_limit = "200000";
        let marginfi_program_id = Pubkey::new_unique().to_string();
        let marginfi_groups_whitelist = Pubkey::new_unique().to_string();
        let marginfi_groups_blacklist = "";
        let address_lookup_tables = Pubkey::new_unique().to_string();
        let solana_clock_refresh_interval = "1000";
        let min_profit = "0.01";
        let isolated_banks = "true";
        let healthcheck_port = "1234";

        set_env("RPC_URL", rpc_url);
        set_env("YELLOWSTONE_ENDPOINT", yellowstone_endpoint);
        set_env("YELLOWSTONE_X_TOKEN", yellowstone_x_token);
        set_env("WALLET_KEYPAIR", &keypair);
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
        set_env("HEALTHCHECK_PORT", healthcheck_port);

        (
            keypair,
            rpc_url.to_string(),
            yellowstone_endpoint.to_string(),
            yellowstone_x_token.to_string(),
            compute_unit_price_micro_lamports.to_string(),
            compute_unit_limit.to_string(),
            marginfi_program_id.to_string(),
            marginfi_groups_whitelist.to_string(),
            marginfi_groups_blacklist.to_string(),
            address_lookup_tables.to_string(),
            solana_clock_refresh_interval.to_string(),
            min_profit.to_string(),
            isolated_banks.to_string(),
            healthcheck_port.to_string(),
        )
    }

    fn setup_rebalancer_env() {
        set_env("TOKEN_ACCOUNT_DUST_THRESHOLD", "0.0001");
        set_env("SWAP_MINT", &Pubkey::new_unique().to_string());
        set_env("JUP_SWAP_API_URL", "https://dummy/swap");
        set_env("SLIPPAGE_BPS", "50");
    }

    #[test]
    #[serial]
    fn test_parse_pubkey_list_empty() {
        unset_env("TEST_PUBKEY_LIST");
        let result = parse_pubkey_list("TEST_PUBKEY_LIST").unwrap();
        assert_eq!(result.len(), 0);
    }

    #[test]
    #[serial]
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
    #[serial]
    fn test_parse_pubkey_list_invalid() {
        set_env("TEST_PUBKEY_LIST", "invalid_pubkey");
        let result = parse_pubkey_list("TEST_PUBKEY_LIST");
        assert!(result.is_err());
    }

    #[test]
    #[serial]
    fn test_general_config_validate_whitelist_only() {
        let mut config = GeneralConfig {
            rpc_url: "".to_string(),
            yellowstone_endpoint: "".to_string(),
            yellowstone_x_token: None,
            wallet_keypair: Vec::<u8>::new(),
            compute_unit_price_micro_lamports: 0,
            compute_unit_limit: 0,
            marginfi_program_id: Pubkey::default(),
            marginfi_api_url: None,
            marginfi_api_key: None,
            marginfi_api_arena_threshold: None,
            marginfi_groups_whitelist: vec![Pubkey::default()],
            marginfi_groups_blacklist: vec![],
            address_lookup_tables: vec![],
            solana_clock_refresh_interval: 0,
            min_profit: 0.01,
            healthcheck_port: 0,
        };
        assert!(config.validate().is_ok());

        config.marginfi_groups_whitelist = vec![];
        config.marginfi_groups_blacklist = vec![Pubkey::default()];
        assert!(config.validate().is_ok());
    }

    #[test]
    #[serial]
    fn test_general_config_validate_both_set() {
        let config = GeneralConfig {
            rpc_url: "".to_string(),
            yellowstone_endpoint: "".to_string(),
            yellowstone_x_token: None,
            wallet_keypair: Vec::<u8>::new(),
            compute_unit_price_micro_lamports: 0,
            compute_unit_limit: 0,
            marginfi_program_id: Pubkey::default(),
            marginfi_api_url: None,
            marginfi_api_key: None,
            marginfi_api_arena_threshold: None,
            marginfi_groups_whitelist: vec![Pubkey::default()],
            marginfi_groups_blacklist: vec![Pubkey::default()],
            address_lookup_tables: vec![],
            solana_clock_refresh_interval: 0,
            min_profit: 0.01,
            healthcheck_port: 0,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    #[serial]
    fn test_general_config_validate_none_set() {
        let config = GeneralConfig {
            rpc_url: "".to_string(),
            yellowstone_endpoint: "".to_string(),
            yellowstone_x_token: None,
            wallet_keypair: Vec::<u8>::new(),
            compute_unit_price_micro_lamports: 0,
            compute_unit_limit: 0,
            marginfi_program_id: Pubkey::default(),
            marginfi_api_url: None,
            marginfi_api_key: None,
            marginfi_api_arena_threshold: None,
            marginfi_groups_whitelist: vec![],
            marginfi_groups_blacklist: vec![],
            address_lookup_tables: vec![],
            solana_clock_refresh_interval: 0,
            min_profit: 0.01,
            healthcheck_port: 0,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    #[serial]
    fn test_eva01_config_new_success() {
        let _ = setup_general_env();
        setup_rebalancer_env();
        let config = Eva01Config::new();
        assert!(config.is_ok());
    }

    #[test]
    #[serial]
    #[should_panic(expected = "RPC_URL environment variable is not set")]
    fn test_eva01_config_new_missing_env() {
        unset_env("RPC_URL");
        let result = Eva01Config::new();
        assert!(result.is_err());
    }

    #[test]
    #[serial]
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
    #[serial]
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

    #[test]
    #[serial]
    #[should_panic(expected = "Invalid MARGINFI_PROGRAM_ID Pubkey")]
    fn test_eva01_config_new_invalid_pubkey_env() {
        let _ = setup_general_env();
        setup_rebalancer_env();
        set_env("MARGINFI_PROGRAM_ID", "not_a_pubkey");
        Eva01Config::new().unwrap();
    }

    #[test]
    #[serial]
    #[should_panic(expected = "Invalid COMPUTE_UNIT_PRICE_MICRO_LAMPORTS number")]
    fn test_eva01_config_new_invalid_compute_unit_price() {
        let _ = setup_general_env();
        setup_rebalancer_env();
        set_env("COMPUTE_UNIT_PRICE_MICRO_LAMPORTS", "not_a_number");
        Eva01Config::new().unwrap();
    }
}
