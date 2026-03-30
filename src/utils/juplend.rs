use crate::{juplend_earn, liquidity};
use anchor_spl::associated_token;
use solana_program::pubkey::Pubkey;

const SEED_LENDING_ADMIN: &str = "lending_admin";
const SEED_LIQUIDITY: &str = "liquidity";
const SEED_RATE_MODEL: &str = "rate_model";

pub fn derive_lending_admin() -> Pubkey {
    Pubkey::find_program_address(&[SEED_LENDING_ADMIN.as_bytes()], &juplend_earn::ID).0
}

pub fn derive_liquidity() -> Pubkey {
    Pubkey::find_program_address(&[SEED_LIQUIDITY.as_bytes()], &liquidity::ID).0
}

pub fn derive_rate_model(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[SEED_RATE_MODEL.as_bytes(), mint.as_ref()], &liquidity::ID).0
}

pub fn derive_liquidity_vault(mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    let liquidity = derive_liquidity();

    associated_token::get_associated_token_address_with_program_id(&liquidity, mint, token_program)
}
