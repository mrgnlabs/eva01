use std::collections::HashSet;

use anchor_lang::{Id, InstructionData, ToAccountMetas};

use solana_sdk::{instruction::Instruction, pubkey::Pubkey};

use crate::{
    drift::{client as drift, program::Drift},
    utils::drift::derive_drift_state,
};

pub fn make_refresh_spot_market_ix(
    spot_market: Pubkey,
    spot_market_vault: Pubkey,
    oracle: Pubkey,
    participating_accounts: &mut HashSet<Pubkey>,
) -> Instruction {
    let accounts = drift::accounts::UpdateSpotMarketCumulativeInterest {
        state: derive_drift_state(),
        spot_market,
        oracle,
        spot_market_vault,
    }
    .to_account_metas(None);

    participating_accounts.extend(accounts.iter().map(|a| a.pubkey));

    Instruction {
        program_id: Drift::id(),
        accounts,
        data: drift::args::UpdateSpotMarketCumulativeInterest.data(),
    }
}
