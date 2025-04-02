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
            .map(|(address, feed_hash)| (feed_hash.clone(), *address))
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

        chunk_results
            .into_iter()
            .flatten()
            .flatten()
            .for_each(|simulated_response| {
                if let Some(price) = calculate_price(simulated_response.results) {
                    prices.push((
                        *feed_hash_to_oracle_hash_map
                            .get(&simulated_response.feedHash)
                            .unwrap(),
                        price,
                    ));
                }
            });

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_price() {
        // Test empty vector
        assert_eq!(calculate_price(vec![]), None);

        // Test single element vector
        assert_eq!(calculate_price(vec![5.0]), Some(5.0));

        // Test odd number of elements
        assert_eq!(calculate_price(vec![3.0, 1.0, 2.0]), Some(2.0));

        // Test even number of elements
        assert_eq!(calculate_price(vec![4.0, 1.0, 3.0, 2.0]), Some(2.5));
    }
}
