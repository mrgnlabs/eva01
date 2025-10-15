use anchor_lang::{prelude::AccountMeta, Id, InstructionData, ToAccountMetas};

use solana_sdk::{instruction::Instruction, pubkey::Pubkey};

use crate::{
    kamino_lending::{client as kamino, program::KaminoLending},
    utils::KaminoOracleSetup,
};

pub fn make_refresh_reserve_ix(
    reserve: Pubkey,
    lending_market: Pubkey,
    oracle_setup: KaminoOracleSetup,
) -> Instruction {
    let (pyth_oracle, switchboard_price_oracle, switchboard_twap_oracle, scope_prices) =
        match oracle_setup {
            KaminoOracleSetup::Pyth(oracle) => (Some(oracle), None, None, None),
            KaminoOracleSetup::Switchboard(oracle) => (None, Some(oracle), None, None),
            KaminoOracleSetup::SwitchboardTWAP(oracle) => (None, None, Some(oracle), None),
            KaminoOracleSetup::Scope(oracle) => (None, None, None, Some(oracle)),
        };
    let accounts = kamino::accounts::RefreshReserve {
        reserve,
        lending_market,
        pyth_oracle,
        switchboard_price_oracle,
        switchboard_twap_oracle,
        scope_prices,
    }
    .to_account_metas(None);

    Instruction {
        program_id: KaminoLending::id(),
        accounts,
        data: kamino::args::RefreshReserve.data(),
    }
}

pub fn make_refresh_obligation_ix(
    obligation: Pubkey,
    lending_market: Pubkey,
    remaining: &[Pubkey],
) -> Instruction {
    let mut accounts = kamino::accounts::RefreshObligation {
        lending_market,
        obligation,
    }
    .to_account_metas(None);

    accounts.extend(
        remaining
            .iter()
            .map(|a| AccountMeta::new_readonly(*a, false)),
    );

    Instruction {
        program_id: KaminoLending::id(),
        accounts,
        data: kamino::args::RefreshObligation.data(),
    }
}
