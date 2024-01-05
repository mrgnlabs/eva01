use backoff::{retry, ExponentialBackoff};
use log::info;
use yellowstone_grpc_client::GeyserGrpcClientError;
use yellowstone_grpc_proto::prelude::*;

pub struct GeyserService {
    endpoint: String,
    x_token: Option<String>,
}

impl GeyserService {
    pub fn new() -> Self {
        GeyserService {}
    }

    pub async fn connect(&self) -> Result<(), Box<GeyserGrpcClientError>> {
        retry(ExponentialBackoff::default(), move || {
            let endpoint = self.endpoint.clone();
            let x_token = self.x_token.clone();

            let mut geyser_client =
                yellowstone_grpc_client::GeyserGrpcClient::connect(endpoint, x_token, None)
                    .map_err(|e| backoff::Error::transient(e))?;

            info!("Connected to geyser");

            tokio::spawn(async move {
                geyser_client.subscribe_once2(SubscribeRequest {
                    ..Default::default()
                });
            });

            Ok::<(), backoff::Error<GeyserGrpcClientError>>(())
        });

        Ok(())
    }
}
