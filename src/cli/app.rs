use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(author, version, about = "Marginfi Liquidator Eva", long_about = None)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    #[command(about = "Run the liquidator, by the given configuration file")]
    Run {
        #[arg(required = true)]
        path: PathBuf,
    },
    #[command(about = "Setups a new configuration file, by the user preferences")]
    Setup,
}
