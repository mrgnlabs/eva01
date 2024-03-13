use state_engine::engine::StateEngineService;
use tokio::task;

mod state_engine;
mod utils;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Assemble stateful engine service
    let data_handle = task::spawn(async { StateEngineService::start_and_run(None).await });

    data_handle.await?.unwrap();

    Ok(())
}
