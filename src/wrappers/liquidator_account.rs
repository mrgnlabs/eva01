use super::{bank::BankWrapper, marginfi_account::MarginfiAccountWrapper};
use crate::{
    cache::Cache,
    cli::setup::marginfi_account_by_authority,
    config::GeneralConfig,
    marginfi_ixs::{
        initialize_marginfi_account, make_deposit_ix, make_liquidate_ix, make_repay_ix,
        make_withdraw_ix,
    },
    thread_debug, thread_error, thread_info,
    transaction_manager::{RawTransaction, TransactionData},
};
use anyhow::{anyhow, Result};
use crossbeam::channel::Sender;
use log::info;
use solana_client::{
    nonblocking::rpc_client::RpcClient as NonBlockingRpcClient, rpc_client::RpcClient,
    rpc_config::RpcSendTransactionConfig,
};
use solana_program::pubkey::Pubkey;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    pubkey,
    signature::{read_keypair_file, Keypair},
    signer::Signer,
    system_instruction::transfer,
};
use std::{
    collections::HashSet,
    str::FromStr,
    sync::{Arc, RwLock},
};
use switchboard_on_demand_client::{
    FetchUpdateManyParams, Gateway, PullFeed, QueueAccountData, SbContext,
};
use tokio::runtime::{Builder, Runtime};

pub struct LiquidatorAccount {
    pub liquidator_address: Pubkey,
    pub signer_keypair: Arc<Keypair>,
    program_id: Pubkey,
    group: Pubkey,
    pub transaction_tx: Sender<TransactionData>,
    pub swb_gateway: Gateway,
    rpc_client: RpcClient,
    pub non_blocking_rpc_client: NonBlockingRpcClient,
    pub pending_liquidations: Arc<RwLock<HashSet<Pubkey>>>,
    compute_unit_limit: u32,
    tokio_rt: Runtime,
    cache: Arc<Cache>,
}

impl LiquidatorAccount {
    pub fn new(
        transaction_tx: Sender<TransactionData>,
        config: &GeneralConfig,
        marginfi_group_id: Pubkey,
        pending_liquidations: Arc<RwLock<HashSet<Pubkey>>>,
        cache: Arc<Cache>,
    ) -> Result<Self> {
        let signer_keypair = Arc::new(read_keypair_file(&config.keypair_path).unwrap());
        let rpc_client = RpcClient::new(config.rpc_url.clone());
        let accounts = marginfi_account_by_authority(
            signer_keypair.pubkey(),
            &rpc_client,
            config.marginfi_program_id,
            marginfi_group_id,
        )?;
        let liquidator_address = if accounts.is_empty() {
            info!("No MarginFi account found for the provided signer. Creating it...");

            let liquidator_marginfi_account = initialize_marginfi_account(
                &rpc_client,
                config.marginfi_program_id,
                marginfi_group_id,
                &signer_keypair,
            )?;
            info!(
                "Initialized new MarginFi account for this liquidator {:?}!",
                liquidator_marginfi_account
            );
            liquidator_marginfi_account
        } else {
            accounts[0]
        };

        let non_blocking_rpc_client = NonBlockingRpcClient::new(config.rpc_url.clone());

        let tokio_rt = Builder::new_multi_thread()
            .thread_name("liquidator-account")
            .worker_threads(2)
            .enable_all()
            .build()?;

        let queue = tokio_rt.block_on(QueueAccountData::load(
            &non_blocking_rpc_client,
            &Pubkey::from_str("A43DyUGA7s8eXPxqEjJY6EBu1KKbNgfxF8h17VAHn13w").unwrap(),
        ))?;
        let swb_gateway =
            tokio_rt.block_on(queue.fetch_gateways(&non_blocking_rpc_client))?[0].clone();

        Ok(Self {
            liquidator_address,
            signer_keypair,
            program_id: config.marginfi_program_id,
            group: marginfi_group_id,
            transaction_tx,
            swb_gateway,
            rpc_client,
            non_blocking_rpc_client,
            pending_liquidations,
            compute_unit_limit: config.compute_unit_limit,
            tokio_rt,
            cache,
        })
    }

    pub fn liquidate(
        &mut self,
        liquidatee_account: &MarginfiAccountWrapper,
        asset_bank: &BankWrapper,
        liab_bank: &BankWrapper,
        asset_amount: u64,
    ) -> Result<()> {
        let liquidatee_account_address = liquidatee_account.address;
        thread_info!(
            "Liquidating account {:?} with liquidator account {:?}",
            liquidatee_account_address,
            self.liquidator_address
        );

        let signer_pk = self.signer_keypair.pubkey();
        let liab_mint = liab_bank.bank.mint;

        let banks_to_include: Vec<Pubkey> = vec![liab_bank.address, asset_bank.address];
        let banks_to_exclude: Vec<Pubkey> = vec![];
        thread_debug!("Collecting observation accounts for the account: {:?} with banks_to_include {:?} and banks_to_exclude {:?}", 
        &self.liquidator_address, &banks_to_include, &banks_to_exclude);
        // TODO: verify that we actually need liquidator's swb oracles. Probably liquidatee's are enough.
        let (liquidator_observation_accounts, _) =
            MarginfiAccountWrapper::get_observation_accounts(
                &self
                    .cache
                    .marginfi_accounts
                    .try_get_account(&self.liquidator_address)?
                    .lending_account,
                &banks_to_include,
                &banks_to_exclude,
                self.cache.clone(),
            )?;
        thread_debug!(
            "Liquidator observation accounts: {:?}",
            liquidator_observation_accounts
        );

        let banks_to_include: Vec<Pubkey> = vec![];
        let banks_to_exclude: Vec<Pubkey> = vec![];
        thread_debug!("Collecting observation accounts for the account: {:?} with banks_to_include {:?} and banks_to_exclude {:?}", 
        &self.liquidator_address, &banks_to_include, &banks_to_exclude);
        let (liquidatee_observation_accounts, swb_oracles) =
            MarginfiAccountWrapper::get_observation_accounts(
                &liquidatee_account.lending_account,
                &banks_to_include,
                &banks_to_exclude,
                self.cache.clone(),
            )?;
        thread_debug!(
            "Liquidatee {:?} observation accounts: {:?}",
            liquidatee_account_address,
            liquidatee_observation_accounts
        );

        let joined_observation_accounts = liquidator_observation_accounts
            .iter()
            .chain(liquidatee_observation_accounts.iter())
            .copied()
            .collect::<Vec<_>>();

        let crank_data = if !swb_oracles.is_empty() {
            thread_debug!("Cranking Swb Oracles for observation accounts.",);
            if let Ok((ix, luts)) = self.tokio_rt.block_on(PullFeed::fetch_update_many_ix(
                SbContext::new(),
                &self.non_blocking_rpc_client,
                FetchUpdateManyParams {
                    feeds: swb_oracles,
                    payer: self.signer_keypair.pubkey(),
                    gateway: self.swb_gateway.clone(),
                    num_signatures: Some(1),
                    ..Default::default()
                },
            )) {
                Some((ix, luts))
            } else {
                return Err(anyhow!("Failed to crank/fetch Swb Oracles data."));
            }
        } else {
            None
        };

        let cu_limit_ix = ComputeBudgetInstruction::set_compute_unit_limit(self.compute_unit_limit);

        let liquidate_ix = make_liquidate_ix(
            self.program_id,
            self.group,
            self.liquidator_address,
            asset_bank,
            liab_bank,
            signer_pk,
            liquidatee_account_address,
            self.cache.mints.try_get_account(&liab_mint)?.account.owner,
            joined_observation_accounts,
            asset_amount,
        );

        let recent_blockhash = self.rpc_client.get_latest_blockhash()?;

        if let Some((crank_ix, crank_lut)) = crank_data {
            let mut transactions =
                vec![RawTransaction::new(vec![crank_ix]).with_lookup_tables(crank_lut)];

            // let transaction = VersionedTransaction::try_new(
            //     VersionedMessage::V0(v0::Message::try_compile(
            //         &signer_pk,
            //         &[crank_ix, liquidate_ix.clone()],
            //         &crank_lut,
            //         recent_blockhash,
            //     )?),
            //     &[&self.signer_keypair],
            // )?;

            // let res = self
            //     .non_blocking_rpc_client
            //     .send_and_confirm_transaction_with_spinner_and_config(
            //         &transaction,
            //         CommitmentConfig::confirmed(),
            //         RpcSendTransactionConfig {
            //             skip_preflight: true,
            //             ..Default::default()
            //         },
            //     )
            //     .await;

            transactions.push(RawTransaction::new(vec![liquidate_ix]));

            thread_debug!(
                "SENDING DOUBLE liquidate: bundle length: {:?}",
                transactions.len()
            );
            self.pending_liquidations
                .write()
                .unwrap()
                .insert(liquidatee_account_address);
            self.transaction_tx.send(TransactionData {
                transactions,
                bundle_id: liquidatee_account_address,
            })?;
        } else {
            let tx: solana_sdk::transaction::Transaction =
                solana_sdk::transaction::Transaction::new_signed_with_payer(
                    &[cu_limit_ix.clone(), liquidate_ix.clone()],
                    Some(&signer_pk),
                    &[&self.signer_keypair],
                    recent_blockhash,
                );

            thread_debug!(
                "cu_limit_ix: ({:?}), liquidate_ix: ({:?})",
                cu_limit_ix,
                liquidate_ix
            );

            let res = self
                .rpc_client
                .send_and_confirm_transaction_with_spinner_and_config(
                    &tx,
                    CommitmentConfig::confirmed(),
                    RpcSendTransactionConfig {
                        skip_preflight: true,
                        ..Default::default()
                    },
                );
            match res {
                Ok(res) => {
                    thread_info!(
                        "Single Transaction sent for address {:?} landed successfully: {:?} ",
                        liquidatee_account.address,
                        res
                    );
                }
                Err(err) => {
                    thread_error!(
                        "Single Transaction sent for address {:?} failed: {:?} ",
                        liquidatee_account.address,
                        err
                    );
                }
            }
        }

        Ok(())
    }

    pub fn withdraw(
        &self,
        bank: &BankWrapper,
        token_account: Pubkey,
        amount: u64,
        withdraw_all: Option<bool>,
    ) -> Result<()> {
        let marginfi_account = self.liquidator_address;

        let signer_pk = self.signer_keypair.pubkey();

        let banks_to_include: Vec<Pubkey> = vec![];
        let banks_to_exclude = if withdraw_all.unwrap_or(false) {
            vec![bank.address]
        } else {
            vec![]
        };
        thread_debug!("Collecting observation accounts for the account: {:?} with banks_to_include {:?} and banks_to_exclude {:?}", 
        &self.liquidator_address, &banks_to_include, &banks_to_exclude);
        let (observation_accounts, _) = MarginfiAccountWrapper::get_observation_accounts(
            &self
                .cache
                .marginfi_accounts
                .try_get_account(&self.liquidator_address)?
                .lending_account,
            &banks_to_include,
            &banks_to_exclude,
            self.cache.clone(),
        )?;

        let mint = bank.bank.mint;
        let withdraw_ix = make_withdraw_ix(
            self.program_id,
            self.group,
            marginfi_account,
            signer_pk,
            bank,
            token_account,
            self.cache.mints.try_get_account(&mint)?.account.owner,
            observation_accounts,
            amount,
            withdraw_all,
        );

        let recent_blockhash = self.rpc_client.get_latest_blockhash()?;

        let tx: solana_sdk::transaction::Transaction =
            solana_sdk::transaction::Transaction::new_signed_with_payer(
                &[withdraw_ix.clone()],
                Some(&signer_pk),
                &[&self.signer_keypair],
                recent_blockhash,
            );

        thread_debug!("Withdrawing {:?} from {:?}", amount, token_account);

        let res = self
            .rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &tx,
                CommitmentConfig::confirmed(),
                RpcSendTransactionConfig {
                    skip_preflight: true,
                    ..Default::default()
                },
            );
        thread_debug!(
            "Withdrawing result for account {:?} (without preflight check): {:?} ",
            marginfi_account,
            res
        );

        Ok(())
    }

    pub fn repay(
        &self,
        bank: &BankWrapper,
        token_account: &Pubkey,
        amount: u64,
        repay_all: Option<bool>,
    ) -> Result<()> {
        let marginfi_account = self.liquidator_address;

        let signer_pk = self.signer_keypair.pubkey();

        let mint = bank.bank.mint;

        let repay_ix = make_repay_ix(
            self.program_id,
            self.group,
            marginfi_account,
            signer_pk,
            bank,
            *token_account,
            self.cache.mints.try_get_account(&mint)?.account.owner,
            amount,
            repay_all,
        );

        let recent_blockhash = self.rpc_client.get_latest_blockhash()?;

        let tx: solana_sdk::transaction::Transaction =
            solana_sdk::transaction::Transaction::new_signed_with_payer(
                &[repay_ix.clone()],
                Some(&signer_pk),
                &[&self.signer_keypair],
                recent_blockhash,
            );

        thread_debug!("Repaying {:?}, token account {:?}", amount, token_account);

        let res = self
            .rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &tx,
                CommitmentConfig::confirmed(),
                RpcSendTransactionConfig {
                    skip_preflight: true,
                    ..Default::default()
                },
            );
        thread_debug!(
            "Repaying result for account {:?} (without preflight check): {:?} ",
            marginfi_account,
            res
        );

        Ok(())
    }

    pub fn deposit(&self, bank: &BankWrapper, token_account: Pubkey, amount: u64) -> Result<()> {
        let marginfi_account = self.liquidator_address;

        let signer_pk = self.signer_keypair.pubkey();

        let mint = bank.bank.mint;

        let deposit_ix = make_deposit_ix(
            self.program_id,
            self.group,
            marginfi_account,
            signer_pk,
            bank,
            token_account,
            self.cache.mints.try_get_account(&mint)?.account.owner,
            amount,
        );

        let recent_blockhash = self.rpc_client.get_latest_blockhash()?;

        let instructions: Vec<Instruction> =
            if mint == pubkey!("So11111111111111111111111111111111111111112") {
                vec![transfer(&signer_pk, &token_account, amount), deposit_ix]
            } else {
                vec![deposit_ix]
            };

        let tx: solana_sdk::transaction::Transaction =
            solana_sdk::transaction::Transaction::new_signed_with_payer(
                &instructions,
                Some(&signer_pk),
                &[&self.signer_keypair],
                recent_blockhash,
            );

        thread_debug!("Depositing {:?}, token account {:?}", amount, token_account);

        let res = self
            .rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &tx,
                CommitmentConfig::confirmed(),
                RpcSendTransactionConfig {
                    skip_preflight: true,
                    ..Default::default()
                },
            );
        thread_debug!(
            "Depositing result for account {:?} (without preflight check): {:?} ",
            marginfi_account,
            res
        );

        Ok(())
    }
}
