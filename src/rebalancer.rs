use crate::{
    cache::Cache,
    config::{GeneralConfig, RebalancerCfg},
    metrics::ERROR_COUNT,
    sender::{SenderCfg, TransactionSender},
    thread_debug, thread_error, thread_info,
    transaction_manager::{RawTransaction, TransactionData},
    utils::{
        self, calc_weighted_assets_new, calc_weighted_liabs_new, swb_cranker::SwbCranker,
        BankAccountWithPriceFeedEva,
    },
    wrappers::{
        liquidator_account::LiquidatorAccount,
        marginfi_account::MarginfiAccountWrapper,
        oracle::{OracleWrapper, OracleWrapperTrait},
    },
};
use anyhow::{anyhow, Result};
use crossbeam::channel::Sender;
use fixed::types::I80F48;
use fixed_macro::types::I80F48;
use jupiter_swap_api_client::{
    quote::QuoteRequest,
    swap::SwapRequest,
    transaction_config::{ComputeUnitPriceMicroLamports, TransactionConfig},
    JupiterSwapApiClient,
};
use log::{error, info};
use marginfi::{
    constants::EXP_10_I80F48,
    state::{
        marginfi_account::{BalanceSide, RequirementType},
        price::PriceBias,
    },
};
use solana_client::{
    nonblocking::rpc_client::RpcClient as NonBlockingRpcClient, rpc_client::RpcClient,
};
use solana_program::pubkey::Pubkey;
use solana_sdk::{
    account::ReadableAccount, signature::read_keypair_file, transaction::VersionedTransaction,
};
use std::{
    cmp::min,
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
    thread,
    time::Duration,
};
use switchboard_on_demand_client::{FetchUpdateManyParams, PullFeed, SbContext};
use tokio::runtime::{Builder, Runtime};

/// The rebalancer is responsible to keep the liquidator account
/// "rebalanced" -> Document this better
pub struct Rebalancer {
    config: RebalancerCfg,
    general_config: GeneralConfig,
    liquidator_account: LiquidatorAccount,
    non_blocking_rpc_client: NonBlockingRpcClient,
    txn_client: Arc<RpcClient>,
    preferred_mints: HashSet<Pubkey>,
    //FIXME: consider remove the Option wrapper. Rebalancing will not work w/o the swap mint bank.
    swap_mint_bank_pk: Pubkey,
    run_rebalance: Arc<AtomicBool>,
    stop_liquidator: Arc<AtomicBool>,
    tokio_rt: Runtime,
    cache: Arc<Cache<OracleWrapper>>,
    swb_price_simulator: Arc<SwbCranker>,
}

impl Rebalancer {
    pub fn new(
        general_config: GeneralConfig,
        config: RebalancerCfg,
        transaction_tx: Sender<TransactionData>,
        pending_bundles: Arc<RwLock<HashSet<Pubkey>>>,
        run_rebalance: Arc<AtomicBool>,
        stop_liquidator: Arc<AtomicBool>,
        cache: Arc<Cache<OracleWrapper>>,
        swb_price_simulator: Arc<SwbCranker>,
    ) -> anyhow::Result<Self> {
        let txn_client = Arc::new(RpcClient::new(general_config.tx_landing_url.clone()));
        let non_blocking_rpc_client = NonBlockingRpcClient::new(general_config.rpc_url.clone());

        let liquidator_account = LiquidatorAccount::new(
            transaction_tx.clone(),
            &general_config,
            pending_bundles,
            cache.clone(),
        )?;

        let preferred_mints = config.preferred_mints.iter().cloned().collect();

        let swap_mint_bank_pk = cache.banks.try_get_account_for_mint(&config.swap_mint)?;

        let tokio_rt = Builder::new_multi_thread()
            .thread_name("rebalancer")
            .worker_threads(2)
            .enable_all()
            .build()?;

        Ok(Self {
            config,
            general_config,
            liquidator_account,
            non_blocking_rpc_client,
            txn_client,
            preferred_mints,
            swap_mint_bank_pk,
            run_rebalance,
            stop_liquidator,
            tokio_rt,
            cache,
            swb_price_simulator,
        })
    }

    pub fn start(&mut self) -> anyhow::Result<()> {
        info!("Starting the Rebalancer loop");
        while !self.stop_liquidator.load(Ordering::Relaxed) {
            if self.run_rebalance.load(Ordering::Relaxed) && self.needs_to_be_rebalanced()? {
                thread_debug!("Running the Rebalancing process...");
                self.run_rebalance.store(false, Ordering::Relaxed);

                if let Err(e) = self.rebalance_accounts() {
                    thread_error!("Failed to rebalance account: {:?}", e);
                    ERROR_COUNT.inc();
                }
                thread_debug!("The Rebalancing process is complete.");
            } else {
                thread::sleep(Duration::from_millis(1000))
            }
        }
        thread_info!("The Rebalancer loop stopped.");

        Ok(())
    }

    fn needs_to_be_rebalanced(&mut self) -> Result<bool> {
        self.should_stop_liquidator()?;

        Ok(self.has_tokens_in_token_accounts()?
            || self.has_non_preferred_deposits()?
            || self.has_liabilities()?)
    }

    // TODO: move to SwbCranker
    fn fetch_swb_prices(&self) -> anyhow::Result<()> {
        let active_banks = MarginfiAccountWrapper::get_active_banks(
            &self
                .cache
                .marginfi_accounts
                .try_get_account(&self.liquidator_account.liquidator_address)?
                .lending_account,
        );

        let active_swb_oracles: Vec<Pubkey> = active_banks
            .iter()
            .filter_map(|&bank_pk| {
                self.cache.get_bank_wrapper(&bank_pk).and_then(|bank| {
                    if bank.oracle_adapter.is_switchboard_pull() {
                        Some(bank.oracle_adapter.address)
                    } else {
                        None
                    }
                })
            })
            .collect();

        if !active_swb_oracles.is_empty() {
            thread_debug!("Fetching SWB prices.");
            let (ix, lut) = self.tokio_rt.block_on(PullFeed::fetch_update_many_ix(
                SbContext::new(),
                &self.non_blocking_rpc_client,
                FetchUpdateManyParams {
                    feeds: active_swb_oracles,
                    payer: self.general_config.signer_pubkey,
                    gateway: self.liquidator_account.swb_gateway.clone(),
                    num_signatures: Some(1),
                    ..Default::default()
                },
            ))?;
            self.liquidator_account
                .transaction_tx
                .send(TransactionData {
                    transactions: vec![RawTransaction::new(vec![ix]).with_lookup_tables(lut)],
                    bundle_id: self.liquidator_account.liquidator_address,
                })?;
        }

        Ok(())
    }

    fn rebalance_accounts(&mut self) -> anyhow::Result<()> {
        self.swb_price_simulator.simulate_swb_prices()?;
        //TODO: It is called right after simulation. Confirm that it is really needed.
        self.fetch_swb_prices()?;

        self.sell_non_preferred_deposits()?;

        self.repay_liabilities()?;

        self.drain_tokens_from_token_accounts()?;

        self.deposit_preferred_tokens()?;

        Ok(())
    }

    // If our margin is at 50% or lower, we should stop the Liquidator and manually adjust it's balances.
    pub fn should_stop_liquidator(&self) -> Result<()> {
        let (assets, liabs) = self.calc_health(
            &self
                .cache
                .marginfi_accounts
                .try_get_account(&self.liquidator_account.liquidator_address)?,
            RequirementType::Initial,
        );

        if assets.is_zero() {
            error!(
                "The Liquidator {:?} has no assets!",
                self.liquidator_account.liquidator_address
            );

            self.stop_liquidator
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }

        let ratio = (assets - liabs) / assets;
        if ratio <= 0.5 {
            error!(
                "The Assets ({}) to Liabilities ({}) ratio ({}) too low for the Liquidator {:?}!",
                assets, liabs, ratio, self.liquidator_account.liquidator_address
            );

            self.stop_liquidator
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }

        Ok(())
    }

    fn sell_non_preferred_deposits(&mut self) -> Result<()> {
        let non_preferred_deposits = self
            .cache
            .marginfi_accounts
            .try_get_account(&self.liquidator_account.liquidator_address)?
            .get_deposits(&self.config.preferred_mints, self.cache.clone());

        if !non_preferred_deposits.is_empty() {
            thread_debug!("Selling non-preferred deposits.");

            for bank_pk in non_preferred_deposits {
                if let Err(error) = self.withdraw_and_sell_deposit(&bank_pk) {
                    error!(
                        "Failed to withdraw and sell deposit for the Bank ({}): {:?}",
                        bank_pk, error
                    );
                }
            }
        }

        Ok(())
    }

    fn repay_liabilities(&mut self) -> Result<()> {
        let liabilities = self
            .cache
            .marginfi_accounts
            .try_get_account(&self.liquidator_account.liquidator_address)?
            .get_liabilities_shares();

        if !liabilities.is_empty() {
            thread_debug!("Repaying liabilities.");
            for (_, bank_pk) in liabilities {
                if let Err(err) = self.repay_liability(bank_pk) {
                    error!(
                        "Failed to repay liability for the Bank ({}): {:?}",
                        bank_pk, err
                    );
                }
            }
        }

        Ok(())
    }

    /// Repay a liability for a given bank
    ///
    /// - Find any bank tokens in token accounts
    /// - Calc $ value of liab
    /// - Find USDC account
    /// - Calc additional USDC to withdraw
    /// - Withdraw USDC
    /// - Swap USDC for bank tokens
    /// - Repay liability
    fn repay_liability(&mut self, bank_pk: Pubkey) -> anyhow::Result<()> {
        let bank = self.cache.try_get_bank_wrapper(&bank_pk)?;

        thread_debug!("Evaluating the {:?} Bank liability.", &bank_pk);

        // Get the balance for the liability bank and check if it's valid
        let liab_balance_opt = self
            .cache
            .marginfi_accounts
            .try_get_account(&self.liquidator_account.liquidator_address)?
            .get_balance_for_bank(&bank);

        if liab_balance_opt.is_none() || matches!(liab_balance_opt, Some((_, BalanceSide::Assets)))
        {
            return Ok(());
        }

        let (liab_balance, _) = liab_balance_opt.unwrap();

        // Gets how much tokens of needing repay asset to purchase
        let token_balance = self
            .get_token_balance_for_bank(&bank_pk)
            .unwrap_or_default();

        let liab_to_purchase = liab_balance - token_balance;

        if liab_to_purchase.is_zero() {
            return Ok(());
        }

        let liab_usd_value = self.get_value(
            liab_to_purchase,
            &bank_pk,
            RequirementType::Initial,
            BalanceSide::Liabilities,
        )?;
        if liab_usd_value < self.config.token_account_dust_threshold {
            thread_debug!(
                "The {:?} unscaled Liability tokens of Bank {:?} are below the dust threshold {}.",
                liab_to_purchase,
                bank_pk,
                self.config.token_account_dust_threshold
            );
            return Ok(());
        }

        thread_debug!(
            "The {:?} unscaled Liability tokens of Bank {:?} need to be repaid with {:?} USD.",
            liab_to_purchase,
            bank_pk,
            liab_usd_value
        );

        // Get the amount of USDC needed to repay the liability

        let required_swap_token = self.get_amount(liab_usd_value, &self.swap_mint_bank_pk, None)?;

        let swap_token_balance = self
            .get_token_balance_for_bank(&self.swap_mint_bank_pk)
            .unwrap_or_default();

        let token_balance_to_withdraw = required_swap_token - swap_token_balance;

        let withdraw_amount = if token_balance_to_withdraw.is_positive() {
            let (max_withdraw_amount, withdraw_all) =
                self.get_max_withdraw_for_bank(&self.swap_mint_bank_pk)?;

            let withdraw_amount = min(max_withdraw_amount, token_balance_to_withdraw);

            let bank = self.cache.try_get_bank_wrapper(&self.swap_mint_bank_pk)?;

            self.liquidator_account.withdraw(
                &bank,
                self.cache.tokens.try_get_token_for_mint(&bank.bank.mint)?,
                withdraw_amount.to_num(),
                Some(withdraw_all),
            )?;

            withdraw_amount
        } else {
            I80F48::ZERO
        };

        let amount_to_swap = min(liab_balance + withdraw_amount, required_swap_token);
        if amount_to_swap.is_positive() {
            thread_debug!(
                "Repaying {:?} unscaled Liability tokens of the Bank {:?} from the Bank {:?}",
                amount_to_swap,
                bank_pk,
                self.swap_mint_bank_pk,
            );
            self.swap(amount_to_swap.to_num(), &self.swap_mint_bank_pk, &bank_pk)?;
        }

        thread_debug!("Repaying liability for bank {}", bank_pk);

        let token_balance = self
            .get_token_balance_for_bank(&bank_pk)
            .unwrap_or_default();

        let repay_all = token_balance >= liab_balance;

        let bank = self.cache.try_get_bank_wrapper(&bank_pk)?;
        let token_account = self.cache.tokens.try_get_token_for_mint(&bank.bank.mint)?;

        self.liquidator_account.repay(
            &bank,
            &token_account,
            token_balance.to_num(),
            Some(repay_all),
        )?;

        Ok(())
    }

    fn deposit_preferred_tokens(&self) -> anyhow::Result<()> {
        if let Some(balance) = self.get_token_balance_for_bank(&self.swap_mint_bank_pk) {
            if !balance.is_zero() {
                thread_debug!(
                    "Depositing {} of preferred tokens to the Swap mint bank {:?}.",
                    balance,
                    &self.swap_mint_bank_pk
                );

                let bank_wrapper = self.cache.try_get_bank_wrapper(&self.swap_mint_bank_pk)?;
                let token_address = self
                    .cache
                    .tokens
                    .try_get_token_for_mint(&bank_wrapper.bank.mint)?;

                if let Err(error) =
                    self.liquidator_account
                        .deposit(&bank_wrapper, token_address, balance.to_num())
                {
                    error!(
                        "Failed to deposit to the Bank ({:?}): {:?}",
                        &self.swap_mint_bank_pk, error
                    );
                }
            }
        }

        Ok(())
    }

    fn has_tokens_in_token_accounts(&self) -> Result<bool> {
        let mut index: usize = 0;
        let len = self.cache.tokens.len()?;
        while index < len {
            if let Some(token_address) = self.cache.tokens.get_address_by_index(index) {
                match self.cache.try_get_token_wrapper(&token_address) {
                    Ok(wrapper) => match wrapper.get_value() {
                        Ok(value) => {
                            if value > self.config.token_account_dust_threshold {
                                return Ok(true);
                            }
                        }
                        Err(error) => error!("Failed compute token value! {}", error),
                    },
                    Err(error) => error!("Failed obtain token data! {}", error),
                }
            }
            index += 1;
        }

        Ok(false)
    }

    fn has_non_preferred_deposits(&self) -> Result<bool> {
        Ok(self
            .cache
            .marginfi_accounts
            .try_get_account(&self.liquidator_account.liquidator_address)?
            .lending_account
            .balances
            .iter()
            .filter(|balance| balance.is_active())
            .any(|balance| {
                self.cache
                    .banks
                    .get_account(&balance.bank_pk)
                    .is_some_and(|bank| {
                        matches!(balance.get_side(), Some(BalanceSide::Assets))
                            && !self.preferred_mints.contains(&bank.mint)
                    })
            }))
    }

    fn has_liabilities(&self) -> Result<bool> {
        Ok(self
            .cache
            .marginfi_accounts
            .try_get_account(&self.liquidator_account.liquidator_address)?
            .has_liabs())
    }

    fn drain_tokens_from_token_accounts(&mut self) -> anyhow::Result<()> {
        let mut index: usize = 0;
        let len = self.cache.tokens.len()?;
        while index < len {
            if let Some(token_address) = self.cache.tokens.get_address_by_index(index) {
                let account = self.cache.try_get_token_wrapper(&token_address)?;
                if account.bank.bank.mint == self.config.swap_mint {
                    continue;
                }

                let value = account.get_value()?;
                if value > self.config.token_account_dust_threshold {
                    self.swap(
                        account.get_amount().to_num(),
                        &account.bank.address,
                        &self.swap_mint_bank_pk,
                    )?;
                } else {
                    thread_debug!(
                        "The {:?} unscaled Drain tokens of Bank {:?} are below the dust threshold {}.",
                        account.get_amount(),
                        &account.bank.address,
                        self.config.token_account_dust_threshold
                    );
                }
            }
            index += 1;
        }

        Ok(())
    }

    /// Withdraw and sells a given asset
    fn withdraw_and_sell_deposit(&mut self, bank_pk: &Pubkey) -> anyhow::Result<()> {
        let bank = self.cache.try_get_bank_wrapper(bank_pk)?;
        let balance = self
            .cache
            .marginfi_accounts
            .try_get_account(&self.liquidator_account.liquidator_address)?
            .get_balance_for_bank(&bank);

        if !matches!(&balance, Some((_, BalanceSide::Assets))) {
            return Ok(());
        }

        let (withdraw_amount, withdrawl_all) = self.get_max_withdraw_for_bank(bank_pk)?;

        let amount = withdraw_amount.to_num::<u64>();

        thread_debug!(
            "Withdrawing {:?} of {:?} from bank {:?}",
            amount,
            bank.bank.mint,
            bank_pk
        );
        self.liquidator_account.withdraw(
            &bank,
            self.cache.tokens.try_get_token_for_mint(&bank.bank.mint)?,
            amount,
            Some(withdrawl_all),
        )?;

        self.swap(amount, bank_pk, &self.swap_mint_bank_pk)?;

        Ok(())
    }

    fn swap(&self, amount: u64, src_bank: &Pubkey, dst_bank: &Pubkey) -> anyhow::Result<()> {
        thread_debug!(
            "Swapping {} unscaled tokens from {} to {}.",
            amount,
            src_bank,
            dst_bank
        );

        let input_mint = self.cache.banks.try_get_account(src_bank)?.mint;
        let output_mint = self.cache.banks.try_get_account(dst_bank)?.mint;

        let jup_swap_client = JupiterSwapApiClient::new(self.config.jup_swap_api_url.clone());

        let quote_response = self
            .tokio_rt
            .block_on(jup_swap_client.quote(&QuoteRequest {
                input_mint,
                output_mint,
                amount,
                slippage_bps: self.config.slippage_bps,
                ..Default::default()
            }))?;

        let swap = self.tokio_rt.block_on(
            jup_swap_client.swap(&SwapRequest {
                user_public_key: self.general_config.signer_pubkey,
                quote_response,
                config: TransactionConfig {
                    wrap_and_unwrap_sol: false,
                    compute_unit_price_micro_lamports: self
                        .config
                        .compute_unit_price_micro_lamports
                        .map(ComputeUnitPriceMicroLamports::MicroLamports),
                    ..Default::default()
                },
            }),
        )?;

        let mut tx = bincode::deserialize::<VersionedTransaction>(&swap.swap_transaction)
            .map_err(|_| anyhow!("Failed to deserialize"))?;

        tx = VersionedTransaction::try_new(
            tx.message,
            &[&read_keypair_file(&self.general_config.keypair_path).unwrap()],
        )?;

        TransactionSender::aggressive_send_tx(&self.txn_client, &tx, SenderCfg::DEFAULT)
            .map_err(|_| anyhow!("Failed to send swap transaction"))?;

        Ok(())
    }

    pub fn get_max_withdraw_for_bank(&self, bank_pk: &Pubkey) -> anyhow::Result<(I80F48, bool)> {
        let free_collateral = self.get_free_collateral()?;
        let balance = self
            .cache
            .marginfi_accounts
            .try_get_account(&self.liquidator_account.liquidator_address)?
            .get_balance_for_bank(&self.cache.try_get_bank_wrapper(bank_pk)?);
        Ok(match balance {
            Some((balance, BalanceSide::Assets)) => {
                let value = self.get_value(
                    balance,
                    bank_pk,
                    RequirementType::Initial,
                    BalanceSide::Assets,
                )?;

                let max_withdraw = value.min(free_collateral);

                let amount = self.get_amount(max_withdraw, bank_pk, Some(PriceBias::Low))?;

                (amount, value <= free_collateral)
            }
            _ => (I80F48!(0), false),
        })
    }

    pub fn get_value(
        &self,
        amount: I80F48,
        bank_pk: &Pubkey,
        requirement_type: RequirementType,
        side: BalanceSide,
    ) -> anyhow::Result<I80F48> {
        let bank = self.cache.try_get_bank_wrapper(bank_pk)?;
        let value = match side {
            BalanceSide::Assets => {
                calc_weighted_assets_new(&bank, amount.to_num(), requirement_type)?
            }
            BalanceSide::Liabilities => {
                calc_weighted_liabs_new(&bank, amount.to_num(), requirement_type)?
            }
        };
        Ok(value)
    }

    fn get_free_collateral(&self) -> anyhow::Result<I80F48> {
        let (assets, liabs) = self.calc_health(
            &self
                .cache
                .marginfi_accounts
                .try_get_account(&self.liquidator_account.liquidator_address)?,
            RequirementType::Initial,
        );
        if assets > liabs {
            Ok(assets - liabs)
        } else {
            Ok(I80F48::ZERO)
        }
    }

    /// Calculates the health of a given account
    fn calc_health(
        &self,
        account: &MarginfiAccountWrapper,
        requirement_type: RequirementType,
    ) -> (I80F48, I80F48) {
        let baws = BankAccountWithPriceFeedEva::load(&account.lending_account, self.cache.clone())
            .unwrap();

        baws.iter().fold(
            (I80F48::ZERO, I80F48::ZERO),
            |(total_assets, total_liabs), baw| {
                let (assets, liabs) = baw
                    .calc_weighted_assets_and_liabilities_values(requirement_type, false)
                    .unwrap();
                (total_assets + assets, total_liabs + liabs)
            },
        )
    }

    fn get_token_balance_for_bank(&self, bank_pk: &Pubkey) -> Option<I80F48> {
        self.cache
            .get_token_account_for_bank(bank_pk)
            .map(|account| I80F48::from_num(utils::accessor::amount(account.data())))
    }

    pub fn get_amount(
        &self,
        value: I80F48,
        bank_pk: &Pubkey,
        price_bias: Option<PriceBias>,
    ) -> anyhow::Result<I80F48> {
        let bank = self.cache.try_get_bank_wrapper(bank_pk)?;
        let price = bank.oracle_adapter.get_price_of_type(
            marginfi::state::price::OraclePriceType::RealTime,
            price_bias,
        )?;

        let amount_ui = value / price;

        Ok(amount_ui * EXP_10_I80F48[bank.bank.mint_decimals as usize])
    }
}
