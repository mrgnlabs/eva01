use solana_sdk::pubkey::Pubkey;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use switchboard_on_demand_client::CrossbarClient;

/// CrossbarMaintainer will maintain the feeds prices
/// with simulated prices from the crossbar service
pub(crate) struct CrossbarMaintainer {
    crossbar_client: CrossbarClient,
}

impl CrossbarMaintainer {
    /// Creates a new CrossbarMaintainer empty instance
    pub fn new() -> Self {
        let crossbar_client = CrossbarClient::default(None);
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

        let feed_hashes: Vec<&str> = feeds
            .iter()
            .map(|(_, feed_hash)| feed_hash.as_str())
            .collect();

        let simulated_prices = self
            .crossbar_client
            .simulate_feeds(&feed_hashes)
            .await
            .unwrap();
        let mut prices = Vec::new();
        for simulated_response in simulated_prices {
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
