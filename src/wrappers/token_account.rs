use super::{bank::BankWrapperT, oracle::OracleWrapperTrait};
use fixed::types::I80F48;
use marginfi::constants::EXP_10_I80F48;

#[derive(Clone)]
pub struct TokenAccountWrapperT<T: OracleWrapperTrait> {
    pub balance: u64,
    pub bank: BankWrapperT<T>,
}

impl<T: OracleWrapperTrait> TokenAccountWrapperT<T> {
    pub fn get_value(&self) -> anyhow::Result<I80F48> {
        let ui_amount = {
            let amount = I80F48::from_num(self.balance);
            let decimal_scale = EXP_10_I80F48[self.bank.bank.mint_decimals as usize];

            amount
                .checked_div(decimal_scale)
                .ok_or(anyhow::anyhow!("Division failed"))?
        };

        let price = self
            .bank
            .oracle_adapter
            .get_price_of_type(marginfi::state::price::OraclePriceType::RealTime, None)?;

        Ok(ui_amount * price)
    }

    pub fn get_amount(&self) -> I80F48 {
        I80F48::from_num(self.balance)
    }
}

#[cfg(test)]
pub mod test_utils {
    use crate::wrappers::oracle::test_utils::TestOracleWrapper;

    use super::*;

    pub type TestTokenAccountWrapper = TokenAccountWrapperT<TestOracleWrapper>;

    impl TokenAccountWrapperT<TestOracleWrapper> {
        pub fn test_sol() -> Self {
            Self {
                balance: 10000000,
                bank: BankWrapperT::test_sol(),
            }
        }

        pub fn test_usdc() -> Self {
            Self {
                balance: 100000,
                bank: BankWrapperT::test_usdc(),
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

        let usdc_acc: TokenAccountWrapperT<crate::wrappers::oracle::test_utils::TestOracleWrapper> =
            TestTokenAccountWrapper::test_usdc();
        assert_eq!(usdc_acc.get_amount(), I80F48::from_num(100000.0));
        assert_eq!(usdc_acc.get_value().unwrap(), I80F48::from_num(1000.0));
    }
}
