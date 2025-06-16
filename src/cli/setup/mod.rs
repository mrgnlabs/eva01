use crate::{
    config::{Eva01Config, GeneralConfig, LiquidatorCfg, RebalancerCfg},
    utils::{ask_keypair_until_valid, expand_tilde, is_valid_url, prompt_user},
};

use anchor_lang::Discriminator;
use anyhow::bail;
use lazy_static::lazy_static;
use reqwest::blocking::Client;
use serde::Deserialize;
use solana_account_decoder::{UiAccountEncoding, UiDataSliceConfig};
use solana_client::{
    rpc_client::RpcClient,
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType},
};
use solana_program::pubkey::Pubkey;
use solana_sdk::{bs58, signature::Signer};
use std::{ops::Not, path::PathBuf, str::FromStr};

lazy_static! {
    static ref DEFAULT_CONFIG_PATH: PathBuf = {
        let mut path = dirs::home_dir().expect("Couldn't find the config directory");
        path.push(".config");
        path.push("eva01");

        path
    };
}

pub fn setup() -> anyhow::Result<()> {
    // Config location
    let input_raw = prompt_user(&format!(
        "Select config location [default: {:?}]: ",
        *DEFAULT_CONFIG_PATH
    ))?;
    let configuration_dir = if input_raw.is_empty() {
        DEFAULT_CONFIG_PATH.clone()
    } else {
        expand_tilde(&input_raw)
    };
    if !configuration_dir.exists() {
        std::fs::create_dir_all(&configuration_dir)?;
    }
    let configuration_path = configuration_dir.join("config.toml");

    // RPC config
    let rpc_url = prompt_user("RPC endpoint url [required]: ")?;
    if !is_valid_url(&rpc_url) {
        bail!("Invalid RPC endpoint");
    }

    // Target program/group
    let input_raw = prompt_user(&format!(
        "Select marginfi program [default: {:?}]: ",
        GeneralConfig::default_marginfi_program_id()
    ))?;
    let marginfi_program_id = if input_raw.is_empty() {
        GeneralConfig::default_marginfi_program_id()
    } else {
        Pubkey::from_str(&input_raw).expect("Invalid marginfi program id")
    };

    let input_raw = prompt_user(
        "Select marginfi group [main group: 4qp6Fx6tnZkY5Wropq9wUYgtFxXKwE6viZxFHg3rdAG8]: ",
    )?;
    let marginfi_group_address =
        Pubkey::from_str(&input_raw).expect("Invalid marginfi group address");

    // Marginfi account discovery/selection
    let (keypair_path, signer_keypair) = ask_keypair_until_valid()?;

    let yellowstone_endpoint = prompt_user("Yellowstone endpoint url [required]: ")?;
    let yellowstone_x_token = {
        let x_token = prompt_user("Yellowstone x-token [optional]: ")?;
        x_token.is_empty().not().then_some(x_token)
    };

    let isolated_banks =
        prompt_user("Enable isolated banks liquidation? [Y/n] ")?.to_lowercase() == "y";

    let general_config = GeneralConfig {
        rpc_url,
        yellowstone_endpoint,
        yellowstone_x_token,
        signer_pubkey: signer_keypair.pubkey(),
        keypair_path,
        compute_unit_price_micro_lamports: GeneralConfig::default_compute_unit_price_micro_lamports(
        ),
        compute_unit_limit: GeneralConfig::default_compute_unit_limit(),
        marginfi_program_id,
        marginfi_api_url: None,
        marginfi_api_key: None,
        marginfi_api_arena_threshold: None,
        marginfi_groups_whitelist: Some(vec![marginfi_group_address]),
        marginfi_groups_blacklist: None,
        account_whitelist: GeneralConfig::default_account_whitelist(),
        address_lookup_tables: GeneralConfig::default_address_lookup_tables(),
        solana_clock_refresh_interval: GeneralConfig::default_sol_clock_refresh_interval(),
        min_profit: GeneralConfig::default_min_profit(),
    };

    let liquidator_config = LiquidatorCfg {
        max_liquidation_value: None,
        isolated_banks,
    };

    let rebalancer_config = RebalancerCfg {
        token_account_dust_threshold: RebalancerCfg::default_token_account_dust_threshold(),
        swap_mint: RebalancerCfg::default_swap_mint(),
        jup_swap_api_url: RebalancerCfg::default_jup_swap_api_url().to_string(),
        slippage_bps: RebalancerCfg::default_slippage_bps(),
        compute_unit_price_micro_lamports: RebalancerCfg::default_compute_unit_price_micro_lamports(
        ),
    };

    println!(
        "{}\n\n{}\n\n{}",
        general_config, liquidator_config, rebalancer_config
    );

    let config = Eva01Config {
        general_config,
        liquidator_config,
        rebalancer_config,
    };

    match config.try_save_from_config(&configuration_path) {
        Ok(_) => println!("Configuration saved into {:?}!", configuration_path),
        Err(_) => println!(
            "Coulnd't save the configuration into {:?}, please try again!",
            configuration_path
        ),
    }

    Ok(())
}

pub fn marginfi_account_by_authority(
    authority: Pubkey,
    rpc_client: &RpcClient,
    marginfi_program_id: Pubkey,
    marginfi_group_id: Pubkey,
) -> anyhow::Result<Vec<Pubkey>> {
    let marginfi_account_address = rpc_client.get_program_accounts_with_config(
        &marginfi_program_id,
        RpcProgramAccountsConfig {
            account_config: RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::Base64),
                data_slice: Some(UiDataSliceConfig {
                    offset: 0,
                    length: 0,
                }),
                ..Default::default()
            },
            filters: Some(vec![
                RpcFilterType::Memcmp(Memcmp::new(
                    8,
                    MemcmpEncodedBytes::Base58(marginfi_group_id.to_string()),
                )),
                RpcFilterType::Memcmp(Memcmp::new(
                    8 + 32,
                    MemcmpEncodedBytes::Base58(authority.to_string()),
                )),
            ]),
            with_context: Some(false),
            sort_results: None,
        },
    )?;

    let marginfi_account_pubkeys: Vec<Pubkey> = marginfi_account_address
        .iter()
        .map(|(pubkey, _)| *pubkey)
        .collect();

    Ok(marginfi_account_pubkeys)
}

#[allow(dead_code)]
#[derive(Deserialize, Debug)]
pub struct PoolResponse {
    pub data: Vec<Pool>,
    pub metadata: Metadata,
}

#[allow(dead_code)]
#[derive(Deserialize, Debug)]
pub struct Metadata {
    pub current_page: usize,
    pub failed_pools: Option<serde_json::Value>,
    pub has_next_page: bool,
    pub has_previous_page: bool,
    pub page_size: usize,
    pub total_items: usize,
    pub total_pages: usize,
}

#[allow(dead_code)]
#[derive(Deserialize, Debug)]
pub struct Pool {
    pub base_bank: Bank,
    pub created_at: String,
    pub created_by: String,
    pub featured: bool,
    pub group: String,
    pub lookup_tables: Vec<String>,
    pub quote_bank: Bank,
    pub status: String,
}

#[allow(dead_code)]
#[derive(Deserialize, Debug)]
pub struct Bank {
    pub address: String,
    pub details: BankDetails,
    pub group: String,
    pub mint: Mint,
}

#[allow(dead_code)]
#[derive(Deserialize, Debug)]
pub struct BankDetails {
    pub borrow_rate: f64,
    pub deposit_rate: f64,
    pub total_borrows: f64,
    pub total_borrows_usd: f64,
    pub total_deposits: f64,
    pub total_deposits_usd: f64,
}

#[allow(dead_code)]
#[derive(Deserialize, Debug)]
pub struct Mint {
    pub address: String,
    pub decimals: u8,
    pub name: String,
    pub symbol: String,
    pub token_program: String,
}

pub fn get_active_arena_pools(
    marginfi_api_url: &String,
    marginfi_api_key: &String,
    total_deposits_usd_threshold: f64,
) -> anyhow::Result<Vec<Pubkey>> {
    let client = Client::new();
    let res = client
        .get(marginfi_api_url)
        .header("x-api-key", marginfi_api_key)
        .send()?
        .json::<PoolResponse>()?;

    let mut groups_with_deposits: Vec<(Pubkey, f64)> = res
        .data
        .into_iter()
        .filter_map(|pool| {
            let total_deposits = pool.quote_bank.details.total_deposits_usd;
            if total_deposits > total_deposits_usd_threshold {
                Some((pool.group.parse::<Pubkey>().ok()?, total_deposits))
            } else {
                None
            }
        })
        .collect();

    // Sort by total_deposits_usd descending
    groups_with_deposits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

    Ok(groups_with_deposits
        .into_iter()
        .map(|(pubkey, _)| pubkey)
        .collect())
}

pub fn marginfi_groups_by_program(
    rpc_client: &RpcClient,
    marginfi_program_id: Pubkey,
    arena_only: bool,
) -> anyhow::Result<Vec<Pubkey>> {
    let discriminator_bytes = marginfi::state::marginfi_group::MarginfiGroup::DISCRIMINATOR;
    let accounts = rpc_client.get_program_accounts_with_config(
        &marginfi_program_id,
        RpcProgramAccountsConfig {
            account_config: RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::Base64),
                data_slice: Some(UiDataSliceConfig {
                    offset: 8 + 32,
                    length: 8,
                }),
                ..Default::default()
            },
            filters: Some(vec![RpcFilterType::Memcmp(Memcmp::new(
                0,
                MemcmpEncodedBytes::Base58(bs58::encode(discriminator_bytes).into_string()),
            ))]),
            with_context: Some(false),
            sort_results: None,
        },
    )?;

    let pubkeys: Vec<Pubkey> = accounts
        .into_iter()
        .filter_map(|(pubkey, account)| {
            if arena_only {
                let flags_bytes: [u8; 8] = account.data[..].try_into().unwrap();
                let group_flags = u64::from_le_bytes(flags_bytes);
                let is_arena = (group_flags & (1 << 1)) != 0; // second bit stands for ARENA_GROUP
                if !is_arena {
                    return None;
                }
            }

            Some(pubkey)
        })
        .collect();

    Ok(pubkeys)
}
