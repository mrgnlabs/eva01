use crate::{
    config::{GeneralConfig, RebalancerCfg},
    crossbar::CrossbarMaintainer,
    geyser::{AccountType, GeyserUpdate},
    sender::{SenderCfg, TransactionSender},
    token_account_manager::TokenAccountManager,
    transaction_manager::{BatchTransactions, RawTransaction},
    utils::{
        accessor, batch_get_multiple_accounts, calc_weighted_assets_new, calc_weighted_liabs_new,
        BankAccountWithPriceFeedEva,
    },
    wrappers::{
        bank::BankWrapper, liquidator_account::LiquidatorAccount,
        marginfi_account::MarginfiAccountWrapper, token_account::TokenAccountWrapper,
    },
};
use anyhow::anyhow;
use crossbeam::channel::{Receiver, Sender};
use fixed::types::I80F48;
use fixed_macro::types::I80F48;
use jupiter_swap_api_client::{
    quote::QuoteRequest,
    swap::SwapRequest,
    transaction_config::{ComputeUnitPriceMicroLamports, TransactionConfig},
    JupiterSwapApiClient,
};
use log::{debug, info, warn};
use marginfi::{
    constants::EXP_10_I80F48,
    state::{
        marginfi_account::{BalanceSide, MarginfiAccount, RequirementType},
        price::{OraclePriceFeedAdapter, OracleSetup, PriceBias, SwitchboardPullPriceFeed},
    },
};
use solana_client::{
    nonblocking::rpc_client::RpcClient as NonBlockingRpcClient, rpc_client::RpcClient,
};
use solana_program::pubkey::Pubkey;
use solana_sdk::{
    account_info::IntoAccountInfo, clock::Clock, commitment_config::CommitmentConfig,
    signature::read_keypair_file, transaction::VersionedTransaction,
};
use std::{
    cmp::min,
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::{atomic::AtomicBool, Arc},
};
use switchboard_on_demand::PullFeedAccountData;
use switchboard_on_demand_client::QueueAccountData;
use switchboard_on_demand_client::{FetchUpdateManyParams, Gateway, PullFeed};
/// The rebalancer is responsible to keep the liquidator account
/// "rebalanced" -> Document this better
pub struct Rebalancer {
    config: RebalancerCfg,
    general_config: GeneralConfig,
    liquidator_account: LiquidatorAccount,
    token_accounts: HashMap<Pubkey, TokenAccountWrapper>,
    banks: HashMap<Pubkey, BankWrapper>,
    token_account_manager: TokenAccountManager,
    rpc_client: Arc<RpcClient>,
    mint_to_bank: HashMap<Pubkey, Pubkey>,
    oracle_to_bank: HashMap<Pubkey, Pubkey>,
    preferred_mints: HashSet<Pubkey>,
    swap_mint_bank_pk: Option<Pubkey>,
    geyser_receiver: Receiver<GeyserUpdate>,
    stop_liquidations: Arc<AtomicBool>,
    crossbar_client: CrossbarMaintainer,
}

impl Rebalancer {
    pub async fn new(
        general_config: GeneralConfig,
        config: RebalancerCfg,
        transaction_tx: Sender<BatchTransactions>,
        geyser_receiver: Receiver<GeyserUpdate>,
        stop_liquidation: Arc<AtomicBool>,
    ) -> anyhow::Result<Self> {
        let rpc_client = Arc::new(RpcClient::new(general_config.rpc_url.clone()));
        let token_account_manager = TokenAccountManager::new(rpc_client.clone())?;

        let liquidator_account = LiquidatorAccount::new(
            RpcClient::new(general_config.rpc_url.clone()),
            general_config.liquidator_account,
            transaction_tx.clone(),
            general_config.clone(),
        )
        .await?;

        let preferred_mints = config.preferred_mints.iter().cloned().collect();

        Ok(Rebalancer {
            config,
            general_config,
            liquidator_account,
            token_accounts: HashMap::new(),
            banks: HashMap::new(),
            token_account_manager,
            rpc_client,
            mint_to_bank: HashMap::new(),
            oracle_to_bank: HashMap::new(),
            preferred_mints,
            swap_mint_bank_pk: None,
            geyser_receiver,
            stop_liquidations: stop_liquidation,
            crossbar_client: CrossbarMaintainer::new(),
        })
    }

    pub async fn load_data(
        &mut self,
        banks_and_map: (HashMap<Pubkey, BankWrapper>, HashMap<Pubkey, Pubkey>),
    ) -> anyhow::Result<()> {
        self.banks = banks_and_map.0;
        self.oracle_to_bank = banks_and_map.1;
        let mut bank_mints = Vec::new();

        for bank in self.banks.values() {
            bank_mints.push(bank.bank.mint);
            self.mint_to_bank.insert(bank.bank.mint, bank.address);
        }

        self.token_account_manager
            .add_mints(&bank_mints, self.general_config.signer_pubkey)?;

        self.token_account_manager
            .create_token_accounts(self.liquidator_account.signer_keypair.clone())?;

        let (mints, token_account_addresses) = self
            .token_account_manager
            .get_mints_and_token_account_addresses();

        self.liquidator_account
            .load_initial_data(&self.rpc_client, mints.clone())
            .await?;

        let accounts = batch_get_multiple_accounts(
            self.rpc_client.clone(),
            &token_account_addresses,
            crate::utils::BatchLoadingConfig::DEFAULT,
        )?;

        info!("Loaded {:?} token accounts!", accounts.len());

        let token_accounts_with_addresses_and_mints = token_account_addresses
            .iter()
            .zip(mints.iter())
            .zip(accounts)
            .collect::<Vec<_>>();

        for ((token_account_addresses, mint), maybe_token_account) in
            token_accounts_with_addresses_and_mints.iter()
        {
            let balance = maybe_token_account
                .as_ref()
                .map(|a| accessor::amount(&a.data))
                .unwrap_or(0);

            let bank = self
                .banks
                .get(self.mint_to_bank.get(mint).unwrap())
                .unwrap();

            let mint_decimals = bank.bank.mint_decimals;

            self.token_accounts.insert(
                **mint,
                TokenAccountWrapper {
                    address: **token_account_addresses,
                    mint: **mint,
                    balance,
                    mint_decimals,
                    bank_address: bank.address,
                },
            );
        }

        self.swap_mint_bank_pk = self
            .get_bank_for_mint(&self.config.swap_mint)
            .map(|bank| bank.address);

        Ok(())
    }

    pub async fn start(&mut self) -> anyhow::Result<()> {
        let max_duration = std::time::Duration::from_secs(10);
        loop {
            let start = std::time::Instant::now().checked_sub(max_duration).unwrap();
            while let Ok(mut msg) = self.geyser_receiver.recv() {
                debug!("Received message {:?}", msg);
                match msg.account_type {
                    AccountType::OracleAccount => {
                        if let Some(bank_to_update_pk) = self.oracle_to_bank.get(&msg.address) {
                            let bank_to_update: &mut BankWrapper =
                                self.banks.get_mut(bank_to_update_pk).unwrap();

                            let oracle_price_adapter = match bank_to_update.bank.config.oracle_setup
                            {
                                OracleSetup::SwitchboardPull => {
                                    let mut offsets_data =
                                        [0u8; std::mem::size_of::<PullFeedAccountData>()];
                                    offsets_data.copy_from_slice(
                                        &msg.account.data
                                            [8..std::mem::size_of::<PullFeedAccountData>() + 8],
                                    );
                                    let swb_feed = crate::utils::load_swb_pull_account_from_bytes(
                                        &offsets_data,
                                    )
                                    .unwrap();

                                    let feed_hash = hex::encode(swb_feed.feed_hash);
                                    bank_to_update.oracle_adapter.swb_feed_hash = Some(feed_hash);

                                    OraclePriceFeedAdapter::SwitchboardPull(
                                        SwitchboardPullPriceFeed {
                                            feed: Box::new((&swb_feed).into()),
                                        },
                                    )
                                }
                                _ => {
                                    let oracle_account_info =
                                        (&msg.address, &mut msg.account).into_account_info();
                                    OraclePriceFeedAdapter::try_from_bank_config_with_max_age(
                                        &bank_to_update.bank.config,
                                        &[oracle_account_info],
                                        &Clock::default(),
                                        i64::MAX as u64,
                                    )
                                    .unwrap()
                                }
                            };

                            bank_to_update.oracle_adapter.price_adapter = oracle_price_adapter;
                        }
                    }
                    AccountType::MarginfiAccount => {
                        if msg.address == self.general_config.liquidator_account {
                            let marginfi_account =
                                bytemuck::from_bytes::<MarginfiAccount>(&msg.account.data[8..]);

                            self.liquidator_account.account_wrapper.account = *marginfi_account;
                        }
                    }
                    AccountType::TokenAccount => {
                        let mint = accessor::mint(&msg.account.data);
                        let balance = accessor::amount(&msg.account.data);

                        let token_to_update = self.token_accounts.get_mut(&mint).unwrap();

                        token_to_update.balance = balance;
                    }
                }

                if start.elapsed() > max_duration && self.needs_to_be_relanced().await {
                    if let Err(e) = self.rebalance_accounts().await {
                        info!("Failed to rebalance account: {:?}", e);
                    }
                    break;
                }
            }
        }
    }

    async fn needs_to_be_relanced(&mut self) -> bool {
        // Update switchboard pull prices with crossbar
        let swb_feed_hashes = self
            .banks
            .values()
            .filter_map(|bank| {
                if let Some(feed_hash) = &bank.oracle_adapter.swb_feed_hash {
                    Some((bank.address, feed_hash.clone()))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        let simulated_prices = self.crossbar_client.simulate(swb_feed_hashes).await;

        for (bank_pk, price) in simulated_prices {
            let bank = self.banks.get_mut(&bank_pk).unwrap();
            bank.oracle_adapter.simulated_price = Some(price);
        }

        self.should_stop_liquidations().await.unwrap();

        self.has_tokens_in_token_accounts()
            || self.has_non_preferred_deposits()
            || self.has_liabilities()
    }

    async fn rebalance_accounts(&mut self) -> anyhow::Result<()> {
        let active_banks = self.liquidator_account.account_wrapper.get_active_banks();

        let active_swb_oracles: Vec<Pubkey> = active_banks
            .iter()
            .filter_map(|&bank_pk| {
                self.banks.get(&bank_pk).and_then(|bank| {
                    if bank.oracle_adapter.is_switchboard_pull() {
                        Some(bank.oracle_adapter.address)
                    } else {
                        None
                    }
                })
            })
            .collect();

        if !active_swb_oracles.is_empty() {
            if let Ok((ix, lut)) = PullFeed::fetch_update_many_ix(
                &self.liquidator_account.non_blocking_rpc_client,
                FetchUpdateManyParams {
                    feeds: active_swb_oracles,
                    payer: self.general_config.signer_pubkey,
                    gateway: self.liquidator_account.swb_gateway.clone(),
                    num_signatures: Some(1),
                    ..Default::default()
                },
            )
            .await
            {
                self.liquidator_account
                    .transaction_tx
                    .send(vec![RawTransaction::new(vec![ix]).with_lookup_tables(lut)])
                    .unwrap();
            }
        }
        debug!("Rebalancing accounts");
        self.sell_non_preferred_deposits().await?;
        self.repay_liabilities().await?;
        self.handle_tokens_in_token_accounts().await?;
        self.deposit_preferred_tokens().await?;

        Ok(())
    }

    // If our margin is at 50% or lower, we should stop liquidations and await until the account
    // is fully rebalanced
    pub async fn should_stop_liquidations(&self) -> anyhow::Result<()> {
        let (assets, liabs) = self.calc_health(
            &self.liquidator_account.account_wrapper,
            RequirementType::Initial,
        );

        if assets.is_zero() {
            warn!("Assets are zero, stopping liquidations");

            self.stop_liquidations
                .store(true, std::sync::atomic::Ordering::Relaxed);

            return Ok(());
        }

        if (assets - liabs) / assets <= 0.5 {
            self.stop_liquidations
                .store(true, std::sync::atomic::Ordering::Relaxed);
        } else {
            self.stop_liquidations
                .store(false, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(())
    }

    pub fn get_accounts_to_track(&self) -> HashMap<Pubkey, AccountType> {
        let mut tracked_accounts: HashMap<Pubkey, AccountType> = HashMap::new();

        for token_account in self.token_accounts.values() {
            tracked_accounts.insert(token_account.address, AccountType::TokenAccount);
        }

        tracked_accounts
    }

    pub fn get_bank_for_mint(&self, mint: &Pubkey) -> Option<&BankWrapper> {
        Some(
            self.banks
                .iter()
                .find(|(_, bank)| bank.bank.mint == *mint)?
                .1,
        )
    }

    async fn sell_non_preferred_deposits(&mut self) -> anyhow::Result<()> {
        let non_preferred_deposits = self
            .liquidator_account
            .account_wrapper
            .get_deposits(&self.config.preferred_mints, &self.banks)?;

        if non_preferred_deposits.is_empty() {
            return Ok(());
        }

        for (_, bank_pk) in non_preferred_deposits {
            self.withdraw_and_sell_deposit(&bank_pk).await?;
        }
        Ok(())
    }

    async fn repay_liabilities(&mut self) -> anyhow::Result<()> {
        let liabilities = self
            .liquidator_account
            .account_wrapper
            .get_liabilities_shares();

        for (_, bank_pk) in liabilities {
            let _ = self.repay_liability(bank_pk).await;
        }

        Ok(())
    }

    /// Repay a liability for a given bank
    ///
    /// - Find any bank tokens in token accounts
    /// - Calc $ value of liab
    /// - Find USDC in token accounts
    /// - Calc additional USDC to withdraw
    /// - Withdraw USDC
    /// - Swap USDC for bank tokens
    /// - Repay liability
    async fn repay_liability(&mut self, bank_pk: Pubkey) -> anyhow::Result<()> {
        let bank = self.banks.get(&bank_pk).unwrap();

        // Get the balance for the liability and check if it's a valide balance

        let balance = self
            .liquidator_account
            .account_wrapper
            .get_balance_for_bank(&bank_pk, bank)?;

        if balance.is_none() || matches!(balance, Some((_, BalanceSide::Assets))) {
            return Ok(());
        }

        let (liab_balance, _) = balance.unwrap();

        // Gets how much tokens of needing repay asset to purchase

        let token_balance = self
            .get_token_balance_for_bank(&bank_pk)?
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

        // Get the amount of USDC needed to repay the liability

        let required_swap_token =
            self.get_amount(liab_usd_value, &self.swap_mint_bank_pk.unwrap(), None)?;

        let swap_token_balance = self
            .get_token_balance_for_bank(&self.swap_mint_bank_pk.unwrap())?
            .unwrap_or_default();

        let token_balance_to_withdraw = required_swap_token - swap_token_balance;

        let withdraw_amount = if token_balance_to_withdraw.is_positive() {
            let (max_withdraw_amount, withdraw_all) =
                self.get_max_withdraw_for_bank(&self.swap_mint_bank_pk.unwrap())?;

            let withdraw_amount = min(max_withdraw_amount, token_balance_to_withdraw);

            let bank = self.banks.get(&self.swap_mint_bank_pk.unwrap()).unwrap();

            self.liquidator_account.withdraw(
                bank,
                self.token_account_manager
                    .get_address_for_mint(bank.bank.mint)
                    .unwrap(),
                withdraw_amount.to_num(),
                Some(withdraw_all),
                &self.banks,
            )?;

            withdraw_amount
        } else {
            I80F48::ZERO
        };

        let amount_to_swap = min(liab_balance + withdraw_amount, required_swap_token);

        if amount_to_swap.is_positive() {
            self.swap(
                amount_to_swap.to_num(),
                &self.swap_mint_bank_pk.unwrap(),
                &bank_pk,
            )
            .await?;

            self.refresh_token_account(&bank_pk).await?;
        }

        let token_balance = self
            .get_token_balance_for_bank(&bank_pk)?
            .unwrap_or_default();

        let repay_all = token_balance >= liab_balance;

        let bank = self.banks.get(&bank_pk).unwrap();

        self.liquidator_account.repay(
            bank,
            &self
                .token_account_manager
                .get_address_for_mint(bank.bank.mint)
                .unwrap(),
            token_balance.to_num(),
            Some(repay_all),
        )?;

        Ok(())
    }

    async fn deposit_preferred_tokens(&self) -> anyhow::Result<()> {
        let balance = self.get_token_balance_for_bank(&self.swap_mint_bank_pk.unwrap())?;

        if balance.is_none() {
            return Ok(());
        }

        let balance = balance.unwrap();

        if balance.is_zero() {
            return Ok(());
        }

        let bank = self.banks.get(&self.swap_mint_bank_pk.unwrap()).unwrap();
        let token_address = self
            .token_account_manager
            .get_address_for_mint(bank.bank.mint)
            .unwrap();

        self.liquidator_account
            .deposit(bank, token_address, balance.to_num())?;

        Ok(())
    }

    fn has_tokens_in_token_accounts(&self) -> bool {
        let has_tokens_in_tas = self.token_accounts.values().any(|account| {
            let bank = self.banks.get(&account.bank_address).unwrap();
            let value = account.get_value(bank).unwrap();
            value > self.config.token_account_dust_threshold
        });
        has_tokens_in_tas
    }

    fn has_non_preferred_deposits(&self) -> bool {
        let has_non_preferred_deposits = self
            .liquidator_account
            .account_wrapper
            .account
            .lending_account
            .balances
            .iter()
            .filter(|balance| balance.active)
            .any(|balance| {
                let mint = self
                    .banks
                    .get(&balance.bank_pk)
                    .map(|bank| bank.bank.mint)
                    .unwrap();

                matches!(balance.get_side(), Some(BalanceSide::Assets))
                    && !self.preferred_mints.contains(&mint)
            });

        has_non_preferred_deposits
    }

    fn has_liabilities(&self) -> bool {
        self.liquidator_account.account_wrapper.has_liabs()
    }

    async fn handle_tokens_in_token_accounts(&mut self) -> anyhow::Result<()> {
        // Step 1: Collect necessary data into a Vec to avoid borrowing issues
        let accounts_data: Vec<(I80F48, I80F48, Pubkey, Pubkey)> = self
            .token_accounts
            .values()
            .filter_map(|account| {
                if account.mint == self.config.swap_mint {
                    return None;
                }
                let bank = self.banks.get(&account.bank_address).unwrap();
                let value = account.get_value(bank).unwrap();
                Some((
                    value,
                    account.get_amount(),
                    account.bank_address,
                    account.mint,
                ))
            })
            .collect();

        // Step 2: Iterate over the collected data
        for (value, amount, bank_address, _) in accounts_data {
            if value > self.config.token_account_dust_threshold {
                self.swap(
                    amount.to_num(),
                    &bank_address,
                    &self.swap_mint_bank_pk.unwrap(),
                )
                .await?;
            }
        }

        Ok(())
    }

    /// Withdraw and sells a given asset
    async fn withdraw_and_sell_deposit(&mut self, bank_pk: &Pubkey) -> anyhow::Result<()> {
        let balance = self
            .liquidator_account
            .account_wrapper
            .get_balance_for_bank(bank_pk, self.banks.get(bank_pk).unwrap())?;

        if !matches!(&balance, Some((_, BalanceSide::Assets))) {
            return Ok(());
        }

        let (withdraw_amount, withdrawl_all) = self.get_max_withdraw_for_bank(bank_pk)?;

        let amount = withdraw_amount.to_num::<u64>();

        let bank = self.banks.get(bank_pk).unwrap();

        self.liquidator_account.withdraw(
            bank,
            self.token_account_manager
                .get_address_for_mint(bank.bank.mint)
                .unwrap(),
            amount,
            Some(withdrawl_all),
            &self.banks,
        )?;

        self.swap(amount, bank_pk, &self.swap_mint_bank_pk.unwrap())
            .await?;

        Ok(())
    }

    async fn swap(
        &mut self,
        amount: u64,
        src_bank: &Pubkey,
        dst_bank: &Pubkey,
    ) -> anyhow::Result<()> {
        let src_mint = {
            let bank = self.banks.get(src_bank).unwrap();

            bank.bank.mint
        };

        let dst_mint = {
            let bank = self.banks.get(dst_bank).unwrap();

            bank.bank.mint
        };

        let jup_swap_client = JupiterSwapApiClient::new(self.config.jup_swap_api_url.clone());

        let quote_response = jup_swap_client
            .quote(&QuoteRequest {
                input_mint: src_mint,
                output_mint: dst_mint,
                amount,
                slippage_bps: self.config.slippage_bps,
                ..Default::default()
            })
            .await?;

        let swap = jup_swap_client
            .swap(&SwapRequest {
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
            })
            .await?;

        let mut tx = bincode::deserialize::<VersionedTransaction>(&swap.swap_transaction)
            .map_err(|_| anyhow!("Failed to deserialize"))?;

        tx = VersionedTransaction::try_new(
            tx.message,
            &[&read_keypair_file(&self.general_config.keypair_path).unwrap()],
        )?;

        TransactionSender::aggressive_send_tx(self.rpc_client.clone(), &tx, SenderCfg::DEFAULT)
            .map_err(|_| anyhow!("Failed to send swap transaction"))?;

        self.refresh_token_account(src_bank).await?;
        self.refresh_token_account(dst_bank).await?;

        Ok(())
    }

    pub fn get_max_withdraw_for_bank(&self, bank_pk: &Pubkey) -> anyhow::Result<(I80F48, bool)> {
        let free_collateral = self.get_free_collateral()?;
        let balance = self
            .liquidator_account
            .account_wrapper
            .get_balance_for_bank(bank_pk, self.banks.get(bank_pk).unwrap())?;
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

    pub async fn refresh_token_account(&mut self, bank_pk: &Pubkey) -> anyhow::Result<()> {
        let mint = self.banks.get(bank_pk).unwrap().bank.mint;

        let token_account_addresses = self
            .token_account_manager
            .get_address_for_mint(mint)
            .unwrap();

        let account = self
            .rpc_client
            .get_account_with_commitment(&token_account_addresses, CommitmentConfig::confirmed())?
            .value
            .ok_or_else(|| anyhow::anyhow!("Token account not found"))?;

        let mint = accessor::mint(&account.data);
        let balance = accessor::amount(&account.data);

        self.token_accounts.get_mut(&mint).unwrap().balance = balance;

        Ok(())
    }

    pub fn get_value(
        &self,
        amount: I80F48,
        bank_pk: &Pubkey,
        requirement_type: RequirementType,
        side: BalanceSide,
    ) -> anyhow::Result<I80F48> {
        let bank = self.banks.get(bank_pk).unwrap();
        let value = match side {
            BalanceSide::Assets => {
                calc_weighted_assets_new(bank, amount.to_num(), requirement_type)?
            }
            BalanceSide::Liabilities => {
                calc_weighted_liabs_new(bank, amount.to_num(), requirement_type)?
            }
        };
        Ok(value)
    }

    fn get_free_collateral(&self) -> anyhow::Result<I80F48> {
        let (assets, liabs) = self.calc_health(
            &self.liquidator_account.account_wrapper,
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
        let baws =
            BankAccountWithPriceFeedEva::load(&account.account.lending_account, self.banks.clone())
                .unwrap();

        baws.iter().fold(
            (I80F48::ZERO, I80F48::ZERO),
            |(total_assets, total_liabs), baw| {
                let (assets, liabs) = baw
                    .calc_weighted_assets_and_liabilities_values(requirement_type)
                    .unwrap();
                (total_assets + assets, total_liabs + liabs)
            },
        )
    }

    fn get_token_balance_for_bank(&self, bank_pk: &Pubkey) -> anyhow::Result<Option<I80F48>> {
        let mint = self.banks.get(bank_pk).unwrap().bank.mint;

        let balance = self
            .token_accounts
            .get(&mint)
            .map(|account| account.get_amount());

        Ok(balance)
    }

    pub fn get_amount(
        &self,
        value: I80F48,
        bank_pk: &Pubkey,
        price_bias: Option<PriceBias>,
    ) -> anyhow::Result<I80F48> {
        let bank = self.banks.get(bank_pk).unwrap();

        let price = bank.oracle_adapter.get_price_of_type(
            marginfi::state::price::OraclePriceType::RealTime,
            price_bias,
        )?;

        let amount_ui = value / price;

        Ok(amount_ui * EXP_10_I80F48[bank.bank.mint_decimals as usize])
    }
}
