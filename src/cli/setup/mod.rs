use super::app::SetupFromCliOpts;
use crate::{
    config::{Eva01Config, GeneralConfig, LiquidatorCfg, RebalancerCfg},
    utils::{ask_keypair_until_valid, expand_tilde, is_valid_url, prompt_user},
};

use anyhow::bail;
use fixed::types::I80F48;
use lazy_static::lazy_static;
use solana_account_decoder::{UiAccountEncoding, UiDataSliceConfig};
use solana_client::{
    rpc_client::RpcClient,
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType},
};
use solana_program::pubkey::Pubkey;
use solana_sdk::signature::{read_keypair_file, Signer};
use std::{ops::Not, path::PathBuf, str::FromStr};

/// Helper for initializing Marginfi Account
pub mod initialize;

lazy_static! {
    static ref DEFAULT_CONFIG_PATH: PathBuf = {
        let mut path = dirs::home_dir().expect("Couldn't find the config directory");
        path.push(".config");
        path.push("eva01");

        path
    };
}

pub async fn setup() -> anyhow::Result<()> {
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
    let rpc_client = RpcClient::new(rpc_url.clone());

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

    let input_raw = prompt_user(&format!(
        "Select marginfi group [default: {:?}]: ",
        GeneralConfig::default_marginfi_group_address()
    ))?;
    let marginfi_group_address = if input_raw.is_empty() {
        GeneralConfig::default_marginfi_group_address()
    } else {
        Pubkey::from_str(&input_raw).expect("Invalid marginfi group address")
    };

    // Marginfi account discovery/selection
    let (keypair_path, signer_keypair) = ask_keypair_until_valid()?;
    let accounts = marginfi_account_by_authority(signer_keypair.pubkey(), rpc_client).await?;
    if accounts.is_empty() {
        println!("No marginfi account found for the provided signer. Please create one first.");
        bail!("No marginfi account found");
        // TODO: initialize a marginfi account programmatically
    }

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
        liquidator_account: accounts[0],
        compute_unit_price_micro_lamports: GeneralConfig::default_compute_unit_price_micro_lamports(
        ),
        marginfi_program_id,
        marginfi_group_address,
        account_whitelist: GeneralConfig::default_account_whitelist(),
        address_lookup_tables: GeneralConfig::default_address_lookup_tables(),
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

pub async fn setup_from_cfg(
    SetupFromCliOpts {
        rpc_url,
        keypair_path,
        marginfi_account,
        yellowstone_endpoint,
        yellowstone_x_token,
        compute_unit_price_micro_lamports,
        marginfi_program_id,
        marginfi_group_address,
        min_profit,
        max_liquidation_value,
        token_account_dust_threshold,
        preferred_mints,
        swap_mint,
        jup_swap_api_url,
        default_slippage_bps,
        configuration_path,
        signer_pubkey,
        isolated_banks,
        yes,
    }: SetupFromCliOpts,
) -> anyhow::Result<()> {
    let signer_pubkey = match signer_pubkey {
        Some(pubkey) => pubkey,
        None => {
            let signer_keypair = read_keypair_file(&keypair_path)
                .unwrap_or_else(|_| panic!("Failed to read keypair from path: {:?}", keypair_path));
            signer_keypair.pubkey()
        }
    };

    let marginfi_account = match marginfi_account {
        Some(account) => account,
        None => marginfi_account_by_authority(signer_pubkey, RpcClient::new(rpc_url.clone()))
            .await
            .expect("Failed to get marginfi account by authority")
            .pop()
            .expect("No marginfi account found"),
    };

    let general_config = GeneralConfig {
        rpc_url,
        yellowstone_endpoint,
        yellowstone_x_token,
        block_engine_url: GeneralConfig::default_block_engine_url(),
        signer_pubkey,
        keypair_path,
        liquidator_account: marginfi_account,
        compute_unit_price_micro_lamports,
        marginfi_program_id,
        marginfi_group_address,
        account_whitelist: None,
        address_lookup_tables: GeneralConfig::default_address_lookup_tables(),
    };

    let liquidator_config = LiquidatorCfg {
        min_profit,
        max_liquidation_value,
        isolated_banks,
    };

    let rebalancer_config = RebalancerCfg {
        token_account_dust_threshold: I80F48::from_num(token_account_dust_threshold),
        preferred_mints,
        swap_mint,
        jup_swap_api_url,
        compute_unit_price_micro_lamports,
        slippage_bps: default_slippage_bps,
    };

    let config = Eva01Config {
        general_config,
        liquidator_config,
        rebalancer_config,
    };

    if configuration_path.exists() && !yes {
        let overwrite = prompt_user(
            "Configuration file already exists. Do you want to overwrite it? (Y/n)\n> ",
        )?;
        if overwrite.to_lowercase() != "y" {
            println!("Aborted. Configuration file not overwritten.");
            return Ok(());
        }
    }

    match config.try_save_from_config(&configuration_path) {
        Ok(_) => println!("Configuration saved into {:?}!", configuration_path),
        Err(_) => println!(
            "Couldn't save the configuration into {:?}, please try again!",
            configuration_path
        ),
    }

    Ok(())
}

async fn marginfi_account_by_authority(
    authority: Pubkey,
    rpc_client: RpcClient,
) -> anyhow::Result<Vec<Pubkey>> {
    let marginfi_account_address = rpc_client.get_program_accounts_with_config(
        &GeneralConfig::default_marginfi_program_id(),
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
                    bytes: MemcmpEncodedBytes::Base58(
                        GeneralConfig::default_marginfi_group_address().to_string(),
                    ),
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
