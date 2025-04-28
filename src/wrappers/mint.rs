use solana_sdk::{account::Account, pubkey::Pubkey};

#[derive(Clone)]
pub struct MintWrapper {
    pub address: Pubkey,
    pub token: Pubkey,
    pub account: Account,
}

impl MintWrapper {
    pub fn new(address: Pubkey, token: Pubkey, account: Account) -> Self {
        Self {
            address,
            token,
            account,
        }
    }
}
