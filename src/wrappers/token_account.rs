use super::{bank::BankWrapper, oracle::OracleWrapperTrait};
use fixed::types::I80F48;
use marginfi::constants::EXP_10_I80F48;
use solana_program::pubkey::Pubkey;

pub struct TokenAccountWrapper {
    pub address: Pubkey,
    pub mint: Pubkey,
    pub balance: u64,
    pub mint_decimals: u8,
    pub bank_address: Pubkey,
}

impl TokenAccountWrapper {
    pub fn get_value(&self, bank: &BankWrapper) -> anyhow::Result<I80F48> {
        let ui_amount = {
            let amount = I80F48::from_num(self.balance);
            let decimal_scale = EXP_10_I80F48[self.mint_decimals as usize];

            amount
                .checked_div(decimal_scale)
                .ok_or(anyhow::anyhow!("Division failed"))?
        };

        let price = bank
            .oracle_adapter
            .get_price_of_type(marginfi::state::price::OraclePriceType::RealTime, None)?;

        Ok(ui_amount * price)
    }

    pub fn get_amount(&self) -> I80F48 {
        I80F48::from_num(self.balance)
    }
}
