use anyhow::{anyhow, Result};
use solana_account_decoder::UiAccount;
use solana_sdk::{account::Account, pubkey::Pubkey};

pub fn decode_and_apply_simulated_accounts<F>(
    addresses: &[Pubkey],
    simulated_accounts: &[Option<UiAccount>],
    source: &str,
    mut apply: F,
) -> Result<()>
where
    F: FnMut(&Pubkey, Account) -> Result<()>,
{
    if simulated_accounts.len() != addresses.len() {
        return Err(anyhow!(
            "{} returned {} accounts, expected {}",
            source,
            simulated_accounts.len(),
            addresses.len()
        ));
    }

    for (address, ui_account_opt) in addresses.iter().zip(simulated_accounts.iter()) {
        let Some(ui_account) = ui_account_opt else {
            return Err(anyhow!("{} returned null account for {}", source, address));
        };

        let account = ui_account
            .decode::<Account>()
            .ok_or_else(|| anyhow!("Failed to decode simulated account for {}", address))?;

        apply(address, account)?;
    }

    Ok(())
}
