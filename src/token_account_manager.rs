use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use anchor_spl::associated_token;
use log::{debug, error, info};
use rayon::iter::{IndexedParallelIterator, IntoParallelRefIterator, ParallelIterator};
use sha2::{Digest, Sha256};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signature::Keypair,
    signer::{SeedDerivable, Signer},
};

use crate::{
    sender::{SenderCfg, TransactionSender},
    utils::{batch_get_multiple_accounts, BatchLoadingConfig},
};

const TOKEN_ACCOUNT_SEED: &[u8] = b"liquidator_ta";
const MAX_INIT_TA_IXS: usize = 4;

#[derive(Debug, Clone, thiserror::Error)]
pub enum TokenAccountManagerError {
    #[error("Failed to setup token account manager: {0}")]
    SetupFailed(&'static str),
}

#[derive(Clone)]
pub struct TokenAccountManager {
    mint_to_account: Arc<RwLock<HashMap<Pubkey, Pubkey>>>,
    rpc_client: Arc<RpcClient>,
}

impl TokenAccountManager {
    pub fn new(rpc_client: Arc<RpcClient>) -> Result<Self, TokenAccountManagerError> {
        Ok(Self {
            mint_to_account: Arc::new(RwLock::new(HashMap::new())),
            rpc_client,
        })
    }

    pub fn add_mints(
        &self,
        mints: &[Pubkey],
        signer: Pubkey,
    ) -> Result<(), TokenAccountManagerError> {
        let mut mint_to_account = self.mint_to_account.write().unwrap();

        mints.iter().try_for_each(|mint| {
            let address = get_address_for_token_account(signer, *mint, TOKEN_ACCOUNT_SEED)?;

            mint_to_account.insert(*mint, address);

            Ok::<_, TokenAccountManagerError>(())
        })
    }

    pub fn get_mints_and_token_account_addresses(&self) -> (Vec<Pubkey>, Vec<Pubkey>) {
        let mints = self
            .mint_to_account
            .read()
            .unwrap()
            .keys()
            .copied()
            .collect::<Vec<_>>();

        let addresses = mints
            .iter()
            .map(|mint| *self.mint_to_account.read().unwrap().get(mint).unwrap())
            .collect::<Vec<_>>();

        (mints, addresses)
    }

    pub fn create_token_accounts(
        &self,
        signer: Arc<Keypair>,
    ) -> Result<(), TokenAccountManagerError> {
        let mints = self
            .mint_to_account
            .read()
            .unwrap()
            .keys()
            .copied()
            .collect::<Vec<_>>();

        let rpc_client = self.rpc_client.clone();

        let tas = mints
            .iter()
            .map(
                |mint| -> Result<(Pubkey, Pubkey), TokenAccountManagerError> {
                    Ok((
                        *mint,
                        self.get_address_for_mint(*mint).ok_or({
                            TokenAccountManagerError::SetupFailed(
                                "Failed to find token account address",
                            )
                        })?,
                    ))
                },
            )
            .collect::<Result<Vec<_>, _>>()?;

        // Create missing token accounts
        {
            let addresses = tas.iter().map(|(_, address)| *address).collect::<Vec<_>>();

            let res = batch_get_multiple_accounts(
                rpc_client.clone(),
                &addresses,
                BatchLoadingConfig::DEFAULT,
            )
            .map_err(|e| {
                error!("Failed to batch get multiple accounts: {:?}", e);
                TokenAccountManagerError::SetupFailed("Failed to find missing accounts")
            })?;

            let tas_to_create = res
                .iter()
                .zip(tas.iter())
                .filter_map(|(res, (mint, address))| {
                    if res.is_none() {
                        debug!("Creating token account for mint: {:?}", mint);
                        Some((address, mint))
                    } else {
                        None
                    }
                })
                .map(|(_, mint)| -> Result<_, TokenAccountManagerError> {
                    let signer_pk = signer.pubkey();
                    let ix = spl_associated_token_account::instruction::create_associated_token_account_idempotent(&signer_pk, &signer_pk, mint, &spl_token::ID);

                    Ok(ix)
                })
                .collect::<Result<Vec<_>, _>>()?;

            info!("Creating {} token accounts", tas_to_create.len());

            let recent_blockhash = rpc_client.get_latest_blockhash().map_err(|e| {
                error!("Failed to get recent blockhash: {:?}", e);
                TokenAccountManagerError::SetupFailed("Failed to get recent blockhash")
            })?;

            tas_to_create
                .par_iter()
                .chunks(MAX_INIT_TA_IXS)
                .try_for_each(|chunk| {
                    let rpc = rpc_client.clone();

                    let ixs = chunk.iter().map(|ix| (*ix).clone()).collect::<Vec<_>>();
                    let signers = vec![signer.as_ref()];

                    let tx = solana_sdk::transaction::Transaction::new_signed_with_payer(
                        &ixs,
                        Some(&signer.pubkey()),
                        &signers,
                        recent_blockhash,
                    );

                    let sig = TransactionSender::aggressive_send_tx(rpc, &tx, SenderCfg::DEFAULT)
                        .map_err(|e| {
                        error!("Failed to send transaction: {:?}", e);
                        TokenAccountManagerError::SetupFailed("Failed to send transaction")
                    })?;

                    debug!("Token accounts created {:?}", sig);

                    Ok::<_, TokenAccountManagerError>(())
                })?;
        }

        Ok(())
    }

    pub fn get_address_for_mint(&self, mint: Pubkey) -> Option<Pubkey> {
        self.mint_to_account.read().unwrap().get(&mint).copied()
    }
}

fn get_liquidator_seed(signer: Pubkey, mint: Pubkey, seed: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();

    hasher.update(signer.as_ref());
    hasher.update(mint.as_ref());
    hasher.update(seed);

    hasher.finalize().into()
}

fn get_keypair_for_token_account(
    signer: Pubkey,
    mint: Pubkey,
    seed: &[u8],
) -> Result<Keypair, TokenAccountManagerError> {
    let keypair_seed = get_liquidator_seed(signer, mint, seed);
    Keypair::from_seed(&keypair_seed)
        .map_err(|_| TokenAccountManagerError::SetupFailed("Keypair::from_seed failed"))
}

fn get_address_for_token_account(
    signer: Pubkey,
    mint: Pubkey,
    _seed: &[u8],
) -> Result<Pubkey, TokenAccountManagerError> {
    Ok(associated_token::get_associated_token_address(
        &signer, &mint,
    ))
}
