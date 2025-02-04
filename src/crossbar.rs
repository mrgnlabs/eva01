use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use switchboard_on_demand_client::CrossbarClient;

const CHUNK_SIZE: usize = 20;

/// CrossbarMaintainer will maintain the feeds prices
/// with simulated prices from the crossbar service
pub(crate) struct CrossbarMaintainer {
    crossbar_client: CrossbarClient,
}

impl CrossbarMaintainer {
    /// Creates a new CrossbarMaintainer empty instance
    pub fn new() -> Self {
        let crossbar_client = CrossbarClient::default();
        Self { crossbar_client }
    }

    pub async fn simulate(&self, feeds: Vec<(Pubkey, String)>) -> Vec<(Pubkey, f64)> {
        if feeds.is_empty() {
            return Vec::new();
        }

        // Create a fast lookup map from feed hash to oracle hash
        let feed_hash_to_oracle_hash_map: HashMap<String, Pubkey> = feeds
            .iter()
            .map(|(address, feed_hash)| (feed_hash.clone(), address.clone()))
            .collect();

        let feed_hashes: Vec<String> = feeds
            .iter()
            .map(|(_, feed_hash)| feed_hash.clone())
            .collect();

        let chunk_futures: Vec<_> = feed_hashes
            .chunks(CHUNK_SIZE)
            .map(|chunk| {
                let client = self.crossbar_client.clone();
                let chunk_owned: Vec<String> = chunk.to_vec();

                tokio::spawn(async move {
                    let chunk_refs: Vec<&str> = chunk_owned.iter().map(|s| s.as_str()).collect();
                    client.simulate_feeds(&chunk_refs).await
                })
            })
            .collect();

        let chunk_results = match futures::future::try_join_all(chunk_futures).await {
            Ok(results) => results,
            Err(e) => {
                log::error!("Error while simulating feeds: {:?}", e);
                return Vec::new();
            }
        };

        let mut prices = Vec::new();

        for result in chunk_results {
            if let Ok(chunk_result) = result {
                for simulated_response in chunk_result {
                    if let Some(price) = calculate_price(simulated_response.results) {
                        prices.push((
                            feed_hash_to_oracle_hash_map
                                .get(&simulated_response.feedHash)
                                .unwrap()
                                .clone(),
                            price,
                        ));
                    }
                }
            }
        }

        prices
    }
}
fn calculate_price(mut numbers: Vec<f64>) -> Option<f64> {
    if numbers.is_empty() {
        return None;
    }

    numbers.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mid = numbers.len() / 2;

    if numbers.len() % 2 == 0 {
        Some((numbers[mid - 1] + numbers[mid]) / 2.0)
    } else {
        Some(numbers[mid])
    }
}
