use env_logger::Builder;
use log::{debug, LevelFilter};
use state_engine::engine::StateEngineService;
use tokio::task;

mod state_engine;
mod utils;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Assemble logger
    let mut builder = Builder::new();
    builder.filter(None, LevelFilter::Debug).init();

    // Assemble stateful engine service
    debug!("starting eva");
    let data_handle = task::spawn(async { StateEngineService::start_and_run(None).await });

    data_handle.await?.unwrap();

    Ok(())
}
