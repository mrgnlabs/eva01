use crate::{
    marginfi_ixs::make_initialize_ix, sender::passive_send_tx, wrappers::marginfi_account::TxConfig,
};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    pubkey::Pubkey,
    signature::Signature,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use std::sync::Arc;

pub fn initialize_marginfi_account(
    rpc_client: Arc<RpcClient>,
    signer: Arc<Keypair>,
    marginfi_program_id: Pubkey,
    marginfi_group_id: Pubkey,
    send_cfg: TxConfig,
) -> anyhow::Result<Signature> {
    let signer_pk = signer.pubkey();

    let initialize_ix = make_initialize_ix(marginfi_program_id, marginfi_group_id, signer_pk);

    let recent_blockhash = rpc_client.get_latest_blockhash()?;

    let mut ixs = vec![initialize_ix];

    if let Some(price) = send_cfg.compute_unit_price_micro_lamports {
        let compute_budget_prixe_ix = ComputeBudgetInstruction::set_compute_unit_price(price);

        ixs.push(compute_budget_prixe_ix);
    }

    let tx = Transaction::new_signed_with_payer(
        &ixs,
        Some(&signer_pk),
        &[signer.as_ref()],
        recent_blockhash,
    );

    let sig = passive_send_tx(rpc_client, &tx)
        .map_err(|e| anyhow::anyhow!("Coulnd't send the transaction: {}", e))?;

    Ok(sig)
}
