use super::{bank::BankWrapper, marginfi_account::MarginfiAccountWrapper};
use crate::{
    config::GeneralConfig,
    marginfi_ixs::{make_deposit_ix, make_liquidate_ix, make_repay_ix, make_withdraw_ix},
    transaction_manager::{BatchTransactions, RawTransaction},
};
use crossbeam::channel::Sender;
use marginfi::state::{marginfi_account::MarginfiAccount, marginfi_group::BankVaultType};
use solana_client::{
    nonblocking::rpc_client::RpcClient as NonBlockingRpcClient, rpc_client::RpcClient,
};
use solana_program::pubkey::Pubkey;
use solana_sdk::{
    signature::{read_keypair_file, Keypair},
    signer::Signer,
};
use std::{collections::HashMap, str::FromStr, sync::Arc};
use switchboard_on_demand_client::{FetchUpdateManyParams, Gateway, PullFeed, QueueAccountData};

/// Wraps the liquidator account into a dedicated strecture
pub struct LiquidatorAccount {
    pub account_wrapper: MarginfiAccountWrapper,
    pub signer_keypair: Arc<Keypair>,
    program_id: Pubkey,
    token_program_per_mint: HashMap<Pubkey, Pubkey>,
    group: Pubkey,
    pub transaction_tx: Sender<BatchTransactions>,
    pub swb_gateway: Gateway,
    pub non_blocking_rpc_client: NonBlockingRpcClient,
}

impl LiquidatorAccount {
    pub async fn new(
        rpc_client: RpcClient,
        liquidator_pubkey: Pubkey,
        transaction_tx: Sender<BatchTransactions>,
        config: GeneralConfig,
    ) -> anyhow::Result<Self> {
        let signer_keypair = Arc::new(read_keypair_file(&config.keypair_path).unwrap());

        let account = rpc_client.get_account(&liquidator_pubkey)?;
        let marginfi_account = bytemuck::from_bytes::<MarginfiAccount>(&account.data[8..]);
        let account_wrapper = MarginfiAccountWrapper::new(liquidator_pubkey, *marginfi_account);
        let group = account_wrapper.account.group;

        let non_blocking_rpc_client = NonBlockingRpcClient::new(config.rpc_url.clone());

        let queue = QueueAccountData::load(
            &non_blocking_rpc_client,
            &Pubkey::from_str("A43DyUGA7s8eXPxqEjJY6EBu1KKbNgfxF8h17VAHn13w").unwrap(),
        )
        .await
        .unwrap();
        let swb_gateway = queue
            .fetch_gateways(&non_blocking_rpc_client)
            .await
            .unwrap()[0]
            .clone();

        Ok(Self {
            account_wrapper,
            signer_keypair,
            program_id: config.marginfi_program_id,
            group,
            transaction_tx,
            token_program_per_mint: HashMap::new(),
            swb_gateway,
            non_blocking_rpc_client,
        })
    }

    pub async fn load_initial_data(
        &mut self,
        rpc_client: &RpcClient,
        mints: Vec<Pubkey>,
    ) -> anyhow::Result<()> {
        let token_program_per_mint = rpc_client
            .get_multiple_accounts(&mints)
            .unwrap()
            .iter()
            .zip(mints)
            .map(|(account, mint)| (mint, account.as_ref().unwrap().owner))
            .collect();

        self.token_program_per_mint = token_program_per_mint;

        Ok(())
    }

    pub async fn liquidate(
        &mut self,
        liquidate_account: &MarginfiAccountWrapper,
        asset_bank: &BankWrapper,
        liab_bank: &BankWrapper,
        asset_amount: u64,
        banks: &HashMap<Pubkey, BankWrapper>,
    ) -> anyhow::Result<()> {
        let liquidator_account_address = self.account_wrapper.address;
        let liquidatee_account_address = liquidate_account.address;
        let signer_pk = self.signer_keypair.pubkey();
        let liab_mint = liab_bank.bank.mint;

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

        let joined_observation_accounts = liquidator_observation_accounts
            .iter()
            .chain(liquidatee_observation_accounts.iter())
            .cloned()
            .collect::<Vec<_>>();

        let observation_swb_oracles = joined_observation_accounts
            .iter()
            .filter_map(|&pk| {
                banks.get(&pk).and_then(|bank| {
                    if bank.oracle_adapter.is_switchboard_pull() {
                        Some(bank.oracle_adapter.address)
                    } else {
                        None
                    }
                })
            })
            .collect::<Vec<_>>();

        let crank_data = if !observation_swb_oracles.is_empty() {
            if let Ok((ix, luts)) = PullFeed::fetch_update_many_ix(
                &self.non_blocking_rpc_client,
                FetchUpdateManyParams {
                    feeds: observation_swb_oracles,
                    payer: self.signer_keypair.pubkey(),
                    gateway: self.swb_gateway.clone(),
                    num_signatures: Some(1),
                    ..Default::default()
                },
            )
            .await
            {
                Some((ix, luts))
            } else {
                return Err(anyhow::anyhow!("Failed to fetch crank data"));
            }
        } else {
            None
        };

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
            *self.token_program_per_mint.get(&liab_mint).unwrap(),
            liquidator_observation_accounts,
            liquidatee_observation_accounts,
            asset_bank.oracle_adapter.address,
            liab_bank.oracle_adapter.address,
            liab_mint,
            asset_amount,
        );

        let mut bundle = vec![];
        if let Some((crank_ix, crank_lut)) = crank_data {
            bundle.push(RawTransaction::new(vec![crank_ix]).with_lookup_tables(crank_lut));
        }

        let recent_blockhash = self.non_blocking_rpc_client.get_latest_blockhash().await.unwrap();

        let tx = solana_sdk::transaction::Transaction::new_signed_with_payer(
            &[liquidate_ix.clone()],
            Some(&signer_pk),
            &[&self.signer_keypair],
            recent_blockhash,
        );

        let res = self.non_blocking_rpc_client.simulate_transaction(&tx).await;
        println!("Simulated transaction {:?}", res);

        bundle.push(RawTransaction::new(vec![liquidate_ix]));

        self.transaction_tx.send(bundle)?;

        Ok(())
    }

    pub fn withdraw(
        &self,
        bank: &BankWrapper,
        token_account: Pubkey,
        amount: u64,
        withdraw_all: Option<bool>,
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
                .get_observation_accounts(&[], &banks_to_exclude, banks);

        let mint = bank.bank.mint;
        let token_program = *self.token_program_per_mint.get(&mint).unwrap();

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
            token_program,
            observation_accounts,
            mint,
            amount,
            withdraw_all,
        );

        self.transaction_tx
            .send(vec![RawTransaction::new(vec![withdraw_ix])])?;

        Ok(())
    }

    pub fn repay(
        &self,
        bank: &BankWrapper,
        token_account: &Pubkey,
        amount: u64,
        repay_all: Option<bool>,
    ) -> anyhow::Result<()> {
        let marginfi_account = self.account_wrapper.address;

        let signer_pk = self.signer_keypair.pubkey();

        let mint = bank.bank.mint;
        let token_program = *self.token_program_per_mint.get(&mint).unwrap();

        let repay_ix = make_repay_ix(
            self.program_id,
            self.group,
            marginfi_account,
            signer_pk,
            bank.address,
            *token_account,
            bank.bank.liquidity_vault,
            token_program,
            mint,
            amount,
            repay_all,
        );

        self.transaction_tx
            .send(vec![RawTransaction::new(vec![repay_ix])])?;

        Ok(())
    }

    pub fn deposit(
        &self,
        bank: &BankWrapper,
        token_account: Pubkey,
        amount: u64,
    ) -> anyhow::Result<()> {
        let marginfi_account = self.account_wrapper.address;

        let signer_pk = self.signer_keypair.pubkey();

        let mint = bank.bank.mint;
        let token_program = *self.token_program_per_mint.get(&mint).unwrap();

        let deposit_ix = make_deposit_ix(
            self.program_id,
            self.group,
            marginfi_account,
            signer_pk,
            bank.address,
            token_account,
            bank.bank.liquidity_vault,
            token_program,
            mint,
            amount,
        );

        self.transaction_tx
            .send(vec![RawTransaction::new(vec![deposit_ix])])?;

        Ok(())
    }
}
