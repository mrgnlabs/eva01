use marginfi::state::price::OraclePriceFeedAdapter;
use solana_program::pubkey::Pubkey;

#[derive(Clone)]
pub struct OracleWrapper {
    pub address: Pubkey,
    pub price_adapter: OraclePriceFeedAdapter,
}

impl OracleWrapper {
    pub fn new(address: Pubkey, price_adapter: OraclePriceFeedAdapter) -> Self {
        Self {
            address,
            price_adapter,
        }
    }
}
