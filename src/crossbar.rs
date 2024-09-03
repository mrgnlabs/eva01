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
    feeds: HashMap<String, Arc<Mutex<f64>>>,
}

impl CrossbarMaintainer {
    /// Creates a new CrossbarMaintainer empty instance
    pub fn new() -> Self {
        let crossbar_client = CrossbarClient::default(None);
        Self {
            crossbar_client,
            feeds: HashMap::new(),
        }
    }

    /// Creates a new CrossbarMaintainer instance with feeds
    pub fn new_with_feeds(feeds: Vec<(String, Arc<Mutex<f64>>)>) -> Self {
        let crossbar_client: CrossbarClient = CrossbarClient::default(None);
        let feeds = feeds
            .into_iter()
            .map(|(feed_hash, price)| (feed_hash, price))
            .collect();
        Self {
            crossbar_client,
            feeds,
        }
    }

    pub async fn simulate(&self, feeds: Vec<(Pubkey, String)>) -> Vec<(Pubkey, f64)> {
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

    /// Adds a feed to the CrossbarMaintainer
    pub fn add_feed(&mut self, feed: (String, Arc<Mutex<f64>>)) {
        self.feeds.insert(feed.0, feed.1);
    }

    pub fn add_feeds(&mut self, feeds: Vec<(String, Arc<Mutex<f64>>)>) {
        for feed in feeds {
            self.feeds.insert(feed.0, feed.1);
        }
    }
}

/// Calculate the median of a list of numbers
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex;

    #[tokio::test]
    async fn test_crossbar_maintainer_new() {
        let price = Arc::new(Mutex::new(0.0));
        let feed_hash =
            "0x4c935636f2523f6aeeb6dc7b7dab0e86a13ff2c794f7895fc78851d69fdb593b".to_string();
        let price2 = Arc::new(Mutex::new(0.0));
        let feed_hash2 =
            "0x5686ebe26b52d5c67dc10b63240c6d937af75d86bfcacf46865cd5da62f760e9".to_string();
        let mut crossbar_maintainer = CrossbarMaintainer::new();
        crossbar_maintainer.add_feed((feed_hash, Arc::clone(&price)));
        crossbar_maintainer.add_feed((feed_hash2, Arc::clone(&price2)));
        crossbar_maintainer.simulate_prices().await;
        println!("Price: {:?}", price.lock().unwrap());
        println!("Price2: {:?}", price2.lock().unwrap());
    }
}
