use super::{bank::BankWrapper, marginfi_account::MarginfiAccountWrapper};
use crate::{
    cache::Cache,
    config::Eva01Config,
    kamino_ixs::{make_kamino_withdraw_ix, make_refresh_obligation_ix, make_refresh_reserve_ix},
    marginfi_ixs::{
        initialize_marginfi_account, make_deposit_ix, make_end_liquidate_ix,
        make_init_liquidation_record_ix, make_repay_ix, make_start_liquidate_ix, make_withdraw_ix,
    },
    metrics::LIQUIDATION_ATTEMPTS,
    utils::{
        self, check_asset_tags_matching, marginfi_account_by_authority,
        swb_cranker::is_stale_swb_price_error,
    },
    wrappers::oracle::OracleWrapper,
};
use anyhow::{anyhow, Context, Result};
use fixed::types::I80F48;
use log::{debug, error, info, warn};
use marginfi_type_crate::types::BalanceSide;
use solana_client::{
    client_error::{ClientError, ClientErrorKind},
    rpc_client::RpcClient,
    rpc_config::RpcSendTransactionConfig,
    rpc_request::RpcError,
};

use solana_program::pubkey::Pubkey;
use solana_sdk::{
    account::ReadableAccount,
    address_lookup_table::AddressLookupTableAccount,
    commitment_config::{CommitmentConfig, CommitmentLevel},
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    message::{v0::Message, CompileError, VersionedMessage},
    pubkey,
    signature::Keypair,
    signer::{Signer, SignerError},
    system_instruction::transfer,
    transaction::VersionedTransaction,
};
use std::{collections::HashSet, sync::Arc, thread, time::Duration};

#[derive(Debug)]
pub enum LiquidationError {
    Anyhow(anyhow::Error),
    StaleOracles(Vec<Pubkey>),
    NotEnoughFunds,
}

impl LiquidationError {
    fn from_anyhow(e: anyhow::Error) -> Self {
        Self::Anyhow(e)
    }

    fn from_compile(e: CompileError) -> Self {
        Self::Anyhow(e.into())
    }

    fn from_signer(e: SignerError) -> Self {
        Self::Anyhow(e.into())
    }

    fn from_client(e: ClientError) -> Self {
        Self::Anyhow(e.into())
    }
}

pub struct PreparedLiquidatableAccount {
    pub liquidatee_account: MarginfiAccountWrapper,
    pub asset_bank: Pubkey,
    pub liab_bank: Pubkey,
    pub asset_amount: I80F48,
    pub liab_amount: I80F48,
    pub profit: u64,
}

pub struct LiquidatorAccount {
    pub liquidator_address: Pubkey,
    pub signer: Keypair,
    program_id: Pubkey,
    group: Pubkey,
    preferred_mint_bank: Pubkey,
    rpc_client: RpcClient,
    cu_limit_ix: Instruction,
    pub cache: Arc<Cache>,
}

impl LiquidatorAccount {
    pub fn new(
        config: &Eva01Config,
        marginfi_group_id: Pubkey,
        preferred_mint: Pubkey,
        cache: Arc<Cache>,
    ) -> Result<Self> {
        let signer = Keypair::from_bytes(&config.wallet_keypair)?;
        let rpc_client =
            RpcClient::new_with_commitment(config.rpc_url.clone(), CommitmentConfig::confirmed());

        let accounts = marginfi_account_by_authority(
            signer.pubkey(),
            &rpc_client,
            config.marginfi_program_id,
            marginfi_group_id,
        )?;
        info!(
            "Found {} MarginFi accounts for the provided signer: {:?}",
            accounts.len(),
            accounts
        );

        let liquidator_address = if accounts.is_empty() {
            info!("No MarginFi account found for the provided signer. Creating it...");
            let liquidator_marginfi_account = initialize_marginfi_account(
                &rpc_client,
                config.marginfi_program_id,
                marginfi_group_id,
                &signer,
            )?;

            while cache
                .marginfi_accounts
                .try_get_account(&liquidator_marginfi_account)
                .is_err()
            {
                info!("Waiting for the new account info to arrive...");
                thread::sleep(Duration::from_secs(5));
            }

            liquidator_marginfi_account
        } else {
            accounts[0]
        };

        let preferred_mint_bank = cache.banks.try_get_account_for_mint(&preferred_mint)?;
        // Kamino test
        //let preferred_mint_bank = Pubkey::from_str_const("52AJuRJJcejMYS9nNDCk1vYmyG1uHSsXoSPkctS3EfhA");

        Ok(Self {
            liquidator_address,
            signer,
            program_id: config.marginfi_program_id,
            group: marginfi_group_id,
            preferred_mint_bank,
            rpc_client,
            cu_limit_ix: ComputeBudgetInstruction::set_compute_unit_limit(
                config.compute_unit_limit,
            ),
            cache,
        })
    }

    pub fn has_funds(&self) -> Result<bool> {
        let account = self
            .cache
            .marginfi_accounts
            .try_get_account(&self.liquidator_address)?;

        let preferred_mint_bank = self.cache.banks.try_get_bank(&self.preferred_mint_bank)?;

        let validation_result =
            account
                .get_balance_for_bank(&preferred_mint_bank)
                .map(|(balance, side)| match side {
                    BalanceSide::Assets => balance > 0,
                    _ => true,
                });

        Ok(validation_result.unwrap_or(false))
    }

    pub fn init_liq_record(&self, liquidatee_account: &MarginfiAccountWrapper) -> Result<()> {
        info!(
            "Initializing liquidation record for account {:?} with liquidator account {:?}.",
            liquidatee_account.address, self.liquidator_address
        );

        let signer_pk = self.signer.pubkey();
        let init_ix =
            make_init_liquidation_record_ix(self.program_id, liquidatee_account.address, signer_pk);

        let recent_blockhash = self
            .rpc_client
            .get_latest_blockhash()
            .map_err(|e| anyhow!(e))?;

        let msg = Message::try_compile(&signer_pk, &[init_ix.clone()], &[], recent_blockhash)
            .map_err(|e| anyhow!(e))?;

        let txn = VersionedTransaction::try_new(VersionedMessage::V0(msg), &[&self.signer])
            .map_err(|e| anyhow!(e))?;

        info!(
            "Sending liquidation tx for the Account {} .",
            liquidatee_account.address
        );
        match self
            .rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &txn,
                CommitmentConfig::confirmed(),
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    preflight_commitment: Some(CommitmentLevel::Processed),
                    ..Default::default()
                },
            ) {
            Ok(signature) => {
                info!(
                    "Liquidation record init tx for the Account {} was confirmed. Signature: {}",
                    liquidatee_account.address, signature,
                );
                Ok(())
            }
            Err(err) => Err(anyhow!(
                "Liquidation record init tx for the Account {} failed: {} ",
                liquidatee_account.address,
                err
            )),
        }
    }

    pub fn liquidate(
        &self,
        account: &PreparedLiquidatableAccount,
        stale_swb_oracles: &HashSet<Pubkey>,
        tokens_in_shortage: &mut HashSet<Pubkey>,
        dust_liab_threshold: I80F48,
    ) -> Result<(), LiquidationError> {
        let PreparedLiquidatableAccount {
            liquidatee_account,
            asset_bank,
            liab_bank,
            asset_amount,
            liab_amount,
            ..
        } = account;

        let mut participating_accounts: HashSet<Pubkey> = HashSet::new();
        let liquidatee_account_address = liquidatee_account.address;
        participating_accounts.insert(liquidatee_account_address);
        let asset_bank_wrapper = self
            .cache
            .banks
            .try_get_bank(asset_bank)
            .map_err(LiquidationError::from_anyhow)?;

        let liab_bank_wrapper = self
            .cache
            .banks
            .try_get_bank(liab_bank)
            .map_err(LiquidationError::from_anyhow)?;

        let signer_pk = self.signer.pubkey();
        let asset_mint = asset_bank_wrapper.bank.mint;
        let liab_mint = liab_bank_wrapper.bank.mint;
        if tokens_in_shortage.contains(&liab_mint) {
            debug!(
                "Skipping liquidation since the liab token is in shortage: {}",
                liab_mint
            );
            return Ok(());
        }

        let liquidator_account = &self
            .cache
            .marginfi_accounts
            .try_get_account(&self.liquidator_address)
            .map_err(LiquidationError::from_anyhow)?;
        participating_accounts.insert(self.liquidator_address);

        let lending_account = &liquidator_account.lending_account;
        for bank_to_validate_against in [&asset_bank_wrapper, &liab_bank_wrapper] {
            if !check_asset_tags_matching(&bank_to_validate_against.bank, lending_account) {
                // This is a precaution to not attempt to liquidate staked collateral positions when liquidator has non-SOL positions open.
                // Expected to happen quite often for now. Later on, we can add a more sophisticated filtering logic on the higher level.
                debug!("Bank {:?} does not match the asset tags of the lending account -> skipping liquidation attempt", bank_to_validate_against.address);
                return Ok(());
            }
        }

        let liab_token_balance =
            I80F48::from_num(self.get_token_balance_for_mint(&liab_mint).unwrap());

        if liab_token_balance < dust_liab_threshold {
            tokens_in_shortage.insert(liab_mint);
            info!("No tokens: {}", liab_mint);
            return Err(LiquidationError::NotEnoughFunds);
        }

        let (asset_amount, liab_amount) = if liab_token_balance < *liab_amount {
            tokens_in_shortage.insert(liab_mint);
            info!(
                "Not enough {} tokens: liquidating for: {} (of {})",
                liab_mint, liab_token_balance, liab_amount
            );
            let proportion = liab_token_balance.checked_div(*liab_amount).unwrap();
            (
                asset_amount.checked_mul(proportion).unwrap(),
                liab_token_balance,
            )
        } else {
            (
                *asset_amount,
                liab_amount.checked_mul(I80F48::from_num(0.91)).unwrap(),
            )
        };

        let banks_to_include: Vec<Pubkey> = vec![];
        let banks_to_exclude: Vec<Pubkey> = vec![];
        let (liquidatee_observation_accounts, liquidatee_swb_oracles, kamino_reserves) =
            MarginfiAccountWrapper::get_observation_accounts::<OracleWrapper>(
                &liquidatee_account.lending_account,
                &banks_to_include,
                &banks_to_exclude,
                self.cache.clone(),
            )
            .map_err(LiquidationError::from_anyhow)?;

        participating_accounts.extend(liquidatee_observation_accounts.iter());

        debug!(
            "The Liquidatee {:?} observation accounts: {:?}",
            liquidatee_account_address, liquidatee_observation_accounts
        );

        if contains_stale_oracles(stale_swb_oracles, &liquidatee_swb_oracles) {
            return Ok(());
        }

        LIQUIDATION_ATTEMPTS.inc();

        if liquidatee_account.liquidation_record == Pubkey::default() {
            // warn!("IGNORING UNINITIALIZED LIQ RECORD");
            // return Ok(());
            self.init_liq_record(liquidatee_account)
                .map_err(LiquidationError::from_anyhow)?;
        }
        participating_accounts.insert(liquidatee_account.liquidation_record);

        let luts: Vec<AddressLookupTableAccount> = self.cache.luts.lock().unwrap().clone();
        let mut ixs = Vec::new();

        ixs.push(self.cu_limit_ix.clone());

        let start_ix = make_start_liquidate_ix(
            self.program_id,
            liquidatee_account_address,
            signer_pk,
            liquidatee_account.liquidation_record,
            liquidatee_observation_accounts.as_ref(),
        );

        participating_accounts.insert(signer_pk);

        for kamino_reserve_address in kamino_reserves {
            let kamino_reserve = self
                .cache
                .kamino_reserves
                .get(&kamino_reserve_address)
                .context(format!(
                    "Couldn't find the data for kamino reserve: {}",
                    kamino_reserve_address
                ))
                .map_err(LiquidationError::from_anyhow)?;

            let refresh_reserve_ix =
                make_refresh_reserve_ix(kamino_reserve_address, kamino_reserve);
            ixs.push(refresh_reserve_ix);
        }

        let asset_mint_wrapper = self
            .cache
            .mints
            .try_get_account(&asset_mint)
            .map_err(LiquidationError::from_anyhow)?;

        let withdraw_ix = if asset_bank_wrapper.bank.kamino_reserve == Pubkey::default() {
            make_withdraw_ix(
                self.program_id,
                self.group,
                liquidatee_account_address,
                signer_pk,
                &asset_bank_wrapper,
                &asset_mint_wrapper,
                liquidatee_observation_accounts.as_ref(),
                asset_amount.to_num(),
                false,
            )
        } else {
            let kamino_reserve = self
                .cache
                .kamino_reserves
                .get(&asset_bank_wrapper.bank.kamino_reserve)
                .context(format!(
                    "Couldn't find the data for kamino reserve: {}",
                    asset_bank_wrapper.bank.kamino_reserve
                ))
                .map_err(LiquidationError::from_anyhow)?;

            let refresh_obligation_ix = make_refresh_obligation_ix(
                asset_bank_wrapper.bank.kamino_obligation,
                kamino_reserve.reserve.lending_market,
                &[asset_bank_wrapper.bank.kamino_reserve],
            );
            ixs.push(refresh_obligation_ix);

            participating_accounts.insert(asset_bank_wrapper.bank.kamino_obligation);
            participating_accounts.insert(kamino_reserve.reserve.lending_market);

            make_kamino_withdraw_ix(
                self.program_id,
                self.group,
                liquidatee_account_address,
                signer_pk,
                &asset_bank_wrapper,
                &asset_mint_wrapper,
                asset_bank_wrapper.bank.kamino_obligation,
                kamino_reserve,
                liquidatee_observation_accounts.as_ref(),
                asset_amount.to_num(),
                false,
            )
        };
        ixs.push(start_ix);
        ixs.push(withdraw_ix);

        participating_accounts.insert(self.group);
        participating_accounts.insert(asset_mint_wrapper.token);
        participating_accounts.insert(asset_mint_wrapper.account.owner);

        let liab_mint_wrapper = self
            .cache
            .mints
            .try_get_account(&liab_mint)
            .map_err(LiquidationError::from_anyhow)?;
        let repay_ix = make_repay_ix(
            self.program_id,
            self.group,
            liquidatee_account_address,
            signer_pk,
            &liab_bank_wrapper,
            &liab_mint_wrapper,
            liab_amount.to_num(),
            false,
        );
        ixs.push(repay_ix);

        participating_accounts.insert(liab_mint_wrapper.token);
        participating_accounts.insert(liab_mint_wrapper.account.owner);

        let end_ix = make_end_liquidate_ix(
            self.program_id,
            liquidatee_account_address,
            signer_pk,
            liquidatee_account.liquidation_record,
            self.cache.global_fee_state_key,
            self.cache.global_fee_wallet,
            liquidatee_observation_accounts.as_ref(),
        );
        ixs.push(end_ix);

        participating_accounts.insert(self.cache.global_fee_state_key);
        participating_accounts.insert(self.cache.global_fee_wallet);

        let recent_blockhash = self
            .rpc_client
            .get_latest_blockhash()
            .map_err(LiquidationError::from_client)?;

        let msg = Message::try_compile(&signer_pk, &ixs, &luts, recent_blockhash)
            .map_err(LiquidationError::from_compile)?;

        let tx = VersionedTransaction::try_new(VersionedMessage::V0(msg), &[&self.signer])
            .map_err(LiquidationError::from_signer)?;

        info!(
            "Liquidating account {:?} with liquidator account {:?}. Amount: {} (liab amount: {})",
            liquidatee_account_address,
            self.liquidator_address,
            asset_amount.to_num::<u64>(),
            liab_amount.to_num::<u64>()
        );

        // TODO: refactor this!
        match self
            .rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &tx,
                CommitmentConfig::confirmed(),
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    preflight_commitment: Some(CommitmentLevel::Processed),
                    ..Default::default()
                },
            ) {
            Ok(signature) => {
                info!(
                    "Liquidation tx for the Account {} was confirmed. Signature: {}",
                    liquidatee_account_address, signature,
                );
                Ok(())
            }
            Err(err) => {
                if is_tx_too_large_client(&err) {
                    warn!("The attempted tx was too large: adding the observation accounts to a LUT and retrying");
                    self.cache
                        .add_addresses_to_lut(
                            &self.rpc_client,
                            &self.signer,
                            participating_accounts,
                        )
                        .map_err(LiquidationError::from_anyhow)?;

                    let luts: Vec<AddressLookupTableAccount> =
                        self.cache.luts.lock().unwrap().clone();

                    let recent_blockhash = self
                        .rpc_client
                        .get_latest_blockhash()
                        .map_err(LiquidationError::from_client)?;

                    let msg = Message::try_compile(&signer_pk, &ixs, &luts, recent_blockhash)
                        .map_err(LiquidationError::from_compile)?;

                    let tx =
                        VersionedTransaction::try_new(VersionedMessage::V0(msg), &[&self.signer])
                            .map_err(LiquidationError::from_signer)?;

                    match self
                        .rpc_client
                        .send_and_confirm_transaction_with_spinner_and_config(
                            &tx,
                            CommitmentConfig::confirmed(),
                            RpcSendTransactionConfig {
                                skip_preflight: false,
                                preflight_commitment: Some(CommitmentLevel::Processed),
                                ..Default::default()
                            },
                        ) {
                        Ok(signature) => {
                            info!(
                                "Liquidation tx for the Account {} was confirmed. Signature: {}",
                                liquidatee_account_address, signature,
                            );
                            Ok(())
                        }
                        Err(err) => {
                            if is_stale_swb_price_error(&err) {
                                // TODO: Should we just crank always?? Also refresh Kamino reserves?
                                Err(LiquidationError::StaleOracles(liquidatee_swb_oracles))
                            } else {
                                Err(LiquidationError::from_client(err))
                            }
                        }
                    }
                } else if is_stale_swb_price_error(&err) {
                    // TODO: Should we just crank always?? Also refresh Kamino reserves?
                    Err(LiquidationError::StaleOracles(liquidatee_swb_oracles))
                } else {
                    Err(LiquidationError::from_client(err))
                }
            }
        }
    }

    pub fn withdraw(&self, bank: &BankWrapper, amount: u64, withdraw_all: bool) -> Result<()> {
        let marginfi_account = self.liquidator_address;

        let signer_pk = self.signer.pubkey();

        let banks_to_include: Vec<Pubkey> = vec![];
        let banks_to_exclude = if withdraw_all {
            vec![bank.address]
        } else {
            vec![]
        };
        debug!("Collecting observation accounts for the account: {:?} with banks_to_include {:?} and banks_to_exclude {:?}", 
        &self.liquidator_address, &banks_to_include, &banks_to_exclude);
        let (observation_accounts, _, _) =
            MarginfiAccountWrapper::get_observation_accounts::<OracleWrapper>(
                &self
                    .cache
                    .marginfi_accounts
                    .try_get_account(&self.liquidator_address)?
                    .lending_account,
                &banks_to_include,
                &banks_to_exclude,
                self.cache.clone(),
            )?;

        let mint_wrapper = self.cache.mints.try_get_account(&bank.bank.mint)?;
        let withdraw_ix = make_withdraw_ix(
            self.program_id,
            self.group,
            marginfi_account,
            signer_pk,
            bank,
            &mint_wrapper,
            observation_accounts.as_ref(),
            amount,
            withdraw_all,
        );

        let ixs: Vec<Instruction> = vec![self.cu_limit_ix.clone(), withdraw_ix];
        let luts: Vec<AddressLookupTableAccount> = self.cache.luts.lock().unwrap().clone();

        let recent_blockhash = self.rpc_client.get_latest_blockhash()?;
        let msg = Message::try_compile(&signer_pk, &ixs, &luts, recent_blockhash)
            .map_err(|e| anyhow::anyhow!(e))?;
        let tx = VersionedTransaction::try_new(VersionedMessage::V0(msg), &[&self.signer])
            .map_err(|e| anyhow::anyhow!(e))?;

        debug!(
            "Withdrawing {:?} unscaled tokens of the Mint {} from the Liquidator account {:?}, Bank {:?}, ",
            amount,
            bank.bank.mint,
            mint_wrapper.token,
            self.preferred_mint_bank
        );

        let res = self
            .rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &tx,
                CommitmentConfig::finalized(),
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    preflight_commitment: Some(CommitmentLevel::Processed),
                    ..Default::default()
                },
            )
            .map_err(|e| anyhow::anyhow!(e))?;

        debug!("Withdrawal tx: {:?} ", res);
        Ok(())
    }

    pub fn deposit(&self, bank: &BankWrapper, amount: u64) -> Result<()> {
        let marginfi_account = self.liquidator_address;

        let signer_pk = self.signer.pubkey();

        let mint = bank.bank.mint;
        let token_account = self.cache.tokens.try_get_token_for_mint(&mint)?;
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
                &[&self.signer],
                recent_blockhash,
            );

        debug!("Depositing {:?}, token account {:?}", amount, token_account);

        let res = self
            .rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &tx,
                CommitmentConfig::finalized(),
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    preflight_commitment: Some(CommitmentLevel::Processed),
                    ..Default::default()
                },
            );
        debug!(
            "Depositing result for account {:?} (without preflight check): {:?} ",
            marginfi_account, res
        );

        Ok(())
    }

    fn get_token_balance_for_mint(&self, mint_address: &Pubkey) -> Option<u64> {
        let token_account_address = self.cache.tokens.get_token_for_mint(mint_address)?;
        match self.cache.tokens.try_get_account(&token_account_address) {
            Ok(account) => match utils::accessor::amount(account.data()) {
                Ok(amount) => Some(amount),
                Err(error) => {
                    error!(
                        "Failed to obtain balance amount for the Token {}: {}",
                        token_account_address, error
                    );
                    None
                }
            },
            Err(error) => {
                error!(
                    "Failed to get the Token account {}: {}",
                    token_account_address, error
                );
                None
            }
        }
    }
}

fn contains_stale_oracles(stale_oracles: &HashSet<Pubkey>, account_oracles: &[Pubkey]) -> bool {
    if let Some(oracle) = account_oracles
        .iter()
        .find(|oracle| stale_oracles.contains(*oracle))
    {
        debug!("Found stale oracle: {}.", oracle);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_contains_stale_oracles_with_stale() {
        let stale_oracle = Pubkey::new_unique();
        let mut stale_oracles = HashSet::new();
        stale_oracles.insert(stale_oracle);
        let account_oracles = vec![Pubkey::new_unique(), stale_oracle, Pubkey::new_unique()];

        assert!(contains_stale_oracles(&stale_oracles, &account_oracles));
    }

    #[test]
    fn test_contains_stale_oracles_without_stale() {
        let stale_oracle = Pubkey::new_unique();
        let mut stale_oracles = HashSet::new();
        stale_oracles.insert(stale_oracle);
        let account_oracles = vec![Pubkey::new_unique(), Pubkey::new_unique()];

        assert!(!contains_stale_oracles(&stale_oracles, &account_oracles));
    }

    #[test]
    fn test_contains_stale_oracles_empty_account_oracles() {
        let stale_oracle = Pubkey::new_unique();
        let mut stale_oracles = HashSet::new();
        stale_oracles.insert(stale_oracle);
        let account_oracles = vec![];

        assert!(!contains_stale_oracles(&stale_oracles, &account_oracles));
    }

    #[test]
    fn test_contains_stale_oracles_empty_stale_oracles() {
        let account_oracles = vec![Pubkey::new_unique()];
        let stale_oracles = HashSet::new();

        assert!(!contains_stale_oracles(&stale_oracles, &account_oracles));
    }

    #[test]
    fn test_contains_stale_oracles_multiple_stale() {
        let stale1 = Pubkey::new_unique();
        let stale2 = Pubkey::new_unique();
        let mut stale_oracles = HashSet::new();
        stale_oracles.insert(stale1);
        stale_oracles.insert(stale2);
        let account_oracles = vec![stale2, Pubkey::new_unique()];

        assert!(contains_stale_oracles(&stale_oracles, &account_oracles));
    }
}

pub fn is_tx_too_large_client(err: &ClientError) -> bool {
    match err.kind() {
        ClientErrorKind::RpcError(rpc) => match rpc {
            RpcError::RpcResponseError { code, message, .. } => {
                *code == -32602 && message.contains("too large")
            }
            RpcError::RpcRequestError(msg) | RpcError::ForUser(msg) => {
                // Some nodes may proxy this as a plain string
                msg.contains("too large")
            }
            _ => false,
        },
        _ => false,
    }
}
