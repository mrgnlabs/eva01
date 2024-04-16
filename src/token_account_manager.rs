use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use log::{debug, error, info};
use rayon::iter::{IndexedParallelIterator, IntoParallelRefIterator, ParallelIterator};
use sha2::{Digest, Sha256};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    program_pack::Pack,
    pubkey::Pubkey,
    signature::Keypair,
    signer::{SeedDerivable, Signer},
    system_instruction,
};

use crate::{
    sender::{aggressive_send_tx, SenderCfg},
    utils::{batch_get_multiple_accounts, BatchLoadingConfig},
};

const TOKEN_ACCOUNT_SEED: &[u8] = b"liquidator_ta";
const MAX_INIT_TA_IXS: usize = 4;

#[derive(Debug, Clone, thiserror::Error)]
pub enum TokenAccountManagerError {
    #[error("Failed to setup token account manager: {0}")]
    SetupFailed(&'static str),
}

pub struct TokenAccountManager {
    mint_to_account: RwLock<HashMap<Pubkey, Pubkey>>,
    rpc_client: Arc<RpcClient>,
}

impl TokenAccountManager {
    pub fn new(rpc_client: Arc<RpcClient>) -> Result<Self, TokenAccountManagerError> {
        Ok(Self {
            mint_to_account: RwLock::new(HashMap::new()),
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

    pub fn get_token_account_addresses(&self) -> Vec<Pubkey> {
        self.mint_to_account
            .read()
            .unwrap()
            .values()
            .copied()
            .collect()
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
                |mint| -> Result<(Keypair, Pubkey, Pubkey), TokenAccountManagerError> {
                    let token_account =
                        get_keypair_for_token_account(signer.pubkey(), *mint, TOKEN_ACCOUNT_SEED)?;

                    let address = token_account.pubkey();

                    Ok((token_account, address, *mint))
                },
            )
            .collect::<Result<Vec<_>, _>>()?;

        // Create missing token accounts
        {
            let addresses = tas
                .iter()
                .map(|(_, address, _)| *address)
                .collect::<Vec<_>>();

            let res = batch_get_multiple_accounts(
                rpc_client.clone(),
                &addresses,
                BatchLoadingConfig::DEFAULT,
            )
            .map_err(|e| {
                error!("Failed to batch get multiple accounts: {:?}", e);
                TokenAccountManagerError::SetupFailed("Failed to find missing accounts")
            })?;

            let rent_lamps = rpc_client
                .get_minimum_balance_for_rent_exemption(spl_token::state::Account::LEN)
                .map_err(|e| {
                    error!("Failed to get minimum balance for rent exemption: {:?}", e);
                    TokenAccountManagerError::SetupFailed(
                        "Failed to get minimum balance for rent exemption",
                    )
                })?;

            let tas_to_create = res
                .iter()
                .zip(tas.iter())
                .filter_map(|(res, (keypair, address, mint))| {
                    if res.is_none() {
                        debug!("Creating token account for mint: {:?}", mint);
                        Some((keypair, address, mint))
                    } else {
                        None
                    }
                })
                .map(
                    |(keypair, address, mint)| -> Result<_, TokenAccountManagerError> {
                        let create_account_ix = system_instruction::create_account(
                            &signer.pubkey(),
                            address,
                            rent_lamps,
                            spl_token::state::Account::LEN as u64,
                            &spl_token::ID,
                        );
                        let init_token_account_ix = spl_token::instruction::initialize_account3(
                            &spl_token::id(),
                            address,
                            mint,
                            &signer.pubkey(),
                        )
                        .map_err(|e| {
                            error!("Failed to create initialize_account instruction: {:?}", e);
                            TokenAccountManagerError::SetupFailed(
                                "Failed to create initialize_account instruction",
                            )
                        })?;

                        Ok((keypair, vec![create_account_ix, init_token_account_ix]))
                    },
                )
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
                    let recent_blockhash = recent_blockhash.clone();
                    let rpc = rpc_client.clone();

                    let ixs = chunk
                        .iter()
                        .map(|(_, ix)| ix.clone())
                        .flatten()
                        .collect::<Vec<_>>();
                    let mut signers = vec![signer.as_ref()];
                    signers.extend(
                        chunk
                            .iter()
                            .map(|(keypair, _)| *keypair)
                            .collect::<Vec<_>>(),
                    );

                    let tx = solana_sdk::transaction::Transaction::new_signed_with_payer(
                        &ixs,
                        Some(&signer.pubkey()),
                        &signers,
                        recent_blockhash,
                    );

                    let sig = aggressive_send_tx(rpc, tx, SenderCfg::DEFAULT).map_err(|e| {
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

    hasher.finalize().try_into().unwrap()
}

fn get_keypair_for_token_account(
    signer: Pubkey,
    mint: Pubkey,
    seed: &[u8],
) -> Result<Keypair, TokenAccountManagerError> {
    let keypair_seed = get_liquidator_seed(signer, mint, seed);
    Ok(Keypair::from_seed(&keypair_seed)
        .map_err(|_| TokenAccountManagerError::SetupFailed("Keypair::from_seed failed"))?)
}

fn get_address_for_token_account(
    signer: Pubkey,
    mint: Pubkey,
    seed: &[u8],
) -> Result<Pubkey, TokenAccountManagerError> {
    let keypair = get_keypair_for_token_account(signer, mint, seed)?;
    Ok(keypair.pubkey())
}
