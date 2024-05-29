use crate::config::Eva01Config;
use clap::Parser;

/// Main Clap app for the CLI
pub mod app;

/// Entrypoints for the Eva
pub mod entrypoints;

/// A wizard like setup menu for creating the liquidator configuration
pub mod setup;

/// Main entrypoint for the Eva
pub async fn main_entry() {
    let args = app::Args::parse();

    match args.cmd {
        app::Commands::Run { path } => {
            let config = Eva01Config::try_load_from_file(path).unwrap();
            entrypoints::run_liquidator(config).await;
        }
        app::Commands::Setup => {
            entrypoints::wizard_setup().await;
        }
    }
}
