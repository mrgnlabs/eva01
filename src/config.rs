use fixed::types::I80F48;
use solana_sdk::pubkey::Pubkey;
use std::{collections::HashMap, str::FromStr};

#[derive(Clone, Debug)]
pub struct TokenThresholds {
    pub declared_value: f64,
    pub min_value: I80F48,
    pub max_value: I80F48,
}

#[derive(Clone, Debug)]
pub struct Eva01Config {
    pub rpc_url: String,
    pub yellowstone_endpoint: String,
    pub yellowstone_x_token: Option<String>,
    pub wallet_keypair: Vec<u8>,
    pub compute_unit_price_micro_lamports: u64,
    pub marginfi_program_id: Pubkey,
    pub marginfi_group_key: Pubkey,
    pub address_lookup_tables: Vec<Pubkey>,
    pub min_profit: f64,
    pub healthcheck_port: u16,
    pub metrics_bind_addr: String,
    pub metrics_port: u16,
    pub crossbar_api_url: Option<String>,
    pub jup_swap_api_url: String,
    pub jup_swap_api_key: String,
    pub swap_mint: Pubkey,
    pub slippage_bps: u16,
    pub token_thresholds: HashMap<Pubkey, TokenThresholds>,
    pub default_token_max_threshold: I80F48,
    pub token_dust_threshold: I80F48,
    pub unstable_swb_feeds: Vec<Pubkey>,
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

        let marginfi_program_id = Pubkey::from_str(
            &std::env::var("MARGINFI_PROGRAM_ID")
                .expect("MARGINFI_PROGRAM_ID environment variable is not set"),
        )
        .expect("Invalid MARGINFI_PROGRAM_ID Pubkey");

        let marginfi_group_key = Pubkey::from_str(
            &std::env::var("MARGINFI_GROUP_KEY")
                .expect("MARGINFI_GROUP_KEY environment variable is not set"),
        )
        .expect("Invalid MARGINFI_GROUP_KEY Pubkey");

        let address_lookup_tables: Vec<Pubkey> =
            parse_pubkey_list("ADDRESS_LOOKUP_TABLES").unwrap_or_else(|_| vec![]);

        let min_profit: f64 = std::env::var("MIN_PROFIT")
            .expect("MIN_PROFIT environment variable is not set")
            .parse()
            .expect("Invalid MIN_PROFIT number");

        let healthcheck_port: u16 = std::env::var("HEALTHCHECK_PORT")
            .unwrap_or("3000".to_string())
            .parse()
            .expect("Invalid HEALTHCHECK_PORT number");

        let metrics_bind_addr =
            std::env::var("METRICS_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0".to_string());
        let metrics_port: u16 = std::env::var("METRICS_PORT")
            .expect("METRICS_PORT environment variable is not set")
            .parse()
            .expect("Invalid METRICS_PORT number");

        let crossbar_api_url = std::env::var("CROSSBAR_API_URL").ok();

        let jup_swap_api_url = std::env::var("JUP_SWAP_API_URL")
            .expect("JUP_SWAP_API_URL environment variable is not set");
        let jup_swap_api_key = std::env::var("JUP_SWAP_API_KEY")
            .expect("JUP_SWAP_API_KEY environment variable is not set");

        let slippage_bps: u16 = std::env::var("SLIPPAGE_BPS")
            .expect("SLIPPAGE_BPS environment variable is not set")
            .parse()
            .expect("Invalid SLIPPAGE_BPS number: {:#?}");

        let swap_mint = Pubkey::from_str(
            &std::env::var("SWAP_MINT").expect("SWAP_MINT environment variable is not set"),
        )
        .expect("Invalid SWAP_MINT Pubkey");

        let token_thresholds = load_token_thresholds_from_env()?;

        let default_token_max_threshold = I80F48::from_num(
            std::env::var("DEFAULT_TOKEN_MAX_THRESHOLD")
                .expect("DEFAULT_TOKEN_MAX_THRESHOLD environment variable is not set")
                .parse::<f64>()
                .expect("Invalid DEFAULT_TOKEN_MAX_THRESHOLD number"),
        );

        let token_dust_threshold = I80F48::from_num(
            std::env::var("TOKEN_DUST_THRESHOLD")
                .unwrap_or("0.001".to_string())
                .parse::<f64>()
                .expect("Invalid TOKEN_DUST_THRESHOLD number"),
        );

        let unstable_swb_feeds: Vec<Pubkey> =
            parse_pubkey_list("UNSTABLE_SWB_FEEDS").unwrap_or_else(|_| vec![]);

        Ok(Eva01Config {
            rpc_url,
            yellowstone_endpoint,
            yellowstone_x_token,
            wallet_keypair,
            compute_unit_price_micro_lamports,
            marginfi_program_id,
            marginfi_group_key,
            address_lookup_tables,
            min_profit,
            healthcheck_port,
            metrics_bind_addr,
            metrics_port,
            crossbar_api_url,
            jup_swap_api_url,
            jup_swap_api_key,
            swap_mint,
            slippage_bps,
            token_thresholds,
            default_token_max_threshold,
            token_dust_threshold,
            unstable_swb_feeds,
        })
    }
}

pub fn load_token_thresholds_from_env() -> anyhow::Result<HashMap<Pubkey, TokenThresholds>> {
    match std::env::var("TOKEN_THRESHOLDS") {
        Ok(s) if !s.trim().is_empty() => {
            let raw: HashMap<String, (f64, f64, f64)> = serde_json::from_str(&s)?;
            let mut out = HashMap::with_capacity(raw.len());
            for (k, (declared_value, min_threshold, max_threshold)) in raw {
                let pk = Pubkey::from_str(&k).map_err(|e| {
                    anyhow::anyhow!("Invalid mint pubkey in TOKEN_THRESHOLDS: {k}: {e}")
                })?;
                if min_threshold * 2.0 > max_threshold {
                    return Err(anyhow::anyhow!(
                        "Invalid thresholds for {}: max must be greater than min * 2",
                        pk
                    ));
                }
                out.insert(
                    pk,
                    TokenThresholds {
                        declared_value,
                        min_value: I80F48::from_num(min_threshold),
                        max_value: I80F48::from_num(max_threshold),
                    },
                );
            }
            Ok(out)
        }
        _ => Ok(HashMap::new()),
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
    use figment::Jail;
    use serial_test::serial;
    use solana_sdk::signature::Keypair;

    use super::*;

    fn setup_general_env(jail: &mut Jail) {
        let keypair = serde_json::to_string(&Keypair::new().to_bytes().to_vec()).unwrap();

        let rpc_url = "http://dummy:1234";
        let yellowstone_endpoint = "http://dummy:1234";
        let yellowstone_x_token = "token";
        let compute_unit_price_micro_lamports = "1000";
        let marginfi_program_id = Pubkey::new_unique().to_string();
        let marginfi_group_key = Pubkey::new_unique().to_string();
        let address_lookup_tables = Pubkey::new_unique().to_string();
        let min_profit = "0.01";
        let default_token_max_threshold = "10.0";
        let token_dust_threshold = "0.01";
        let unstable_swb_feeds = Pubkey::new_unique().to_string();
        let metrics_port = "9898";
        let healthcheck_port = "3000";

        jail.set_env("RPC_URL", rpc_url);
        jail.set_env("YELLOWSTONE_ENDPOINT", yellowstone_endpoint);
        jail.set_env("YELLOWSTONE_X_TOKEN", yellowstone_x_token);
        jail.set_env("WALLET_KEYPAIR", &keypair);
        jail.set_env(
            "COMPUTE_UNIT_PRICE_MICRO_LAMPORTS",
            compute_unit_price_micro_lamports,
        );
        jail.set_env("MARGINFI_PROGRAM_ID", &marginfi_program_id);
        jail.set_env("MARGINFI_GROUP_KEY", &marginfi_group_key);
        jail.set_env("ADDRESS_LOOKUP_TABLES", &address_lookup_tables);
        jail.set_env("MIN_PROFIT", min_profit);
        jail.set_env("HEALTHCHECK_PORT", healthcheck_port);
        jail.set_env("METRICS_BIND_ADDR", "127.0.0.1");
        jail.set_env("METRICS_PORT", metrics_port);
        jail.set_env("DEFAULT_TOKEN_MAX_THRESHOLD", default_token_max_threshold);
        jail.set_env("TOKEN_DUST_THRESHOLD", token_dust_threshold);
        jail.set_env("UNSTABLE_SWB_FEEDS", &unstable_swb_feeds);
    }

    fn setup_rebalancer_env(jail: &mut Jail) {
        jail.set_env("TOKEN_ACCOUNT_DUST_THRESHOLD", "0.0001");
        jail.set_env("SWAP_MINT", &Pubkey::new_unique().to_string());
        jail.set_env("JUP_SWAP_API_URL", "https://dummy/swap");
        jail.set_env("JUP_SWAP_API_KEY", "API_KEY");
        jail.set_env("SLIPPAGE_BPS", "50");
    }

    #[test]
    #[serial]
    fn test_parse_pubkey_list_empty() {
        Jail::expect_with(|_jail| {
            // TEST_PUBKEY_LIST is not set in the jail, so it should return empty
            let result = parse_pubkey_list("TEST_PUBKEY_LIST").unwrap();
            assert_eq!(result.len(), 0);
            Ok(())
        });
    }

    #[test]
    #[serial]
    fn test_parse_pubkey_list_valid() {
        Jail::expect_with(|jail| {
            jail.set_env(
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
            Ok(())
        });
    }

    #[test]
    #[serial]
    fn test_parse_pubkey_list_invalid() {
        Jail::expect_with(|jail| {
            jail.set_env("TEST_PUBKEY_LIST", "invalid_pubkey");
            let result = parse_pubkey_list("TEST_PUBKEY_LIST");
            assert!(result.is_err());
            Ok(())
        });
    }

    #[test]
    #[serial]
    fn test_eva01_config_new_success() {
        Jail::expect_with(|mut jail| {
            setup_general_env(&mut jail);
            setup_rebalancer_env(&mut jail);
            let config = Eva01Config::new();
            assert!(config.is_ok());
            Ok(())
        });
    }

    #[test]
    #[serial]
    #[should_panic(expected = "RPC_URL environment variable is not set")]
    fn test_eva01_config_new_missing_env() {
        Jail::expect_with(|_jail| {
            // RPC_URL is not set in the jail, so it should panic
            let _result = Eva01Config::new();
            Ok(())
        });
    }

    #[test]
    #[serial]
    #[should_panic(expected = "Invalid MARGINFI_PROGRAM_ID Pubkey")]
    fn test_eva01_config_new_invalid_pubkey_env() {
        Jail::expect_with(|mut jail| {
            setup_general_env(&mut jail);
            setup_rebalancer_env(&mut jail);
            jail.set_env("MARGINFI_PROGRAM_ID", "not_a_pubkey");
            Eva01Config::new().unwrap();
            Ok(())
        });
    }

    #[test]
    #[serial]
    #[should_panic(expected = "Invalid COMPUTE_UNIT_PRICE_MICRO_LAMPORTS number")]
    fn test_eva01_config_new_invalid_compute_unit_price() {
        Jail::expect_with(|mut jail| {
            setup_general_env(&mut jail);
            setup_rebalancer_env(&mut jail);
            jail.set_env("COMPUTE_UNIT_PRICE_MICRO_LAMPORTS", "not_a_number");
            Eva01Config::new().unwrap();
            Ok(())
        });
    }
}
