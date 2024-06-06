use super::{
    bank::BankWrapper,
    marginfi_account::{MarginfiAccountWrapper, TxConfig},
};
use crate::{
    marginfi_ixs::{make_deposit_ix, make_liquidate_ix, make_repay_ix, make_withdraw_ix},
    sender::{SenderCfg, TransactionSender},
};
use log::info;
use marginfi::state::{marginfi_account::MarginfiAccount, marginfi_group::BankVaultType};
use solana_client::rpc_client::RpcClient;
use solana_program::pubkey::Pubkey;
use solana_sdk::{signature::Keypair, signer::Signer};
use std::{collections::HashMap, sync::Arc};

/// Wraps the liquidator account into a dedicated strecture
pub struct LiquidatorAccount {
    pub account_wrapper: MarginfiAccountWrapper,
    pub signer_keypair: Arc<Keypair>,
    rpc_client: Arc<RpcClient>,
    program_id: Pubkey,
    token_program: Pubkey,
    group: Pubkey,
}

impl LiquidatorAccount {
    pub fn new(
        signer_keypair: Keypair,
        rpc_client: RpcClient,
        liquidator_pubkey: Pubkey,
    ) -> anyhow::Result<Self> {
        let account = rpc_client.get_account(&liquidator_pubkey)?;

        let marginfi_account = bytemuck::from_bytes::<MarginfiAccount>(&account.data[8..]);

        let account_wrapper = MarginfiAccountWrapper::new(liquidator_pubkey, *marginfi_account);

        let program_id = marginfi::id();
        let token_program = spl_token::id();
        let group = account_wrapper.account.group;

        Ok(Self {
            account_wrapper,
            signer_keypair: Arc::new(signer_keypair),
            rpc_client: Arc::new(rpc_client),
            program_id,
            token_program,
            group,
        })
    }

    pub fn liquidate(
        &self,
        liquidate_account: &MarginfiAccountWrapper,
        asset_bank: &BankWrapper,
        liab_bank: &BankWrapper,
        asset_amount: u64,
        send_cfg: TxConfig,
        banks: &HashMap<Pubkey, BankWrapper>,
    ) -> anyhow::Result<()> {
        let liquidator_account_address = self.account_wrapper.address;
        let liquidatee_account_address = liquidate_account.address;
        let signer_pk = self.signer_keypair.pubkey();

        let (bank_liquidaity_vault_authority, _) = crate::utils::find_bank_vault_authority_pda(
            &liab_bank.address,
            BankVaultType::Liquidity,
            &self.program_id,
        );

        let bank_liquidaity_vault = liab_bank.bank.liquidity_vault;
        let bank_insurante_vault = liab_bank.bank.insurance_vault;

        let liquidator_observation_accounts = self.account_wrapper.get_observation_accounts(
            &[liab_bank.address, asset_bank.address],
            &[],
            banks,
        );

        let liquidatee_observation_accounts =
            liquidate_account.get_observation_accounts(&[], &[], banks);

        let liquidate_ix = make_liquidate_ix(
            self.program_id,
            self.group,
            liquidator_account_address,
            asset_bank.address,
            liab_bank.address,
            signer_pk,
            liquidatee_account_address,
            bank_liquidaity_vault_authority,
            bank_liquidaity_vault,
            bank_insurante_vault,
            self.token_program,
            liquidator_observation_accounts,
            liquidatee_observation_accounts,
            asset_bank.bank.config.oracle_keys[0],
            liab_bank.bank.config.oracle_keys[0],
            asset_amount,
        );

        let sig = TransactionSender::send_ix(
            self.rpc_client.clone(),
            liquidate_ix,
            self.signer_keypair.clone(),
            Some(send_cfg),
            SenderCfg::DEFAULT,
        )
        .map_err(|e| anyhow::anyhow!("Coulnd't send the transaction {:?}", e))?;

        info!("Liquidation successful, tx signature: {:?}", sig);
        Ok(())
    }

    pub fn withdraw(
        &self,
        bank: &BankWrapper,
        token_account: Pubkey,
        amount: u64,
        withdraw_all: Option<bool>,
        send_cfg: TxConfig,
        banks: &HashMap<Pubkey, BankWrapper>,
    ) -> anyhow::Result<()> {
        let marginfi_account = self.account_wrapper.address;

        let signer_pk = self.signer_keypair.pubkey();

        let banks_to_exclude = if withdraw_all.unwrap_or(false) {
            vec![bank.address]
        } else {
            vec![]
        };

        let observation_accounts =
            self.account_wrapper
                .get_observation_accounts(&[], &banks_to_exclude, &banks);

        let withdraw_ix = make_withdraw_ix(
            self.program_id,
            self.group,
            marginfi_account,
            signer_pk,
            bank.address,
            token_account,
            crate::utils::find_bank_vault_authority_pda(
                &bank.address,
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

        let sig = TransactionSender::send_ix(
            self.rpc_client.clone(),
            withdraw_ix,
            self.signer_keypair.clone(),
            Some(send_cfg),
            SenderCfg::DEFAULT,
        )
        .map_err(|e| anyhow::anyhow!("Coulnd't send the transaction {:?}", e))?;

        info!("Withdraw successful, tx signature: {:?}", sig);

        Ok(())
    }

    pub fn repay(
        &self,
        bank: &BankWrapper,
        token_account: &Pubkey,
        amount: u64,
        repay_all: Option<bool>,
        send_cfg: TxConfig,
    ) -> anyhow::Result<()> {
        let marginfi_account = self.account_wrapper.address;

        let signer_pk = self.signer_keypair.pubkey();

        let repay_ix = make_repay_ix(
            self.token_program,
            self.group,
            marginfi_account,
            signer_pk,
            bank.address,
            *token_account,
            bank.bank.liquidity_vault,
            self.token_program,
            amount,
            repay_all,
        );

        let sig = TransactionSender::send_ix(
            self.rpc_client.clone(),
            repay_ix,
            self.signer_keypair.clone(),
            Some(send_cfg),
            SenderCfg::DEFAULT,
        )
        .map_err(|e| anyhow::anyhow!("Coulnd't send the transaction {:?}", e))?;

        info!("Withdraw successful, tx signature: {:?}", sig);
        Ok(())
    }

    pub fn deposit(
        &self,
        bank: &BankWrapper,
        token_account: Pubkey,
        amount: u64,
        send_cfg: TxConfig,
    ) -> anyhow::Result<()> {
        let marginfi_account = self.account_wrapper.address;

        let signer_pk = self.signer_keypair.pubkey();

        let deposit_ix = make_deposit_ix(
            self.program_id,
            self.group,
            marginfi_account,
            signer_pk,
            bank.address,
            token_account,
            bank.bank.liquidity_vault,
            self.token_program,
            amount,
        );

        let sig = TransactionSender::send_ix(
            self.rpc_client.clone(),
            deposit_ix,
            self.signer_keypair.clone(),
            Some(send_cfg),
            SenderCfg::DEFAULT,
        )
        .map_err(|e| anyhow::anyhow!("Coulnd't send the transaction {:?}", e))?;

        info!("Deposit successful, tx signature: {:?}", sig);

        Ok(())
    }
}
