use fixed::types::I80F48;
use log::debug;
use marginfi::state::price::{OraclePriceFeedAdapter, OraclePriceType, PriceAdapter, PriceBias};
use solana_program::pubkey::Pubkey;

pub trait OracleWrapperTrait {
    fn new(address: Pubkey, price_adapter: OraclePriceFeedAdapter) -> Self;
    fn get_price_of_type(
        &self,
        oracle_type: OraclePriceType,
        price_bias: Option<PriceBias>,
    ) -> anyhow::Result<I80F48>;
    fn get_address(&self) -> Pubkey;
}

#[derive(Clone)]
pub struct OracleWrapper {
    pub address: Pubkey,
    pub price_adapter: OraclePriceFeedAdapter,
    // Simulated price are only for swb pull oracles
    pub simulated_price: Option<f64>,
}

impl OracleWrapperTrait for OracleWrapper {
    fn new(address: Pubkey, price_adapter: OraclePriceFeedAdapter) -> Self {
        Self {
            address,
            price_adapter,
            simulated_price: None,
        }
    }
    fn get_price_of_type(
        &self,
        oracle_type: OraclePriceType,
        price_bias: Option<PriceBias>,
    ) -> anyhow::Result<I80F48> {
        match self.simulated_price {
            Some(price) => {
                debug!("USING SIMULATED PRICE!");
                Ok(I80F48::from_num(price))
            }
            None => Ok(self
                .price_adapter
                .get_price_of_type(oracle_type, price_bias)?),
        }
    }

    fn get_address(&self) -> Pubkey {
        self.address
    }
}

pub fn price_adapter_name(adapter: &OraclePriceFeedAdapter) -> &'static str {
    match adapter {
        OraclePriceFeedAdapter::PythLegacy(_) => "PythLegacy",
        OraclePriceFeedAdapter::SwitchboardV2(_) => "SwitchboardV2",
        OraclePriceFeedAdapter::PythPushOracle(_) => "PythPushOracle",
        OraclePriceFeedAdapter::SwitchboardPull(_) => "SwitchboardPull",
    }
}

#[cfg(test)]
pub mod test_utils {
    use std::str::FromStr;

    use super::*;

    #[derive(Clone)]
    pub struct TestOracleWrapper {
        pub price: f64,
        pub bias: f64,
        pub address: Pubkey,
    }

    const SOL_ORACLE_ADDRESS: &str = "11111119rSGfPZLcyCGzY4uYEL1fkzJr6fke9qKxb";
    const USDC_ORACLE_ADDRESS: &str = "1111111Af7Udc9v3L82dQM5b4zee1Xt77Be4czzbH";
    const BONK_ORACLE_ADDRESS: &str = "8ihFLu5FimgTQ1Unh4dVyEHUGodJ5gJQCrQf4KUVB9bN";

    impl Default for TestOracleWrapper {
        fn default() -> Self {
            TestOracleWrapper {
                price: 42.0,
                bias: 5.0,
                address: Pubkey::new_unique(),
            }
        }
    }

    impl TestOracleWrapper {
        pub fn test_sol() -> Self {
            Self {
                price: 200.0,
                bias: 10.0,
                address: Pubkey::from_str(SOL_ORACLE_ADDRESS).unwrap(),
            }
        }

        pub fn test_usdc() -> Self {
            Self {
                price: 1.0,
                bias: 0.1,
                address: Pubkey::from_str(USDC_ORACLE_ADDRESS).unwrap(),
            }
        }

        pub fn test_bonk() -> Self {
            Self {
                price: 1000.0,
                bias: 1.0,
                address: Pubkey::from_str(BONK_ORACLE_ADDRESS).unwrap(),
            }
        }
    }

    impl OracleWrapperTrait for TestOracleWrapper {
        fn new(_: Pubkey, _: OraclePriceFeedAdapter) -> Self {
            TestOracleWrapper {
                price: 42.0,
                bias: 5.0,
                address: Pubkey::new_unique(),
            }
        }

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

        fn get_address(&self) -> Pubkey {
            self.address
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
