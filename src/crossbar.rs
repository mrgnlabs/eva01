use futures::stream::{self, StreamExt};
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use switchboard_on_demand::prelude::rust_decimal::Decimal;
use switchboard_on_demand_client::CrossbarClient;

#[allow(dead_code)]
const CHUNK_SIZE: usize = 20;

/// CrossbarMaintainer will maintain the feeds prices
/// with simulated prices from the crossbar service
#[allow(dead_code)]
pub(crate) struct CrossbarMaintainer {
    crossbar_client: CrossbarClient,
}

impl CrossbarMaintainer {
    /// Creates a new CrossbarMaintainer empty instance
    pub fn new() -> Self {
        let crossbar_client = CrossbarClient::default();
        Self { crossbar_client }
    }

    #[allow(dead_code)]
    pub async fn simulate(&self, feeds: Vec<(Pubkey, String)>) -> Vec<(Pubkey, Decimal)> {
        if feeds.is_empty() {
            return Vec::new();
        }

        let feed_hash_to_pubkey: HashMap<&str, Pubkey> = feeds
            .iter()
            .map(|(pk, hash)| (hash.as_str(), *pk))
            .collect();

        let feed_hashes: Vec<String> = feeds.iter().map(|(_, hash)| hash.clone()).collect();

        // Stream chunks and limit concurrent simulations
        let results = stream::iter(feed_hashes.chunks(CHUNK_SIZE).map(|chunk| {
            let client = self.crossbar_client.clone();
            let chunk_refs: Vec<String> = chunk.to_vec(); // clone once for move

            async move {
                let chunk_refs: Vec<&str> = chunk_refs.iter().map(String::as_str).collect();
                client.simulate_feeds(&chunk_refs).await.unwrap_or_default()
            }
        }))
        .buffer_unordered(10) // Limit to 10 concurrent simulations
        .collect::<Vec<_>>()
        .await;

        let mut prices = Vec::new();

        for responses in results.into_iter().flatten() {
            if let Some(price) = calculate_price(responses.results) {
                if let Some(address) = feed_hash_to_pubkey.get(responses.feedHash.as_str()) {
                    prices.push((*address, price));
                } else {
                    log::warn!("Missing mapping for feed hash: {}", responses.feedHash);
                }
            }
        }

        prices
    }
}

#[allow(dead_code)]
fn calculate_price(mut numbers: Vec<Decimal>) -> Option<Decimal> {
    if numbers.is_empty() {
        return None;
    }

    numbers.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mid = numbers.len() / 2;

    if numbers.len() % 2 == 0 {
        Some((numbers[mid - 1] + numbers[mid]) / Decimal::from(2u32))
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
        assert_eq!(
            calculate_price(vec![Decimal::new(5, 0)]),
            Some(Decimal::new(5, 0))
        );

        // Test odd number of elements
        assert_eq!(
            calculate_price(vec![
                Decimal::new(3, 0),
                Decimal::new(1, 0),
                Decimal::new(2, 0)
            ]),
            Some(Decimal::new(2, 0))
        );

        // Test even number of elements
        assert_eq!(
            calculate_price(vec![
                Decimal::new(4, 0),
                Decimal::new(1, 0),
                Decimal::new(3, 0),
                Decimal::new(2, 0)
            ]),
            Some(Decimal::new(25, 1))
        );
    }
}
