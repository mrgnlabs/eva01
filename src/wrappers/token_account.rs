use crate::wrappers::oracle::OracleWrapperTrait;

use super::bank::BankWrapper;
use fixed::types::I80F48;
use marginfi::constants::EXP_10_I80F48;

#[derive(Clone)]
pub struct TokenAccountWrapper<T: OracleWrapperTrait> {
    pub balance: u64,
    pub bank_wrapper: BankWrapper,
    pub oracle_wrapper: T,
}

impl<T: OracleWrapperTrait> TokenAccountWrapper<T> {
    pub fn get_amount(&self) -> I80F48 {
        I80F48::from_num(self.balance)
    }

    pub fn get_value(&self) -> anyhow::Result<I80F48> {
        let ui_amount = {
            let amount = I80F48::from_num(self.balance);
            let decimal_scale = EXP_10_I80F48[self.bank_wrapper.bank.mint_decimals as usize];

            amount
                .checked_div(decimal_scale)
                .ok_or(anyhow::anyhow!("Division failed"))?
        };

        let price = self.oracle_wrapper.get_actual_price_of_type(
            marginfi::state::price::OraclePriceType::RealTime,
            None,
            self.bank_wrapper.bank.config.oracle_max_confidence,
        )?;

        Ok(ui_amount * price)
    }
}

#[cfg(test)]
pub mod test_utils {
    use crate::wrappers::{
        bank::test_utils::{test_sol, test_usdc},
        oracle::test_utils::TestOracleWrapper,
    };

    use super::*;

    pub type TestTokenAccountWrapper = TokenAccountWrapper<TestOracleWrapper>;

    impl TestTokenAccountWrapper {
        pub fn test_sol() -> Self {
            Self {
                balance: 10000000,
                bank_wrapper: test_sol(),
                oracle_wrapper: TestOracleWrapper::test_sol(),
            }
        }

        pub fn test_usdc() -> Self {
            Self {
                balance: 100000,
                bank_wrapper: test_usdc(),
                oracle_wrapper: TestOracleWrapper::test_usdc(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_utils::*;
    use super::*;

    #[test]
    fn test_token_account() {
        let sol_acc = TestTokenAccountWrapper::test_sol();
        assert_eq!(sol_acc.get_amount(), I80F48::from_num(10000000.0));
        assert_eq!(sol_acc.get_value().unwrap(), I80F48::from_num(2000.0));

        let usdc_acc: TestTokenAccountWrapper = TestTokenAccountWrapper::test_usdc();
        assert_eq!(usdc_acc.get_amount(), I80F48::from_num(100000.0));
    }
}
