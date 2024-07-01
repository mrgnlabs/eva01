use crate::{
    marginfi_ixs::make_initialize_ix,
    sender::{SenderCfg, TransactionSender},
};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signature::Signature,
    signature::{Keypair, Signer},
};
use std::sync::Arc;

pub fn initialize_marginfi_account(
    rpc_client: Arc<RpcClient>,
    signer: Arc<Keypair>,
    marginfi_program_id: Pubkey,
    marginfi_group_id: Pubkey,
    send_cfg: SenderCfg,
) -> anyhow::Result<Signature> {
    let signer_pk = signer.pubkey();

    let initialize_ix = make_initialize_ix(marginfi_program_id, marginfi_group_id, signer_pk);

    let sig = TransactionSender::send_ix(rpc_client, initialize_ix, signer, None, send_cfg)
        .map_err(|e| anyhow::anyhow!("Coulnd't send the transaction: {}", e))?;

    Ok(sig)
}
