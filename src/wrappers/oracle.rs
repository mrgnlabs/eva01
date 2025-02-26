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
pub mod test_utils {
    use super::*;

    #[derive(Clone)]
    pub struct TestOracleWrapper {
        pub price: f64,
        pub bias: f64,
    }

    impl Default for TestOracleWrapper {
        fn default() -> Self {
            TestOracleWrapper {
                price: 42.0,
                bias: 5.0,
            }
        }
    }

    impl OracleWrapperTrait for TestOracleWrapper {
        fn get_price_of_type(
            &self,
            _: OraclePriceType,
            price_bias: Option<PriceBias>,
        ) -> anyhow::Result<I80F48> {
            match price_bias {
                Some(PriceBias::Low) => Ok(I80F48::from_num(self.price - self.bias)),
                Some(PriceBias::High) => Ok(I80F48::from_num(self.price + self.bias)),
                None => Ok(I80F48::from_num(self.price)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_utils::*;
    use super::*;

    #[test]
    fn test_oracle() {
        let oracle = TestOracleWrapper::default();

        assert_eq!(
            oracle
                .get_price_of_type(OraclePriceType::RealTime, None)
                .unwrap(),
            I80F48::from_num(42.0)
        );
        assert_eq!(
            oracle
                .get_price_of_type(OraclePriceType::TimeWeighted, Some(PriceBias::Low))
                .unwrap(),
            I80F48::from_num(37.0)
        );
        assert_eq!(
            oracle
                .get_price_of_type(OraclePriceType::RealTime, Some(PriceBias::High))
                .unwrap(),
            I80F48::from_num(47.0)
        );
    }
}
