use super::oracle::OracleWrapper;
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
    pub oracle_adapter: OracleWrapper,
}

impl BankWrapper {
    pub fn new(address: Pubkey, bank: Bank, oracle_adapter_wrapper: OracleWrapper) -> Self {
        Self {
            address,
            bank,
            oracle_adapter: oracle_adapter_wrapper,
        }
    }

    fn get_pricing_params(
        &self,
        side: BalanceSide,
        requirement_type: RequirementType,
    ) -> (I80F48, Option<PriceBias>, OraclePriceType) {
        match (side, requirement_type) {
            (BalanceSide::Assets, RequirementType::Initial) => (
                self.bank.config.asset_weight_init.into(),
                Some(PriceBias::Low),
                OraclePriceType::TimeWeighted,
            ),
            (BalanceSide::Assets, RequirementType::Maintenance) => (
                self.bank.config.asset_weight_maint.into(),
                Some(PriceBias::Low),
                OraclePriceType::RealTime,
            ),
            (BalanceSide::Liabilities, RequirementType::Initial) => (
                self.bank.config.liability_weight_init.into(),
                Some(PriceBias::High),
                OraclePriceType::TimeWeighted,
            ),
            (BalanceSide::Liabilities, RequirementType::Maintenance) => (
                self.bank.config.liability_weight_maint.into(),
                Some(PriceBias::High),
                OraclePriceType::RealTime,
            ),
            _ => (I80F48::ONE, None, OraclePriceType::RealTime),
        }
    }

    pub fn calc_amount(
        &self,
        value: I80F48,
        side: BalanceSide,
        requirement_type: RequirementType,
    ) -> anyhow::Result<I80F48> {
        let (_, price_bias, oracle_type) = self.get_pricing_params(side, requirement_type);

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
        let (_, price_bias, oracle_type) = self.get_pricing_params(side, requirement_type);

        let price = self
            .oracle_adapter
            .get_price_of_type(oracle_type, price_bias)
            .unwrap();

        Ok(calc_value(amount, price, self.bank.mint_decimals, None)?)
    }
}
