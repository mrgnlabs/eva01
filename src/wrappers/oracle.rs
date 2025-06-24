use anyhow::{anyhow, Result};
use fixed::types::I80F48;
use marginfi::state::price::{
    OraclePriceFeedAdapter, OraclePriceType, OracleSetup, PriceAdapter, PriceBias,
    SwitchboardPullPriceFeed,
};
use solana_program::pubkey::Pubkey;
use solana_sdk::account_info::IntoAccountInfo;
use switchboard_on_demand_client::PullFeedAccountData;

use crate::{
    cache::Cache,
    clock_manager, thread_debug, thread_error, thread_warn,
    utils::{find_oracle_keys, load_swb_pull_account_from_bytes},
};

pub trait OracleWrapperTrait {
    fn new(address: Pubkey, price_adapter: OraclePriceFeedAdapter) -> Self;
    fn build(cache: &Cache, bank_address: &Pubkey) -> Result<Self>
    where
        Self: Sized;
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
                thread_debug!("USING SIMULATED PRICE!");
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
    fn build(cache: &Cache, bank_address: &Pubkey) -> Result<Self> {
        let bank_wrapper = cache.banks.try_get_bank(bank_address)?;
        let oracle_addresses = find_oracle_keys(&bank_wrapper.bank.config);

        let mut result: Option<Self> = None;
        match bank_wrapper.bank.config.oracle_setup {
            OracleSetup::SwitchboardPull => {
                for (oracle_address, oracle_account) in
                    cache.oracles.try_get_accounts(&oracle_addresses)?
                {
                    let mut offsets_data = [0u8; std::mem::size_of::<PullFeedAccountData>()];
                    offsets_data.copy_from_slice(
                        &oracle_account.data[8..std::mem::size_of::<PullFeedAccountData>() + 8],
                    );
                    match load_swb_pull_account_from_bytes(&offsets_data) {
                        Result::Ok(swb_feed) => {
                            let price_adapter =
                                OraclePriceFeedAdapter::SwitchboardPull(SwitchboardPullPriceFeed {
                                    feed: Box::new((&swb_feed).into()),
                                });
                            let oracle_wrapper = Self::new(oracle_address, price_adapter);
                            result = Some(oracle_wrapper);
                            break;
                        }
                        Err(e) => {
                            thread_warn!(
                            "Failed to deserialize Switchboard Pull account data for Oracle {:?} : {}",
                            oracle_address,
                            e
                        );
                            continue;
                        }
                    }
                }
            }
            OracleSetup::PythPushOracle => {
                for (oracle_address, oracle_account) in
                    cache.oracles.try_get_accounts(&oracle_addresses)?
                {
                    let mut oracle_tuple = (oracle_address, oracle_account);
                    let oracle_account_info = oracle_tuple.into_account_info();
                    match OraclePriceFeedAdapter::try_from_bank_config(
                        &bank_wrapper.bank.config,
                        &[oracle_account_info],
                        &clock_manager::get_clock(&cache.clock)?,
                    ) {
                        Result::Ok(price_adapter) => {
                            let oracle_wrapper = Self::new(oracle_address, price_adapter);
                            result = Some(oracle_wrapper);
                            break;
                        }
                        Err(e) => {
                            crate::thread_trace!(
                            "Failed to build Pyth Push price adapter for Bank {:?} and Oracle {:?} : {}",
                            bank_address,
                            oracle_address,
                            e
                        );
                            continue;
                        }
                    }
                }
            }
            OracleSetup::StakedWithPythPush => {
                if oracle_addresses.len() != 3 {
                    return Err(anyhow!(
                        "StakedWithPythPush setup requires exactly 3 oracle keys, but found {} for the Bank {:?}.",
                        oracle_addresses.len(), bank_address
                    ));
                }

                let bank_oracle_address = *oracle_addresses.first().unwrap();
                let mut bank_oracle = cache.oracles.try_get_account(&bank_oracle_address)?;
                let bank_oracle_account_info =
                    (&bank_oracle_address, &mut bank_oracle).into_account_info();

                let mint_oracle_address = *oracle_addresses.get(1).unwrap();
                let mut mint_oracle = cache.oracles.try_get_account(&mint_oracle_address)?;
                let mint_oracle_account_info =
                    (&mint_oracle_address, &mut mint_oracle).into_account_info();

                let sol_pool_oracle_address = *oracle_addresses.get(2).unwrap();
                let mut sol_pool_oracle = cache.oracles.try_get_account(&mint_oracle_address)?;
                let sol_pool_account_info =
                    (&sol_pool_oracle_address, &mut sol_pool_oracle).into_account_info();

                let adapter = OraclePriceFeedAdapter::try_from_bank_config(
                    &bank_wrapper.bank.config,
                    &[
                        bank_oracle_account_info,
                        mint_oracle_account_info,
                        sol_pool_account_info,
                    ],
                    &clock_manager::get_clock(&cache.clock)?,
                )?;

                let oracle_wrapper = Self::new(bank_oracle_address, adapter);
                result = Some(oracle_wrapper);
            }
            _ => {
                thread_error!(
                    "Unsupported Oracle setup for the Bank {:?} : {:?}",
                    bank_address,
                    bank_wrapper.bank.config.oracle_setup
                )
            }
        }

        match result {
            Some(wrapper) => Ok(wrapper),
            None => Err(anyhow!(
                "No valid oracle wrapper found for the Bank {:?}",
                bank_address
            )),
        }
    }
}

#[cfg(test)]
pub mod test_utils {
    use std::str::FromStr;

    use solana_sdk::account::Account;

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
        pub fn new(address: Pubkey, price: f64, bias: f64) -> Self {
            Self {
                price,
                bias,
                address,
            }
        }

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
            panic!("The new method is not implemented for TestOracleWrapper.");
        }

        fn build(cache: &Cache, bank_address: &Pubkey) -> Result<Self> {
            let bank_wrapper = cache.banks.try_get_bank(bank_address)?;
            if matches!(
                bank_wrapper.bank.config.oracle_setup,
                OracleSetup::SwitchboardPull
                    | OracleSetup::PythPushOracle
                    | OracleSetup::StakedWithPythPush
            ) {
                Ok(TestOracleWrapper::new(
                    bank_wrapper.bank.config.oracle_keys[0],
                    100.0,
                    5.0,
                ))
            } else {
                panic!(
                    "Unsupported Oracle type {:?}",
                    bank_wrapper.bank.config.oracle_setup
                )
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

    pub fn create_empty_oracle_account() -> Account {
        let buffer = vec![0u8; std::mem::size_of::<PullFeedAccountData>()];
        let pull_feed_data = bytemuck::try_from_bytes::<PullFeedAccountData>(&buffer).unwrap();
        let bytes: &[u8] = bytemuck::bytes_of(pull_feed_data);

        let mut oracle_account = Account::default();
        oracle_account
            .data
            .resize(std::mem::size_of::<PullFeedAccountData>() + 8, 0);
        oracle_account.data[8..].copy_from_slice(bytes);

        oracle_account
    }
}

#[cfg(test)]
mod tests {
    use crate::cache::test_utils::create_test_cache;
    use crate::wrappers::bank::test_utils::test_usdc;

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

    /// Create test for try_build_oracle_wrapper
    #[test]
    fn test_try_build_oracle_wrapper() {
        let oracle_key = Pubkey::new_unique();

        let mut usdc_bank_wrapper = test_usdc();
        usdc_bank_wrapper.bank.config.oracle_setup = OracleSetup::SwitchboardPull;
        usdc_bank_wrapper.bank.config.oracle_keys[0] = oracle_key.clone();

        let mut cache = create_test_cache(&vec![usdc_bank_wrapper.clone()]);
        cache
            .banks
            .insert(usdc_bank_wrapper.address, usdc_bank_wrapper.bank);

        let oracle_account = create_empty_oracle_account();

        // Mock oracles in the cache
        cache
            .oracles
            .try_insert(oracle_key, oracle_account)
            .unwrap();

        let oracle_wrapper: TestOracleWrapper =
            TestOracleWrapper::build(&cache, &usdc_bank_wrapper.address).unwrap();

        assert_eq!(oracle_wrapper.get_address(), oracle_key);
    }
}
