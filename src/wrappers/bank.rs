use crate::wrappers::oracle::OracleWrapperTrait;
use fixed::types::I80F48;
use marginfi::state::{
    marginfi_account::{calc_amount, calc_value, BalanceSide, RequirementType},
    marginfi_group::Bank,
    price::{OraclePriceType, PriceBias},
};
use solana_program::pubkey::Pubkey;

#[derive(Clone)]
pub struct BankWrapper {
    pub address: Pubkey,
    pub bank: Bank,
}

impl BankWrapper {
    pub fn new(address: Pubkey, bank: Bank) -> Self {
        Self { address, bank }
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
        oracle_wrapper: &impl OracleWrapperTrait,
        value: I80F48,
        side: BalanceSide,
        requirement_type: RequirementType,
    ) -> anyhow::Result<I80F48> {
        let (price_bias, oracle_type) = Self::get_pricing_params(side, requirement_type);

        let price = oracle_wrapper
            .get_price_of_type(
                oracle_type,
                price_bias,
                self.bank.config.oracle_max_confidence,
            )
            .unwrap();

        Ok(calc_amount(value, price, self.bank.mint_decimals)?)
    }

    pub fn calc_value(
        &self,
        oracle_wrapper: &impl OracleWrapperTrait,
        amount: I80F48,
        side: BalanceSide,
        requirement_type: RequirementType,
    ) -> anyhow::Result<I80F48> {
        let (price_bias, oracle_type) = Self::get_pricing_params(side, requirement_type);

        let price = oracle_wrapper
            .get_price_of_type(
                oracle_type,
                price_bias,
                self.bank.config.oracle_max_confidence,
            )
            .unwrap();

        Ok(calc_value(amount, price, self.bank.mint_decimals, None)?)
    }
}

#[cfg(test)]
pub mod test_utils {
    use crate::wrappers::oracle::test_utils::TestOracleWrapper;

    use super::*;
    use marginfi::state::{marginfi_group::BankConfig, price::OracleSetup};
    use std::str::FromStr;

    const SOL_BANK_ADDRESS: &str = "1111111Bs8Haw3nAsWf5hmLfKzc6PMEzcxUCKkVYK";
    const USDC_BANK_ADDRESS: &str = "11111117353mdUKehx9GW6JNHznGt5oSZs9fWkVkB";
    const BONK_BANK_ADDRESS: &str = "DeyH7QxWvnbbaVB4zFrf4hoq7Q8z1ZT14co42BGwGtfM";

    pub fn test_sol() -> BankWrapper {
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
        BankWrapper::new(Pubkey::from_str(SOL_BANK_ADDRESS).unwrap(), bank)
    }

    pub fn test_usdc() -> BankWrapper {
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
        BankWrapper::new(Pubkey::from_str(USDC_BANK_ADDRESS).unwrap(), bank)
    }

    pub fn test_bonk() -> BankWrapper {
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
        BankWrapper::new(Pubkey::from_str(BONK_BANK_ADDRESS).unwrap(), bank)
    }
}
#[cfg(test)]
mod tests {
    use crate::wrappers::oracle::test_utils::TestOracleWrapper;

    use super::*;

    fn setup_sol_bank_and_oracle() -> (BankWrapper, TestOracleWrapper) {
        let bank = test_utils::test_sol();
        let oracle = TestOracleWrapper::test_sol();
        (bank, oracle)
    }

    fn setup_usdc_bank_and_oracle() -> (BankWrapper, TestOracleWrapper) {
        let bank = test_utils::test_usdc();
        let oracle = TestOracleWrapper::test_usdc();
        (bank, oracle)
    }

    #[test]
    fn test_calc_amount_sol_assets_initial() {
        let (bank, oracle) = setup_sol_bank_and_oracle();
        let value = I80F48::from_num(1000);
        let result = bank.calc_amount(
            &oracle,
            value,
            BalanceSide::Assets,
            RequirementType::Initial,
        );
        assert!(result.is_ok());
        let amount = result.unwrap();
        assert!(amount > I80F48::from_num(0));
    }

    #[test]
    fn test_calc_amount_sol_liabilities_maintenance() {
        let (bank, oracle) = setup_sol_bank_and_oracle();
        let value = I80F48::from_num(500);
        let result = bank.calc_amount(
            &oracle,
            value,
            BalanceSide::Liabilities,
            RequirementType::Maintenance,
        );
        assert!(result.is_ok());
        let amount = result.unwrap();
        assert!(amount > I80F48::from_num(0));
    }

    #[test]
    fn test_calc_value_usdc_assets_initial() {
        let (bank, oracle) = setup_usdc_bank_and_oracle();
        let amount = I80F48::from_num(100);
        let result = bank.calc_value(
            &oracle,
            amount,
            BalanceSide::Assets,
            RequirementType::Initial,
        );
        assert!(result.is_ok());
        let value = result.unwrap();
        assert!(value > I80F48::from_num(0));
    }

    #[test]
    fn test_calc_value_usdc_liabilities_maintenance() {
        let (bank, oracle) = setup_usdc_bank_and_oracle();
        let amount = I80F48::from_num(200);
        let result = bank.calc_value(
            &oracle,
            amount,
            BalanceSide::Liabilities,
            RequirementType::Maintenance,
        );
        assert!(result.is_ok());
        let value = result.unwrap();
        assert!(value > I80F48::from_num(0));
    }

    #[test]
    fn test_calc_amount_with_zero_value() {
        let (bank, oracle) = setup_sol_bank_and_oracle();
        let value = I80F48::from_num(0);
        let result = bank.calc_amount(
            &oracle,
            value,
            BalanceSide::Assets,
            RequirementType::Initial,
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), I80F48::from_num(0));
    }

    #[test]
    fn test_calc_value_with_zero_amount() {
        let (bank, oracle) = setup_usdc_bank_and_oracle();
        let amount = I80F48::from_num(0);
        let result = bank.calc_value(
            &oracle,
            amount,
            BalanceSide::Assets,
            RequirementType::Initial,
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), I80F48::from_num(0));
    }
}
