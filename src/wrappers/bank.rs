use super::oracle::{OracleWrapper, OracleWrapperTrait};
use fixed::types::I80F48;
use marginfi::state::{
    marginfi_account::{calc_amount, calc_value, BalanceSide, RequirementType},
    marginfi_group::Bank,
    price::{OraclePriceType, PriceBias},
};
use solana_program::pubkey::Pubkey;

#[derive(Clone)]
pub struct BankWrapperT<T: OracleWrapperTrait> {
    pub address: Pubkey,
    pub bank: Bank,
    pub oracle_adapter: T,
}

pub type BankWrapper = BankWrapperT<OracleWrapper>;

impl<T: OracleWrapperTrait + Clone> BankWrapperT<T> {
    pub fn new(address: Pubkey, bank: Bank, oracle_adapter: T) -> Self {
        Self {
            address,
            bank,
            oracle_adapter,
        }
    }

    fn get_pricing_params(
        side: BalanceSide,
        requirement_type: RequirementType,
    ) -> (Option<PriceBias>, OraclePriceType) {
        match (side, requirement_type) {
            (BalanceSide::Assets, RequirementType::Initial) => {
                (Some(PriceBias::Low), OraclePriceType::TimeWeighted)
            }
            (BalanceSide::Assets, RequirementType::Maintenance) => {
                (Some(PriceBias::Low), OraclePriceType::RealTime)
            }
            (BalanceSide::Liabilities, RequirementType::Initial) => {
                (Some(PriceBias::High), OraclePriceType::TimeWeighted)
            }
            (BalanceSide::Liabilities, RequirementType::Maintenance) => {
                (Some(PriceBias::High), OraclePriceType::RealTime)
            }
            _ => (None, OraclePriceType::RealTime),
        }
    }

    pub fn calc_amount(
        &self,
        value: I80F48,
        side: BalanceSide,
        requirement_type: RequirementType,
    ) -> anyhow::Result<I80F48> {
        let (price_bias, oracle_type) = Self::get_pricing_params(side, requirement_type);

        let price = self
            .oracle_adapter
            .get_price_of_type(oracle_type, price_bias)
            .unwrap();

        Ok(calc_amount(value, price, self.bank.mint_decimals)?)
    }

    pub fn calc_value(
        &self,
        amount: I80F48,
        side: BalanceSide,
        requirement_type: RequirementType,
    ) -> anyhow::Result<I80F48> {
        let (price_bias, oracle_type) = Self::get_pricing_params(side, requirement_type);

        let price = self
            .oracle_adapter
            .get_price_of_type(oracle_type, price_bias)
            .unwrap();

        Ok(calc_value(amount, price, self.bank.mint_decimals, None)?)
    }
}

#[cfg(test)]
use super::oracle::test_utils::TestOracleWrapper;

#[cfg(test)]
pub mod test_utils {
    use super::*;
    use anyhow::Result;
    use marginfi::state::{marginfi_group::BankConfig, price::OracleSetup};
    use std::str::FromStr;

    pub type TestBankWrapper = BankWrapperT<TestOracleWrapper>;

    const SOL_BANK_ADDRESS: &str = "1111111Bs8Haw3nAsWf5hmLfKzc6PMEzcxUCKkVYK";
    const USDC_BANK_ADDRESS: &str = "11111117353mdUKehx9GW6JNHznGt5oSZs9fWkVkB";
    const BONK_BANK_ADDRESS: &str = "DeyH7QxWvnbbaVB4zFrf4hoq7Q8z1ZT14co42BGwGtfM";

    impl TestBankWrapper {
        pub fn test_sol() -> Self {
            let mut bank = Bank::new(
                Pubkey::new_unique(),
                BankConfig::default(),
                Pubkey::new_unique(),
                6u8,
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                0i64,
                0u8,
                0u8,
                0u8,
                0u8,
                0u8,
                0u8,
            );
            let oracle = TestOracleWrapper::test_sol();
            bank.config.oracle_setup = OracleSetup::PythPushOracle;
            bank.config.oracle_keys[0] = oracle.address;
            BankWrapperT::new(Pubkey::from_str(SOL_BANK_ADDRESS).unwrap(), bank, oracle)
        }

        pub fn test_usdc() -> Self {
            let mut bank = Bank::new(
                Pubkey::new_unique(),
                BankConfig::default(),
                Pubkey::new_unique(),
                2u8,
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                0i64,
                0u8,
                0u8,
                0u8,
                0u8,
                0u8,
                0u8,
            );
            let oracle = TestOracleWrapper::test_usdc();
            bank.config.oracle_setup = OracleSetup::PythPushOracle;
            bank.config.oracle_keys[0] = oracle.address;
            BankWrapperT::new(Pubkey::from_str(USDC_BANK_ADDRESS).unwrap(), bank, oracle)
        }

        pub fn test_bonk() -> Self {
            let mut bank = Bank::new(
                Pubkey::new_unique(),
                BankConfig::default(),
                Pubkey::new_unique(),
                2u8,
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                Pubkey::new_unique(),
                0i64,
                0u8,
                0u8,
                0u8,
                0u8,
                0u8,
                0u8,
            );
            let oracle = TestOracleWrapper::test_bonk();
            bank.config.oracle_setup = OracleSetup::SwitchboardPull;
            bank.config.oracle_keys[0] = oracle.address;
            BankWrapperT::new(Pubkey::from_str(BONK_BANK_ADDRESS).unwrap(), bank, oracle)
        }
    }

    pub fn get_pyth_pull_oracle_wrapper<T: OracleWrapperTrait + Clone>(
        bank_pk: &Pubkey,
    ) -> Result<T> {
        let wrapper = match bank_pk.to_string().as_str() {
            SOL_BANK_ADDRESS => TestOracleWrapper::test_sol(),
            USDC_BANK_ADDRESS => TestOracleWrapper::test_usdc(),
            BONK_BANK_ADDRESS => TestOracleWrapper::test_bonk(),
            _ => return Err(anyhow::anyhow!("Unknown bank address: {}", bank_pk)),
        };
        Ok(T::new(wrapper.address, None))
    }
}

#[cfg(test)]
mod tests {
    use marginfi::assert_eq_with_tolerance;

    use super::test_utils::*;
    use super::*;

    #[test]
    fn test_bank_wrapper() {
        let usdc_bank = TestBankWrapper::test_usdc();
        assert_eq_with_tolerance!(
            usdc_bank
                .calc_amount(
                    I80F48::from_num(9000.0),
                    BalanceSide::Assets,
                    RequirementType::Initial
                )
                .unwrap(),
            I80F48::from_num(1000000.0),
            1e-6
        );
        assert_eq_with_tolerance!(
            usdc_bank
                .calc_value(
                    I80F48::from_num(20000.0),
                    BalanceSide::Assets,
                    RequirementType::Initial
                )
                .unwrap(),
            I80F48::from_num(180.0),
            1e-6
        );

        let sol_bank = TestBankWrapper::test_sol();
        assert_eq_with_tolerance!(
            sol_bank
                .calc_amount(
                    I80F48::from_num(1050.0),
                    BalanceSide::Liabilities,
                    RequirementType::Initial
                )
                .unwrap(),
            I80F48::from_num(5000000.0),
            1e-6
        );
        assert_eq_with_tolerance!(
            sol_bank
                .calc_value(
                    I80F48::from_num(30000000.0),
                    BalanceSide::Liabilities,
                    RequirementType::Initial
                )
                .unwrap(),
            I80F48::from_num(6300.0),
            1e-6
        );
    }
}
