use std::sync::{Arc, RwLock};

use log::{debug, error, info};
use marginfi::state::marginfi_group::BankVaultType;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{pubkey::Pubkey, signature::Keypair, signer::Signer, transaction::Transaction};

use crate::{
    marginfi_ixs::*,
    sender::{aggressive_send_tx, SenderCfg},
    state_engine::{engine::StateEngineService, marginfi_account::MarginfiAccountWrapper},
};

#[derive(thiserror::Error, Debug)]
pub enum MarginfiAccountError {
    #[error("Failed to perform action: {0}")]
    ActionFailed(&'static str),
    #[error("Failed to read marginfi account")]
    RWError,
    #[error("Client error: {0}")]
    RpcClientError(#[from] solana_client::client_error::ClientError),
}

pub struct MarginfiAccount {
    pub account_wrapper: Arc<RwLock<MarginfiAccountWrapper>>,
    state_engine: Arc<StateEngineService>,
    signer_keypair: Arc<Keypair>,
    rpc_client: Arc<RpcClient>,
    program_id: Pubkey,
    token_program: Pubkey,
    group: Pubkey,
}

impl MarginfiAccount {
    pub fn new(
        account_wrapper: Arc<RwLock<MarginfiAccountWrapper>>,
        state_engine: Arc<StateEngineService>,
        signer_keypair: Arc<Keypair>,
        rpc_client: Arc<RpcClient>,
    ) -> Self {
        let program_id = marginfi::id();
        let token_program = spl_token::id();
        let group = account_wrapper.read().unwrap().account.group;

        Self {
            account_wrapper,
            state_engine,
            signer_keypair,
            rpc_client,
            program_id,
            token_program,
            group,
        }
    }

    pub fn deposit(&self, bank_pk: Pubkey, amount: u64) -> Result<(), MarginfiAccountError> {
        info!("Depositing {} into bank {}", amount, bank_pk);
        let bank_ref = self.state_engine.get_bank(&bank_pk).unwrap();
        let bank = bank_ref.read().map_err(|_| MarginfiAccountError::RWError)?;

        let token_account = self
            .state_engine
            .token_account_manager
            .get_address_for_mint(bank.bank.mint)
            .unwrap();

        let marginfi_account = self
            .account_wrapper
            .read()
            .map_err(|_| MarginfiAccountError::RWError)?
            .address;

        let signer_pk = self.signer_keypair.pubkey();

        let deposit_ix = make_deposit_ix(
            self.program_id,
            self.group,
            marginfi_account,
            signer_pk,
            bank_pk,
            token_account,
            bank.bank.liquidity_vault,
            self.token_program,
            amount,
        );

        let recent_blockhash = self.rpc_client.get_latest_blockhash()?;

        let tx = Transaction::new_signed_with_payer(
            &[deposit_ix],
            Some(&signer_pk),
            &[self.signer_keypair.as_ref()],
            recent_blockhash,
        );

        let sig =
            aggressive_send_tx(self.rpc_client.clone(), &tx, SenderCfg::DEFAULT).map_err(|e| {
                info!("Failed to deposit: {:?}", e);
                MarginfiAccountError::ActionFailed("Failed to deposit")
            })?;

        info!("Deposit successful, tx signature: {:?}", sig);

        Ok(())
    }

    pub fn repay(
        &self,
        bank_pk: Pubkey,
        amount: u64,
        repay_all: Option<bool>,
    ) -> anyhow::Result<()> {
        let bank_ref = self.state_engine.get_bank(&bank_pk).unwrap();
        let bank = bank_ref.read().map_err(|_| MarginfiAccountError::RWError)?;

        let token_account = self
            .state_engine
            .token_account_manager
            .get_address_for_mint(bank.bank.mint)
            .unwrap();

        let marginfi_account = self
            .account_wrapper
            .read()
            .map_err(|_| MarginfiAccountError::RWError)?
            .address;
        let signer_pk = self.signer_keypair.pubkey();

        let repay_ix = make_repay_ix(
            self.program_id,
            self.group,
            marginfi_account,
            signer_pk,
            bank_pk,
            token_account,
            bank.bank.liquidity_vault,
            self.token_program,
            amount,
            repay_all,
        );

        let recent_blockhash = self.rpc_client.get_latest_blockhash()?;
        let tx = Transaction::new_signed_with_payer(
            &[repay_ix],
            Some(&signer_pk),
            &[self.signer_keypair.as_ref()],
            recent_blockhash,
        );

        let sig = aggressive_send_tx(self.rpc_client.clone(), &tx, SenderCfg::DEFAULT).unwrap();

        info!("Repay successful, tx signature: {:?}", sig);

        Ok(())
    }

    pub fn withdraw(
        &self,
        bank_pk: &Pubkey,
        amount: u64,
        withdraw_all: Option<bool>,
    ) -> Result<(), MarginfiAccountError> {
        debug!(
            "Withdrawing {} from bank {}, withdraw_all: {:?}",
            amount, bank_pk, withdraw_all
        );
        let bank_ref = self.state_engine.get_bank(bank_pk).unwrap();
        let bank = bank_ref.read().map_err(|_| MarginfiAccountError::RWError)?;

        let token_account = self
            .state_engine
            .token_account_manager
            .get_address_for_mint(bank.bank.mint)
            .unwrap();

        let marginfi_account = self
            .account_wrapper
            .read()
            .map_err(|_| MarginfiAccountError::RWError)?
            .address;
        let signer_pk = self.signer_keypair.pubkey();

        let banks_to_exclude = if withdraw_all.unwrap_or(false) {
            vec![*bank_pk]
        } else {
            vec![]
        };

        let observation_accounts = self
            .account_wrapper
            .read()
            .map_err(|_| MarginfiAccountError::RWError)?
            .get_observation_accounts(&[], &banks_to_exclude);

        let repay_ix = make_withdraw_ix(
            self.program_id,
            self.group,
            marginfi_account,
            signer_pk,
            *bank_pk,
            token_account,
            crate::utils::find_bank_vault_authority_pda(
                &bank_pk,
                BankVaultType::Liquidity,
                &self.program_id,
            )
            .0,
            bank.bank.liquidity_vault,
            self.token_program,
            observation_accounts,
            amount,
            withdraw_all,
        );

        let recent_blockhash = self.rpc_client.get_latest_blockhash()?;

        let tx = Transaction::new_signed_with_payer(
            &[repay_ix],
            Some(&signer_pk),
            &[self.signer_keypair.as_ref()],
            recent_blockhash,
        );

        let sig = aggressive_send_tx(self.rpc_client.clone(), &tx, SenderCfg::DEFAULT).unwrap();

        info!("Repay successful, tx signature: {:?}", sig);

        Ok(())
    }

    pub fn liquidate(
        &self,
        liquidate_account: Arc<RwLock<MarginfiAccountWrapper>>,
        asset_bank_pk: Pubkey,
        liab_bank_pk: Pubkey,
        asset_amount: u64,
    ) -> Result<(), MarginfiAccountError> {
        let asset_bank_ref = self.state_engine.get_bank(&asset_bank_pk).unwrap();
        let asset_bank = asset_bank_ref
            .read()
            .map_err(|_| MarginfiAccountError::RWError)?;

        let liab_bank_ref = self.state_engine.get_bank(&liab_bank_pk).unwrap();
        let liab_bank = liab_bank_ref
            .read()
            .map_err(|_| MarginfiAccountError::RWError)?;

        let liquidator_account_address = self
            .account_wrapper
            .read()
            .map_err(|_| MarginfiAccountError::RWError)?
            .address;
        let liquidatee_account_address = liquidate_account
            .read()
            .map_err(|_| MarginfiAccountError::RWError)?
            .address;

        let signer_pk = self.signer_keypair.pubkey();

        let (bank_liquidity_vault_authority, _) = crate::utils::find_bank_vault_authority_pda(
            &liab_bank_pk,
            BankVaultType::Liquidity,
            &self.program_id,
        );

        let bank_liquidity_vault = liab_bank.bank.liquidity_vault;
        let bank_insurance_vault = liab_bank.bank.insurance_vault;

        let token_program = self.token_program;

        let liquidatee_observation_accounts = liquidate_account
            .read()
            .map_err(|_| MarginfiAccountError::RWError)?
            .get_observation_accounts(&[asset_bank_pk], &[]);

        let liquidator_observation_accounts = self
            .account_wrapper
            .read()
            .map_err(|_| MarginfiAccountError::RWError)?
            .get_observation_accounts(&[liab_bank_pk], &[]);

        let liquidate_ix = make_liquidate_ix(
            self.program_id,
            self.group,
            liquidator_account_address,
            asset_bank_pk,
            liab_bank_pk,
            signer_pk,
            liquidatee_account_address,
            bank_liquidity_vault_authority,
            bank_liquidity_vault,
            bank_insurance_vault,
            token_program,
            liquidatee_observation_accounts,
            liquidator_observation_accounts,
            asset_amount,
        );

        let tx = Transaction::new_signed_with_payer(
            &[liquidate_ix],
            Some(&signer_pk),
            &[self.signer_keypair.as_ref()],
            self.rpc_client.get_latest_blockhash()?,
        );

        let sig =
            aggressive_send_tx(self.rpc_client.clone(), &tx, SenderCfg::DEFAULT).map_err(|e| {
                error!("Failed to liquidate: {:?}", e);
                MarginfiAccountError::ActionFailed("Failed to liquidate")
            })?;

        info!("Liquidation successful, tx signature: {:?}", sig);

        Ok(())
    }
}
