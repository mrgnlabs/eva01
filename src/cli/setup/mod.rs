use anchor_lang::Discriminator;
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
use solana_sdk::bs58;
use std::path::PathBuf;

lazy_static! {
    static ref DEFAULT_CONFIG_PATH: PathBuf = {
        let mut path = dirs::home_dir().expect("Couldn't find the config directory");
        path.push(".config");
        path.push("eva01");

        path
    };
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

#[derive(Deserialize, Debug)]
pub struct PoolResponse {
    pub data: Vec<Pool>,
}

#[derive(Deserialize, Debug)]
pub struct Pool {
    pub group: String,
    pub base_bank: Bank,
    pub quote_bank: Bank,
}

#[derive(Deserialize, Debug)]
pub struct Bank {
    pub details: BankDetails,
}

#[derive(Deserialize, Debug)]
pub struct BankDetails {
    pub total_deposits_usd: f64,
}

pub fn get_active_arena_pools(
    marginfi_api_url: &String,
    marginfi_api_key: &String,
    total_deposits_usd_threshold: u64,
) -> anyhow::Result<Vec<Pubkey>> {
    let client = Client::new();
    let res = client
        .get(marginfi_api_url)
        .header("x-api-key", marginfi_api_key)
        .send()?
        .json::<PoolResponse>()?;

    let mut groups_with_deposits: Vec<(Pubkey, u64)> = res
        .data
        .into_iter()
        .filter_map(|pool| {
            let total_deposits = std::cmp::max(
                pool.base_bank.details.total_deposits_usd as u64,
                pool.quote_bank.details.total_deposits_usd as u64,
            );
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
