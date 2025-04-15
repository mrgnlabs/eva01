use super::{bank::BankWrapper, marginfi_account::MarginfiAccountWrapper};
use crate::{
    config::GeneralConfig,
    marginfi_ixs::{make_deposit_ix, make_liquidate_ix, make_repay_ix, make_withdraw_ix},
    transaction_manager::{RawTransaction, TransactionData},
};
use crossbeam::channel::Sender;
use futures::executor::block_on;
use log::{debug, error, info};
use marginfi::state::marginfi_account::MarginfiAccount;
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
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::{Arc, RwLock},
    thread,
};
use switchboard_on_demand_client::{
    FetchUpdateManyParams, Gateway, PullFeed, QueueAccountData, SbContext,
};

pub struct LiquidatorAccount {
    pub account_wrapper: MarginfiAccountWrapper,
    pub signer_keypair: Arc<Keypair>,
    program_id: Pubkey,
    token_program_per_mint: HashMap<Pubkey, Pubkey>,
    group: Pubkey,
    pub transaction_tx: Sender<TransactionData>,
    pub swb_gateway: Gateway,
    rpc_client: RpcClient,
    pub non_blocking_rpc_client: NonBlockingRpcClient,
    pub pending_liquidations: Arc<RwLock<HashSet<Pubkey>>>,
    compute_unit_limit: u32,
}

impl LiquidatorAccount {
    pub fn new(
        transaction_tx: Sender<TransactionData>,
        config: &GeneralConfig,
        pending_liquidations: Arc<RwLock<HashSet<Pubkey>>>,
    ) -> anyhow::Result<Self> {
        let signer_keypair = Arc::new(read_keypair_file(&config.keypair_path).unwrap());

        let rpc_client = RpcClient::new(config.rpc_url.clone());
        let account = rpc_client.get_account(&config.liquidator_account)?;
        let marginfi_account = bytemuck::from_bytes::<MarginfiAccount>(&account.data[8..]);
        let account_wrapper = MarginfiAccountWrapper::new(
            config.liquidator_account,
            marginfi_account.lending_account,
        );

        let non_blocking_rpc_client = NonBlockingRpcClient::new(config.rpc_url.clone());

        let queue = block_on(QueueAccountData::load(
            &non_blocking_rpc_client,
            &Pubkey::from_str("A43DyUGA7s8eXPxqEjJY6EBu1KKbNgfxF8h17VAHn13w").unwrap(),
        ))?;
        let swb_gateway = block_on(queue.fetch_gateways(&non_blocking_rpc_client))?[0].clone();

        Ok(Self {
            account_wrapper,
            signer_keypair,
            program_id: config.marginfi_program_id,
            group: marginfi_account.group,
            transaction_tx,
            token_program_per_mint: HashMap::new(),
            swb_gateway,
            rpc_client,
            non_blocking_rpc_client,
            pending_liquidations,
            compute_unit_limit: config.compute_unit_limit,
        })
    }

    pub fn load_initial_data(
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

    pub fn liquidate(
        &mut self,
        liquidatee_account: &MarginfiAccountWrapper,
        asset_bank: &BankWrapper,
        liab_bank: &BankWrapper,
        asset_amount: u64,
        banks: &HashMap<Pubkey, BankWrapper>,
    ) -> anyhow::Result<()> {
        let liquidator_account_address = self.account_wrapper.address;
        let liquidatee_account_address = liquidatee_account.address;
        let signer_pk = self.signer_keypair.pubkey();
        let liab_mint = liab_bank.bank.mint;

        let liquidator_observation_accounts = MarginfiAccountWrapper::get_observation_accounts(
            &self.account_wrapper.lending_account,
            &[liab_bank.address, asset_bank.address],
            &[],
            banks,
        );
        debug!(
            "liquidator_observation_accounts: {:?}",
            liquidator_observation_accounts
        );

        let liquidatee_observation_accounts = MarginfiAccountWrapper::get_observation_accounts(
            &liquidatee_account.lending_account,
            &[],
            &[],
            banks,
        );
        debug!(
            "liquidatee_observation_accounts: {:?}",
            liquidatee_observation_accounts
        );

        let joined_observation_accounts = liquidator_observation_accounts
            .iter()
            .chain(liquidatee_observation_accounts.iter())
            .cloned()
            .collect::<Vec<_>>();

        let observation_swb_oracles = joined_observation_accounts
            .iter()
            .filter_map(|pk| {
                banks.get(pk).and_then(|bank| {
                    if bank.oracle_adapter.is_switchboard_pull() {
                        Some(bank.oracle_adapter.address)
                    } else {
                        None
                    }
                })
            })
            .collect::<Vec<_>>();

        debug!(
            "liquidate: observation_swb_oracles length: {:?}",
            observation_swb_oracles.len()
        );

        let crank_data = if !observation_swb_oracles.is_empty() {
            if let Ok((ix, luts)) = block_on(PullFeed::fetch_update_many_ix(
                SbContext::new(),
                &self.non_blocking_rpc_client,
                FetchUpdateManyParams {
                    feeds: observation_swb_oracles,
                    payer: self.signer_keypair.pubkey(),
                    gateway: self.swb_gateway.clone(),
                    num_signatures: Some(1),
                    ..Default::default()
                },
            )) {
                Some((ix, luts))
            } else {
                return Err(anyhow::anyhow!("Failed to fetch crank data"));
            }
        } else {
            None
        };

        let cu_limit_ix = ComputeBudgetInstruction::set_compute_unit_limit(self.compute_unit_limit);

        let liquidate_ix = make_liquidate_ix(
            self.program_id,
            self.group,
            liquidator_account_address,
            asset_bank,
            liab_bank,
            signer_pk,
            liquidatee_account_address,
            *self.token_program_per_mint.get(&liab_mint).unwrap(),
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

            transactions.push(RawTransaction::new(vec![cu_limit_ix, liquidate_ix]));

            debug!(
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

            debug!(
                "Thread {:?}: cu_limit_ix: ({:?}), liquidate_ix: ({:?})",
                thread::current().id(),
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
                    info!(
                        "Single Transaction sent for address {:?} landed successfully: {:?} ",
                        liquidatee_account.address, res
                    );
                }
                Err(err) => {
                    error!(
                        "Single Transaction sent for address {:?} failed: {:?} ",
                        liquidatee_account.address, err
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
        banks: &HashMap<Pubkey, BankWrapper>,
    ) -> anyhow::Result<()> {
        let marginfi_account = self.account_wrapper.address;

        let signer_pk = self.signer_keypair.pubkey();

        let banks_to_exclude = if withdraw_all.unwrap_or(false) {
            vec![bank.address]
        } else {
            vec![]
        };

        let observation_accounts = MarginfiAccountWrapper::get_observation_accounts(
            &self.account_wrapper.lending_account,
            &[],
            &banks_to_exclude,
            banks,
        );

        let mint = bank.bank.mint;
        let token_program = *self.token_program_per_mint.get(&mint).unwrap();

        let withdraw_ix = make_withdraw_ix(
            self.program_id,
            self.group,
            marginfi_account,
            signer_pk,
            bank,
            token_account,
            token_program,
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

        debug!("Withdrawing {:?} from {:?}", amount, token_account);

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
        debug!(
            "Withdrawing result for account {:?} (without preflight check): {:?} ",
            marginfi_account, res
        );

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
            bank,
            *token_account,
            token_program,
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

        debug!("Repaying {:?}, token account {:?}", amount, token_account);

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
        debug!(
            "Repaying result for account {:?} (without preflight check): {:?} ",
            marginfi_account, res
        );

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
            bank,
            token_account,
            token_program,
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

        debug!("Depositing {:?}, token account {:?}", amount, token_account);

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
        debug!(
            "Depositing result for account {:?} (without preflight check): {:?} ",
            marginfi_account, res
        );

        Ok(())
    }
}
