use crate::{
    config::{Eva01Config, GeneralConfig, LiquidatorCfg, RebalancerCfg},
    utils::{ask_keypair_until_valid, expand_tilde, is_valid_url, prompt_user},
};

use anchor_lang::Discriminator;
use anyhow::bail;
use lazy_static::lazy_static;
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
        block_engine_url: GeneralConfig::default_block_engine_url(),
        signer_pubkey: signer_keypair.pubkey(),
        keypair_path,
        compute_unit_price_micro_lamports: GeneralConfig::default_compute_unit_price_micro_lamports(
        ),
        compute_unit_limit: GeneralConfig::default_compute_unit_limit(),
        marginfi_program_id,
        marginfi_groups_whitelist: Some(vec![marginfi_group_address]),
        marginfi_groups_blacklist: None,
        account_whitelist: GeneralConfig::default_account_whitelist(),
        address_lookup_tables: GeneralConfig::default_address_lookup_tables(),
        solana_clock_refresh_interval: GeneralConfig::default_sol_clock_refresh_interval(),
    };

    let liquidator_config = LiquidatorCfg {
        min_profit: LiquidatorCfg::default_min_profit(),
        max_liquidation_value: None,
        isolated_banks,
    };

    let rebalancer_config = RebalancerCfg {
        token_account_dust_threshold: RebalancerCfg::default_token_account_dust_threshold(),
        preferred_mints: RebalancerCfg::default_preferred_mints(),
        swap_mint: RebalancerCfg::default_swap_mint(),
        jup_swap_api_url: RebalancerCfg::default_jup_swap_api_url().to_string(),
        compute_unit_price_micro_lamports: RebalancerCfg::default_compute_unit_price_micro_lamports(
        ),
        slippage_bps: RebalancerCfg::default_slippage_bps(),
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
                #[allow(deprecated)]
                RpcFilterType::Memcmp(Memcmp {
                    offset: 8,
                    #[allow(deprecated)]
                    bytes: MemcmpEncodedBytes::Base58(marginfi_group_id.to_string()),
                    #[allow(deprecated)]
                    encoding: None,
                }),
                #[allow(deprecated)]
                RpcFilterType::Memcmp(Memcmp {
                    offset: 8 + 32,
                    #[allow(deprecated)]
                    bytes: MemcmpEncodedBytes::Base58(authority.to_string()),
                    #[allow(deprecated)]
                    encoding: None,
                }),
            ]),
            with_context: Some(false),
        },
    )?;

    let marginfi_account_pubkeys: Vec<Pubkey> = marginfi_account_address
        .iter()
        .map(|(pubkey, _)| *pubkey)
        .collect();

    Ok(marginfi_account_pubkeys)
}

pub fn marginfi_groups_by_program(
    rpc_client: &RpcClient,
    marginfi_program_id: Pubkey,
) -> anyhow::Result<Vec<Pubkey>> {
    let discriminator_bytes = marginfi::state::marginfi_group::MarginfiGroup::discriminator();

    let filters = vec![
        #[allow(deprecated)]
        RpcFilterType::Memcmp(Memcmp {
            offset: 0,
            #[allow(deprecated)]
            bytes: MemcmpEncodedBytes::Base58(bs58::encode(discriminator_bytes).into_string()),
            #[allow(deprecated)]
            encoding: None,
        }),
    ];

    let accounts = rpc_client.get_program_accounts_with_config(
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
            filters: Some(filters),
            with_context: Some(false),
        },
    )?;

    let pubkeys: Vec<Pubkey> = accounts.iter().map(|(pubkey, _)| *pubkey).collect();

    Ok(pubkeys)
}
