use anchor_lang::Discriminator;
use lazy_static::lazy_static;
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

pub fn marginfi_groups_by_program(
    rpc_client: &RpcClient,
    marginfi_program_id: Pubkey,
) -> anyhow::Result<Vec<Pubkey>> {
    let discriminator_bytes = marginfi::state::marginfi_group::MarginfiGroup::DISCRIMINATOR;

    let filters = vec![RpcFilterType::Memcmp(Memcmp::new(
        0,
        MemcmpEncodedBytes::Base58(bs58::encode(discriminator_bytes).into_string()),
    ))];

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
            sort_results: None,
        },
    )?;

    let pubkeys: Vec<Pubkey> = accounts.iter().map(|(pubkey, _)| *pubkey).collect();

    Ok(pubkeys)
}
