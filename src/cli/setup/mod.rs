use crate::config::{Eva01Config, GeneralConfig, LiquidatorCfg, RebalancerCfg};
use solana_account_decoder::{UiAccountEncoding, UiDataSliceConfig};
use solana_client::rpc_client::RpcClient;
use solana_client::{
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType},
};
use solana_program::pubkey::Pubkey;
use solana_sdk::{
    signature::{read_keypair_file, Signer},
    signer::keypair::Keypair,
};
use std::io::Write;
use std::path::PathBuf;
/// 1º -> Ask for path where the config file will be stored
///
/// 2º -> Ask for a solana RPC endpoint url
///    -> Verify if we have access to the RPC endpoint
///
/// 3º -> Ask for the signer keypair path
///    -> Check if the MarginfiAccount is already initialized in that keypair
///       If not, try to initialize a account
///
/// 4º -> Ask for the yellowstone rpc and optinal x token
pub async fn setup() -> anyhow::Result<()> {
    // 1º Step
    let configuration_path = PathBuf::from(
        prompt_user("Pretended configuration file location\nExample: /home/mrgn/.config/liquidator/config.toml\n> ")?
    );

    // 2º Step
    let rpc_url = prompt_user("RPC endpoint url\n> ")?;
    let rpc_client = RpcClient::new(rpc_url.clone());

    // 3º Step
    let (keypair_path, signer_keypair) = ask_keypair_until_valid()?;
    let accounts = marginfi_account_by_authority(signer_keypair.pubkey(), rpc_client).await?;
    if accounts.is_empty() {
        let create_new =
            prompt_user("There is no marginfi account \nDo you wish to create a new one? Y/n\n> ")?
                .as_str()
                != "n";
        if !create_new {
            println!("Can't proceed without a marginfi account.");
            return Err(anyhow::anyhow!("Can't proceed without a marginfi account."));
        }
        // Initialize a marginfi account
    }

    // 4º step
    let yellowstone_endpoint = prompt_user("Yellowstone endpoint url\n> ")?;

    let yellowstone_x_token = {
        let x_token =
            prompt_user("Do you wish to add yellowstone x token? \nPress enter if not\n> ")?;

        if x_token.is_empty() {
            None
        } else {
            Some(x_token)
        }
    };

    let general_config = GeneralConfig {
        rpc_url,
        yellowstone_endpoint,
        yellowstone_x_token,
        signer_pubkey: signer_keypair.pubkey(),
        keypair_path,
        liquidator_account: accounts[0],
        compute_unit_price_micro_lamports: GeneralConfig::default_compute_unit_price_micro_lamports(
        ),
        marginfi_program_id: GeneralConfig::default_marginfi_program_id(),
        marginfi_group_address: GeneralConfig::default_marginfi_group_address(),
        account_whitelist: GeneralConfig::default_account_whitelist(),
    };

    let liquidator_config = LiquidatorCfg {
        min_profit: LiquidatorCfg::default_min_profit(),
        max_liquidation_value: None,
    };

    let rebalancer_config = RebalancerCfg {
        token_account_dust_threshold: RebalancerCfg::default_token_account_dust_threshold(),
        preferred_mints: RebalancerCfg::default_preferred_mints(),
        swap_mint: RebalancerCfg::default_swap_mint(),
        jup_swap_api_url: RebalancerCfg::default_jup_swap_api_url(),
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

fn prompt_user(prompt_text: &str) -> anyhow::Result<String> {
    print!("{}", prompt_text);
    let mut input = String::new();
    std::io::stdout().flush()?;
    std::io::stdin().read_line(&mut input)?;
    input.pop();
    Ok(input)
}

/// Simply asks the keypair path until it is a valid one,
/// Returns (keypair_path, signer_keypair)
fn ask_keypair_until_valid() -> anyhow::Result<(String, Keypair)> {
    print!("Keypair file path\n");
    loop {
        let keypair_path = prompt_user("> ")?;
        match read_keypair_file(&keypair_path) {
            Ok(keypair) => return Ok((keypair_path, keypair)),
            Err(_) => {
                println!("Failed to load the keypair from the provided path. Please try again");
            }
        }
    }
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
