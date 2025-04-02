use std::path::PathBuf;

use clap::{Parser, Subcommand};
use solana_sdk::pubkey::Pubkey;

#[derive(Parser, Debug)]
#[command(author, version, about = "Eva01 Marginfi Liquidator", long_about = None)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Commands,
}

#[allow(clippy::large_enum_variant)]
#[derive(Subcommand, Debug)]
pub enum Commands {
    #[command(about = "Run the liquidator, by the given configuration file")]
    Run {
        #[arg(required = true)]
        path: PathBuf,
    },
    #[command(about = "Setups a new configuration file, by the user preferences")]
    Setup,
    #[command(
        hide = true,
        about = "Setups a new configuration file, by the user preferences"
    )]
    SetupFromCli(SetupFromCliOpts),
}

#[derive(Parser, Debug)]
pub struct SetupFromCliOpts {
    #[arg(short = 'u', long, help = "RPC endpoint url")]
    pub rpc_url: String,
    #[arg(short = 't', long, help = "Tx landing url")]
    pub tx_landing_url: String,
    #[arg(short = 'k', long, help = "Signer keypair path")]
    pub keypair_path: PathBuf,
    #[arg(
        long,
        help = "Signer pubkey, if not provided, will be derived from the keypair"
    )]
    pub signer_pubkey: Option<Pubkey>,
    #[arg(long, help = "Liquidator account")]
    pub marginfi_account: Option<Pubkey>,
    #[arg(long, help = "Yellowstone endpoint url")]
    pub yellowstone_endpoint: String,
    #[arg(long, help = "Yellowstone x token")]
    pub yellowstone_x_token: Option<String>,
    #[arg(long, help = "Compute unit price in micro lamports")]
    pub compute_unit_price_micro_lamports: Option<u64>,
    #[arg(
        long,
        help = "Marginfi program id",
        default_value = "MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA"
    )]
    pub marginfi_program_id: Pubkey,
    #[arg(
        long,
        help = "Marginfi group address",
        default_value = "4qp6Fx6tnZkY5Wropq9wUYgtFxXKwE6viZxFHg3rdAG8"
    )]
    pub marginfi_group_address: Pubkey,
    #[arg(
        long,
        help = "Minimum profit to consider a liquidation",
        default_value = "1"
    )]
    pub min_profit: f64,
    #[arg(long, help = "Maximum liquidation value")]
    pub max_liquidation_value: Option<f64>,
    #[arg(long, help = "Token account dust threshold", default_value = "0.01")]
    pub token_account_dust_threshold: f64,
    #[arg(
        long,
        help = "Preferred mints",
        default_value = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
    )]
    pub preferred_mints: Vec<Pubkey>,
    #[arg(
        long,
        help = "Swap mint",
        default_value = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
    )]
    pub swap_mint: Pubkey,
    #[arg(
        long,
        help = "Jup swap api url",
        default_value = "https://quote-api.jup.ag/v6"
    )]
    pub jup_swap_api_url: String,
    #[arg(long, help = "Slippage basis points", default_value = "250")]
    pub default_slippage_bps: u16,
    #[arg(long, help = "Isolated banks")]
    pub isolated_banks: bool,
    #[arg(help = "Path to save the configuration file")]
    pub configuration_path: PathBuf,
    #[arg(short = 'y', long, help = "Auto confirm the setup")]
    pub yes: bool,
}
