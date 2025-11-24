use crate::{kamino_farms::program::Farms as KaminoFarms, kamino_lending::program::KaminoLending};
use anchor_lang::Id;
use solana_program::pubkey::Pubkey;

pub const SEED_LENDING_MARKET_AUTH: &str = "lma";
pub const SEED_USER_STATE: &str = "user";

// TODO: expose these from the program side?
pub fn derive_lending_market_authority(lending_market: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[SEED_LENDING_MARKET_AUTH.as_bytes(), lending_market.as_ref()],
        &KaminoLending::id(),
    )
    .0
}

pub fn derive_user_state(farm_state: &Pubkey, obligation: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[
            SEED_USER_STATE.as_bytes(),
            farm_state.as_ref(),
            obligation.as_ref(),
        ],
        &KaminoFarms::id(),
    )
    .0
}
