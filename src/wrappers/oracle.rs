use fixed::types::I80F48;
use marginfi::state::price::{OraclePriceFeedAdapter, OraclePriceType, PriceAdapter, PriceBias};
use solana_program::pubkey::Pubkey;

pub trait OracleWrapperTrait {
    fn get_price_of_type(
        &self,
        oracle_type: OraclePriceType,
        price_bias: Option<PriceBias>,
    ) -> anyhow::Result<I80F48>;
}

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

    pub fn is_switchboard_pull(&self) -> bool {
        matches!(
            self.price_adapter,
            OraclePriceFeedAdapter::SwitchboardPull(_)
        )
    }
}

impl OracleWrapperTrait for OracleWrapper {
    fn get_price_of_type(
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

#[cfg(test)]
pub struct TestOracleWrapper {
    pub simulated_price: f64,
}

#[cfg(test)]
impl OracleWrapperTrait for TestOracleWrapper {
    fn get_price_of_type(
        &self,
        _: OraclePriceType,
        _: Option<PriceBias>,
    ) -> anyhow::Result<I80F48> {
        Ok(I80F48::from_num(self.simulated_price))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_oracle() {
        let oracle = TestOracleWrapper {
            simulated_price: 42.0,
        };

        assert_eq!(
            oracle
                .get_price_of_type(OraclePriceType::RealTime, None)
                .unwrap(),
            I80F48::from_num(42.0)
        );
    }
}
