use std::sync::Arc;

use fixed::types::I80F48;
use marginfi::state::price::{OraclePriceFeedAdapter, OraclePriceType, PriceAdapter, PriceBias};
use solana_program::pubkey::Pubkey;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct OracleWrapper {
    pub address: Pubkey,
    pub price_adapter: OraclePriceFeedAdapter,
    // Simulated price are only for swb pull oracles
    pub simulated_price: Option<f64>,
    pub swb_feed_hash: Option<String>,
}

impl OracleWrapper {
    pub fn new(address: Pubkey, price_adapter: OraclePriceFeedAdapter) -> Self {
        Self {
            address,
            price_adapter,
            simulated_price: None,
            swb_feed_hash: None,
        }
    }

    pub fn get_price_of_type(
        &self,
        oracle_type: OraclePriceType,
        price_bias: Option<PriceBias>,
    ) -> anyhow::Result<I80F48> {
        match self.simulated_price {
            Some(price) => Ok(I80F48::from_num(price)),
            None => Ok(self
                .price_adapter
                .get_price_of_type(oracle_type, price_bias)?),
        }
    }
}
