use crate::{
    cache::Cache,
    config::Eva01Config,
    thread_debug, thread_error, thread_info, thread_trace,
    utils::{
        self, build_emode_config, calc_total_weighted_assets_liabs, calc_weighted_bank_assets,
        calc_weighted_bank_liabs, find_oracle_keys, get_free_collateral, swb_cranker::SwbCranker,
        BankAccountWithPriceFeedEva,
    },
    ward,
    wrappers::{
        liquidator_account::LiquidatorAccount,
        marginfi_account::MarginfiAccountWrapper,
        oracle::{OracleWrapper, OracleWrapperTrait},
    },
};
use anyhow::{anyhow, Result};
use fixed::types::I80F48;
use fixed_macro::types::I80F48;
use jupiter_swap_api_client::{
    quote::QuoteRequest,
    swap::SwapRequest,
    transaction_config::{ComputeUnitPriceMicroLamports, TransactionConfig},
    JupiterSwapApiClient,
};
use marginfi::{
    constants::EXP_10_I80F48,
    state::{
        marginfi_account::{BalanceSide, LendingAccount, RequirementType},
        price::{OracleSetup, PriceBias},
    },
};
use solana_client::{rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig};
use solana_program::pubkey::Pubkey;
use solana_sdk::{account::ReadableAccount, signature::Keypair, transaction::VersionedTransaction};
use solana_sdk::{commitment_config::CommitmentConfig, signature::Signer};
use std::{cmp::min, sync::Arc, thread, time::Duration};
use tokio::runtime::{Builder, Runtime};

const MIN_LIQUIDATIONS_COUNT: u64 = 10;

/// The rebalancer is responsible to keep the liquidator account
/// "rebalanced" -> Document this better
#[allow(dead_code)]
pub struct Rebalancer {
    signer: Keypair,
    liquidator_account: Arc<LiquidatorAccount>,
    swb_cranker: SwbCranker,
    rpc_client: RpcClient,
    swap_mint: Pubkey,
    swap_mint_bank: Pubkey,
    min_profit: f64,
    token_account_dust_threshold: I80F48,
    jup_swap_api_url: String,
    slippage_bps: u16,
    compute_unit_price_micro_lamports: ComputeUnitPriceMicroLamports,
    tokio_rt: Runtime,
    cache: Arc<Cache>,
}

impl Rebalancer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Eva01Config,
        liquidator_account: Arc<LiquidatorAccount>,
        cache: Arc<Cache>,
    ) -> anyhow::Result<Self> {
        let signer = Keypair::from_bytes(&config.general_config.wallet_keypair)?;

        let rpc_client = RpcClient::new(config.general_config.rpc_url.clone());

        let swap_mint = config.rebalancer_config.swap_mint;
        let swap_mint_bank = cache
            .banks
            .try_get_account_for_mint(&config.rebalancer_config.swap_mint)?;

        let min_profit = config.general_config.min_profit;
        let token_account_dust_threshold = config.rebalancer_config.token_account_dust_threshold;
        let jup_swap_api_url = config.rebalancer_config.jup_swap_api_url;
        let slippage_bps = config.rebalancer_config.slippage_bps;
        let compute_unit_price_micro_lamports = ComputeUnitPriceMicroLamports::MicroLamports(
            config.general_config.compute_unit_price_micro_lamports,
        );

        let tokio_rt = Builder::new_multi_thread()
            .thread_name("rebalancer")
            .worker_threads(2)
            .enable_all()
            .build()?;

        let swb_cranker = SwbCranker::new(&config.general_config)?;

        Ok(Self {
            signer,
            liquidator_account,
            swb_cranker,
            rpc_client,
            swap_mint,
            swap_mint_bank,
            min_profit,
            token_account_dust_threshold,
            jup_swap_api_url,
            slippage_bps,
            compute_unit_price_micro_lamports,
            tokio_rt,
            cache,
        })
    }

    pub fn fund_liquidator_account(&mut self) -> anyhow::Result<()> {
        // Multiply by 40 because the profit is 0.025 of the amount that is being liquidated.
        // And then multiply by MIN_LIQUIDATIONS_COUNT to get enough funds for that many liquidations.
        let usd_value = (self.min_profit * 40.0) * (MIN_LIQUIDATIONS_COUNT as f64);

        let sol_price = 150.0; // TODO: Fetch the current SOL price and account for slippage by lowering it
        let sol_amount: f64 = usd_value / sol_price;
        let swap_mint_amount = self.swap(
            solana_sdk::native_token::sol_to_lamports(sol_amount),
            spl_token::native_mint::ID,
            self.swap_mint,
        )?;

        thread_info!(
            "Funding the Liquidator account with {} tokens ({} in SOL).",
            swap_mint_amount,
            sol_amount
        );
        thread::sleep(Duration::from_secs(10));
        if let Err(error) = self.deposit_preferred_token(swap_mint_amount) {
            thread_error!("Failed to deposit preferred token: {}", error)
        }
        Ok(())
    }

    pub fn run(&mut self) -> anyhow::Result<()> {
        let lq_acc = self
            .cache
            .marginfi_accounts
            .try_get_account(&self.liquidator_account.liquidator_address)?;

        check_liquidator_health(&self.cache, &lq_acc)?;

        match self.needs_to_be_rebalanced(&lq_acc) {
            Ok(should_rebalance) => {
                if should_rebalance {
                    thread_info!("Running the Rebalancing process...");
                    self.rebalance_accounts();
                    thread_info!("The Rebalancing process is complete.");
                }
            }
            Err(error) => {
                thread_error!(
                    "Failed to check if the Liquidator account {} needs to be rebalanced: {}",
                    &self.liquidator_account.liquidator_address,
                    error
                );
            }
        }
        Ok(())
    }

    fn needs_to_be_rebalanced(&mut self, lq_acc: &MarginfiAccountWrapper) -> Result<bool> {
        Ok(self.has_non_preferred_deposits(&lq_acc.lending_account)? || lq_acc.has_liabs())
    }

    fn crank_active_swb_oracles(&self) -> anyhow::Result<()> {
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
                self.cache
                    .banks
                    .get_bank(&bank_pk)
                    .and_then(|bank_wrapper| {
                        if matches!(
                            bank_wrapper.bank.config.oracle_setup,
                            OracleSetup::SwitchboardPull
                        ) {
                            Some(bank_wrapper.bank.config)
                        } else {
                            None
                        }
                    })
            })
            .flat_map(|swb_bank_config| find_oracle_keys(&swb_bank_config))
            .collect();

        if !active_swb_oracles.is_empty() {
            self.swb_cranker.crank_oracles(active_swb_oracles)?
        }

        Ok(())
    }

    fn deposit_preferred_token(&self, amount: u64) -> anyhow::Result<()> {
        if amount == 0 {
            return Err(anyhow::anyhow!("Deposit amount is zero!"));
        }
        thread_debug!(
            "Depositing {} of preferred token to the Swap mint bank {:?}.",
            amount,
            &self.swap_mint_bank
        );

        let bank_wrapper = self.cache.banks.try_get_bank(&self.swap_mint_bank)?;
        if let Err(error) = self.liquidator_account.deposit(&bank_wrapper, amount) {
            thread_error!(
                "Failed to deposit to the Bank ({:?}): {:?}",
                &self.swap_mint_bank,
                error
            );
        }

        Ok(())
    }

    fn rebalance_accounts(&mut self) {
        if let Err(error) = self.crank_active_swb_oracles() {
            thread_error!(
                "Failed to crank Swb Oracles for the Liquidator banks: {}",
                error
            )
        }

        if let Err(error) = self.repay_liabilities() {
            thread_error!("Failed to repay liabilities! {}", error)
        }

        let sold_deposits = ward!(self
            .sell_non_preferred_deposits()
            .map_err(|e| thread_error!("Failed to sell non preferred deposits! {}", e))
            .ok());
        thread_debug!("Sold {:?} non-preferred deposits.", sold_deposits.len());

        let drained_amount = ward!(self
            .drain_tokens_from_token_accounts(sold_deposits)
            .map_err(|e| thread_error!("Failed to drain the Liquidator's tokens! {}", e))
            .ok());
        thread_debug!(
            "Drained {:?} tokens from the Liquidator's token accounts.",
            drained_amount
        );

        if drained_amount > 0 {
            if let Err(error) = self.deposit_preferred_token(drained_amount) {
                thread_error!("Failed to deposit preferred token: {}", error)
            }
        }
    }

    fn sell_non_preferred_deposits(&mut self) -> Result<Vec<(Pubkey, Pubkey)>> {
        let liquidator_account = self
            .cache
            .marginfi_accounts
            .try_get_account(&self.liquidator_account.liquidator_address)?;
        let non_preferred_deposits = liquidator_account.get_deposits(&[self.swap_mint_bank]);

        if non_preferred_deposits.is_empty() {
            return Ok(vec![]);
        }

        let mut sold_deposits = vec![];

        thread_debug!("Selling non-preferred deposits.");

        for bank_pk in non_preferred_deposits {
            match self.withdraw_and_sell_deposit(&bank_pk, &liquidator_account) {
                Err(error) => {
                    thread_error!(
                        "Failed to withdraw and sell deposit for the Bank ({}): {:?}",
                        bank_pk,
                        error
                    );
                }
                Ok(mint_pk) => {
                    sold_deposits.push((mint_pk, bank_pk));
                }
            }
        }

        Ok(sold_deposits)
    }

    fn repay_liabilities(&mut self) -> Result<()> {
        let lq_account = self
            .cache
            .marginfi_accounts
            .try_get_account(&self.liquidator_account.liquidator_address)?;
        let liabilities = lq_account.get_liabilities_shares();

        if !liabilities.is_empty() {
            thread_debug!("Repaying liabilities.");
            for (_, bank_pk) in liabilities {
                if let Err(err) = self.repay_liability(bank_pk, &lq_account) {
                    thread_error!(
                        "Failed to repay liability for the Bank ({}): {:?}",
                        bank_pk,
                        err
                    );
                }
            }
        }

        Ok(())
    }

    /// Repay a liability for a given bank
    ///
    /// - Calc $ value of liab
    /// - Find swap token account
    /// - Swap enough of swap token to cover the liab in bank token
    /// - Repay liability
    fn repay_liability(
        &mut self,
        bank_pk: Pubkey,
        lq_account: &MarginfiAccountWrapper,
    ) -> anyhow::Result<()> {
        let bank_wrapper = self.cache.banks.try_get_bank(&bank_pk)?;

        thread_debug!(
            "Evaluating the {:?} Bank liability for the Liquidator account {:?}.",
            &bank_pk,
            lq_account.address
        );

        // Get the balance for the liability bank and check if it's valid
        let liab_balance_opt = lq_account.get_balance_for_bank(&bank_wrapper);

        if liab_balance_opt.is_none() || matches!(liab_balance_opt, Some((_, BalanceSide::Assets)))
        {
            return Ok(());
        }

        let (liab_balance, _) = liab_balance_opt.unwrap();

        if liab_balance.is_zero() {
            return Ok(());
        }

        let liab_usd_value = self.get_value(
            liab_balance,
            &bank_pk,
            lq_account,
            RequirementType::Equity,
            BalanceSide::Liabilities,
        )?;
        if liab_usd_value < self.token_account_dust_threshold {
            thread_debug!(
                "The {:?} unscaled Liability tokens of Bank {:?} are below the dust threshold {}.",
                liab_balance,
                bank_pk,
                self.token_account_dust_threshold
            );
            return Ok(());
        }

        thread_debug!(
            "The {:?} unscaled Liability tokens of Bank {:?} need to be repaid with {:?} USD.",
            liab_balance,
            bank_pk,
            liab_usd_value
        );

        // Get the amount of swap token needed to repay the liability
        let required_swap_token =
            self.get_amount(liab_usd_value, &self.swap_mint_bank, Some(PriceBias::Low))?;

        let withdraw_amount = if required_swap_token.is_positive() {
            let (max_withdraw_amount, withdraw_all) =
                self.get_max_withdraw_for_bank(&self.swap_mint_bank, lq_account)?;

            let withdraw_amount = min(max_withdraw_amount, required_swap_token);

            let swap_bank_wrapper = self.cache.banks.try_get_bank(&self.swap_mint_bank)?;
            self.liquidator_account.withdraw(
                &swap_bank_wrapper,
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
                self.swap_mint_bank,
            );
            self.swap(
                amount_to_swap.to_num(),
                self.swap_mint,
                bank_wrapper.bank.mint,
            )?;
        }

        thread_debug!("Repaying liability for bank {}", bank_pk);

        let token_balance = self
            .get_token_balance_for_mint(&bank_wrapper.bank.mint)
            .unwrap_or_default();

        let repay_all = token_balance >= liab_balance;

        self.liquidator_account
            .repay(&bank_wrapper, token_balance.to_num(), Some(repay_all))?;

        Ok(())
    }

    #[allow(dead_code)]
    // This function evaluates all loaded tokens, but it should only look at the Liquidator's token accounts.
    // Addressed in https://linear.app/p0dotxyz/issue/LIQ-20/the-token-balances-check-is-broken
    fn has_tokens_in_token_accounts(&self) -> Result<bool> {
        for (mint_address, token_address) in self
            .cache
            .tokens
            .get_non_preferred_mints(self.cache.get_preferred_mints())
        {
            match self
                .cache
                .try_get_token_wrapper::<OracleWrapper>(&mint_address, &token_address)
            {
                Ok(wrapper) => match wrapper.get_value() {
                    Ok(value) => {
                        if value > self.token_account_dust_threshold {
                            return Ok(true);
                        }
                    }
                    Err(error) => thread_error!(
                        "Failed compute the Liquidator's Token {} value for mint {}: {}",
                        token_address,
                        mint_address,
                        error
                    ),
                },
                Err(error) => thread_debug!(
                    "Skipping evaluation of the Liquidator's Token {} for mint {}. Cause: {}",
                    token_address,
                    mint_address,
                    error
                ),
            }
        }

        Ok(false)
    }

    fn has_non_preferred_deposits(&self, lq_account: &LendingAccount) -> Result<bool> {
        Ok(lq_account
            .balances
            .iter()
            .filter(|balance| balance.is_active())
            .any(|balance| {
                self.cache
                    .banks
                    .get_bank(&balance.bank_pk)
                    .is_some_and(|bank_wrapper| {
                        matches!(balance.get_side(), Some(BalanceSide::Assets))
                            && bank_wrapper.bank.mint != self.swap_mint
                    })
            }))
    }

    fn drain_tokens_from_token_accounts(
        &mut self,
        mint_token_addresses: Vec<(Pubkey, Pubkey)>,
    ) -> anyhow::Result<u64> {
        let mut drained_amount = 0;
        for (mint_address, token_address) in mint_token_addresses {
            match self.cache.try_get_token_wrapper::<OracleWrapper>(&mint_address, &token_address) {
                Ok(wrapper) => {
                    // Ignore the swap token, usually USDC
                    if wrapper.bank_wrapper.bank.mint == self.swap_mint {
                        continue;
                    }

                    let value = wrapper.get_value()?;
                    if value > self.token_account_dust_threshold {
                        drained_amount += self.swap(
                            wrapper.get_amount().to_num(),
                            wrapper.bank_wrapper.bank.mint,
                            self.swap_mint,
                        )?;
                    } else {
                        thread_debug!(
                                "The {:?} unscaled Liquidator's Token {:?} amount is below the dust threshold {}.",
                                wrapper.get_amount(),
                                &token_address,
                                self.token_account_dust_threshold
                            );
                    }
                }
                Err(error) => thread_trace!(
                    "Not draining the Liquidator's token {} because failed to obtain the token wrapper: {}",
                    token_address,
                    error
                ),
            }
        }

        Ok(drained_amount)
    }

    /// Withdraw and sells a given asset
    fn withdraw_and_sell_deposit(
        &mut self,
        bank_pk: &Pubkey,
        lq_account: &MarginfiAccountWrapper,
    ) -> anyhow::Result<Pubkey> {
        let bank_wrapper = self.cache.banks.try_get_bank(bank_pk)?;
        let balance = lq_account.get_balance_for_bank(&bank_wrapper);

        if !matches!(&balance, Some((_, BalanceSide::Assets))) {
            return Err(anyhow!("No assets to withdraw from the bank"));
        }

        let (withdraw_amount, withdrawl_all) =
            self.get_max_withdraw_for_bank(bank_pk, lq_account)?;

        let amount = withdraw_amount.to_num::<u64>();

        self.liquidator_account
            .withdraw(&bank_wrapper, amount, Some(withdrawl_all))?;

        self.swap(amount, bank_wrapper.bank.mint, self.swap_mint)?;

        Ok(bank_wrapper.bank.mint)
    }

    fn swap(&self, amount: u64, input_mint: Pubkey, output_mint: Pubkey) -> anyhow::Result<u64> {
        thread_info!("Jupiter swap: {} -> {}", input_mint, output_mint);
        let jup_swap_client = JupiterSwapApiClient::new(self.jup_swap_api_url.clone());

        let quote_response = self
            .tokio_rt
            .block_on(jup_swap_client.quote(&QuoteRequest {
                input_mint,
                output_mint,
                amount,
                slippage_bps: self.slippage_bps,
                ..Default::default()
            }))?;

        let out_amount = quote_response.out_amount;

        let swap = self.tokio_rt.block_on(jup_swap_client.swap(
            &SwapRequest {
                user_public_key: self.signer.pubkey(),
                quote_response,
                config: TransactionConfig {
                    wrap_and_unwrap_sol: input_mint == spl_token::native_mint::ID,
                    compute_unit_price_micro_lamports: Some(
                        self.compute_unit_price_micro_lamports.clone(),
                    ),
                    ..Default::default()
                },
            },
            None,
        ))?;

        let mut tx = bincode::deserialize::<VersionedTransaction>(&swap.swap_transaction)
            .map_err(|_| anyhow::anyhow!("Failed to deserialize"))?;

        tx = VersionedTransaction::try_new(tx.message, &[&self.signer])?;

        thread_info!(
            "Swapping unscaled {} tokens of mint {} to {} tokens of mint {} ...",
            amount,
            input_mint,
            out_amount,
            output_mint
        );

        let sig = self
            .rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &tx,
                CommitmentConfig::finalized(),
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    ..Default::default()
                },
            )?;
        thread_info!("The swap txn is finalized. Sig: {:?}", sig);

        Ok(out_amount)
    }

    pub fn get_max_withdraw_for_bank(
        &self,
        bank_pk: &Pubkey,
        lq_account: &MarginfiAccountWrapper,
    ) -> anyhow::Result<(I80F48, bool)> {
        let free_collateral = get_free_collateral(&self.cache, lq_account)?;
        let balance = lq_account.get_balance_for_bank(&self.cache.banks.try_get_bank(bank_pk)?);
        Ok(match balance {
            Some((balance, BalanceSide::Assets)) => {
                let value = self.get_value(
                    balance,
                    bank_pk,
                    lq_account,
                    RequirementType::Equity,
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
        lq_account: &MarginfiAccountWrapper,
        requirement_type: RequirementType,
        side: BalanceSide,
    ) -> anyhow::Result<I80F48> {
        let bank_wrapper = self.cache.banks.try_get_bank(bank_pk)?;
        let oracle_wrapper = OracleWrapper::build(&self.cache, bank_pk)?;

        let value = match side {
            BalanceSide::Assets => {
                let baws = BankAccountWithPriceFeedEva::<OracleWrapper>::load(
                    &lq_account.lending_account,
                    &self.cache,
                )?;
                let emode_config = build_emode_config(&baws)?;

                calc_weighted_bank_assets(
                    &bank_wrapper,
                    &oracle_wrapper,
                    amount.to_num(),
                    requirement_type,
                    &emode_config,
                )?
            }
            BalanceSide::Liabilities => calc_weighted_bank_liabs(
                &bank_wrapper,
                &oracle_wrapper,
                amount.to_num(),
                requirement_type,
            )?,
        };
        Ok(value)
    }

    fn get_token_balance_for_mint(&self, mint_address: &Pubkey) -> Option<I80F48> {
        let token_account_address = self.cache.tokens.get_token_for_mint(mint_address)?;
        match self.cache.tokens.try_get_account(&token_account_address) {
            Ok(account) => match utils::accessor::amount(account.data()) {
                Ok(amount) => Some(I80F48::from_num(amount)),
                Err(error) => {
                    thread_error!(
                        "Failed to obtain balance amount for the Token {}: {}",
                        token_account_address,
                        error
                    );
                    None
                }
            },
            Err(error) => {
                thread_error!(
                    "Failed to get the Token account {}: {}",
                    token_account_address,
                    error
                );
                None
            }
        }
    }

    pub fn get_amount(
        &self,
        value: I80F48,
        bank_pk: &Pubkey,
        price_bias: Option<PriceBias>,
    ) -> anyhow::Result<I80F48> {
        let bank_wrapper = self.cache.banks.try_get_bank(bank_pk)?;
        let oracle_wrapper = OracleWrapper::build(&self.cache, bank_pk)?;
        let price = oracle_wrapper.get_price_of_type(
            marginfi::state::price::OraclePriceType::RealTime,
            price_bias,
        )?;

        let amount_ui = value / price;

        Ok(amount_ui * EXP_10_I80F48[bank_wrapper.bank.mint_decimals as usize])
    }
}

// If our margin is at 50% or lower, we should stop the Liquidator and manually adjust it's balances.
pub fn check_liquidator_health(cache: &Arc<Cache>, lq_acc: &MarginfiAccountWrapper) -> Result<()> {
    let (weighted_assets, weighted_liabs) =
        calc_total_weighted_assets_liabs(cache, &lq_acc.lending_account, RequirementType::Initial)?;

    if weighted_assets.is_zero() {
        return Err(anyhow!(
            "The Liquidator {:?} has no assets!",
            lq_acc.address
        ));
    }

    let ratio = (weighted_assets - weighted_liabs) / weighted_assets;
    if ratio <= 0.5 {
        return Err(anyhow!(
            "The Assets ({}) to Liabilities ({}) ratio ({}) too low for the Liquidator {:?}!",
            weighted_assets,
            weighted_liabs,
            ratio,
            lq_acc.address
        ));
    }

    Ok(())
}
