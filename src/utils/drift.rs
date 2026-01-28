use crate::drift;
use solana_program::pubkey::Pubkey;

const SEED_STATE: &str = "drift_state";
const SEED_SPOT_MARKET: &str = "spot_market";
const SEED_SPOT_MARKET_VAULT: &str = "spot_market_vault";
const SEED_DRIFT_SIGNER: &str = "drift_signer";

pub fn derive_drift_state() -> Pubkey {
    Pubkey::find_program_address(&[SEED_STATE.as_bytes()], &drift::ID).0
}

pub fn derive_spot_market(market_index: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[
            SEED_SPOT_MARKET.as_bytes(),
            market_index.to_le_bytes().as_ref(),
        ],
        &drift::ID,
    )
    .0
}

pub fn derive_spot_market_vault(market_index: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[
            SEED_SPOT_MARKET_VAULT.as_bytes(),
            market_index.to_le_bytes().as_ref(),
        ],
        &drift::ID,
    )
    .0
}

pub fn derive_drift_signer() -> Pubkey {
    Pubkey::find_program_address(&[SEED_DRIFT_SIGNER.as_bytes()], &drift::ID).0
}
