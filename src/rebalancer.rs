use crate::{
    config::{GeneralConfig, RebalancerCfg},
    crossbar::CrossbarMaintainer,
    geyser::{AccountType, GeyserUpdate},
    metrics::{update_balance, ERROR_COUNT},
    sender::{SenderCfg, TransactionSender},
    token_account_manager::TokenAccountManager,
    transaction_manager::{RawTransaction, TransactionData},
    utils::{
        accessor, batch_get_multiple_accounts, calc_weighted_assets_new, calc_weighted_liabs_new,
        clock::CachedClock, load_swb_pull_account_from_bytes, BankAccountWithPriceFeedEva,
        BatchLoadingConfig,
    },
    ward,
    wrappers::{
        bank::BankWrapper, liquidator_account::LiquidatorAccount,
        marginfi_account::MarginfiAccountWrapper, oracle::OracleWrapperTrait,
        token_account::TokenAccountWrapper,
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
use log::{debug, error, info, warn};
use marginfi::{
    constants::EXP_10_I80F48,
    state::{
        marginfi_account::{BalanceSide, MarginfiAccount, RequirementType},
        price::{OraclePriceFeedAdapter, OracleSetup, PriceBias, SwitchboardPullPriceFeed},
    },
};
use solana_client::rpc_client::RpcClient;
use solana_program::pubkey::Pubkey;
use solana_sdk::{
    account::Account, account_info::IntoAccountInfo, commitment_config::CommitmentConfig,
    signature::read_keypair_file, transaction::VersionedTransaction,
};
use std::{
    cmp::min,
    collections::{HashMap, HashSet},
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};
use switchboard_on_demand::PullFeedAccountData;
use switchboard_on_demand_client::{FetchUpdateManyParams, PullFeed, SbContext};
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
    cache_oracle_needed_accounts: HashMap<Pubkey, Account>,
}

impl Rebalancer {
    pub async fn new(
        general_config: GeneralConfig,
        config: RebalancerCfg,
        transaction_tx: Sender<TransactionData>,
        ack_rx: Receiver<Pubkey>,
        geyser_receiver: Receiver<GeyserUpdate>,
        stop_liquidations: Arc<AtomicBool>,
    ) -> anyhow::Result<Self> {
        let rpc_client = Arc::new(RpcClient::new(general_config.tx_landing_url.clone()));
        let token_account_manager = TokenAccountManager::new(rpc_client.clone())?;

        let liquidator_account = LiquidatorAccount::new(
            RpcClient::new(general_config.rpc_url.clone()),
            transaction_tx.clone(),
            ack_rx,
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
            stop_liquidations,
            crossbar_client: CrossbarMaintainer::new(),
            cache_oracle_needed_accounts: HashMap::new(),
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

        let all_keys = self
            .banks
            .values()
            .filter(|b| b.bank.config.oracle_setup == OracleSetup::StakedWithPythPush)
            .flat_map(|bank| {
                vec![
                    bank.bank.config.oracle_keys[1],
                    bank.bank.config.oracle_keys[2],
                ]
            })
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();

        let all_accounts = batch_get_multiple_accounts(
            self.rpc_client.clone(),
            &all_keys,
            BatchLoadingConfig::DEFAULT,
        )
        .unwrap();

        self.cache_oracle_needed_accounts = all_keys
            .into_iter()
            .zip(all_accounts.into_iter())
            .map(|(key, acc)| (key, acc.unwrap()))
            .collect();

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
            BatchLoadingConfig::DEFAULT,
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
                .unwrap()
                .clone();

            self.token_accounts.insert(
                **mint,
                TokenAccountWrapper {
                    address: **token_account_addresses,
                    balance,
                    bank,
                },
            );
        }

        self.swap_mint_bank_pk = self
            .get_bank_for_mint(&self.config.swap_mint)
            .map(|bank| bank.address);

        Ok(())
    }

    pub async fn start(&mut self) -> anyhow::Result<()> {
        let max_duration = std::time::Duration::from_secs(20);
        let rpc_client = RpcClient::new(self.general_config.rpc_url.clone());
        let mut start = std::time::Instant::now();
        let cached_clock = CachedClock::new(Duration::from_secs(1)); // Cache for 1 second

        while let Ok(mut msg) = self.geyser_receiver.recv() {
            info!(
                "Received geyser update: {:?} for {:?}",
                msg.account_type, msg.address
            );
            match msg.account_type {
                AccountType::Oracle => {
                    let bank_to_update_pk = ward!(self.oracle_to_bank.get(&msg.address), continue);
                    //debug!("Received oracle update for bank: {:?}", bank_to_update_pk);

                    let bank_to_update: &mut BankWrapper =
                        self.banks.get_mut(bank_to_update_pk).unwrap();

                    let oracle_price_adapter = match bank_to_update.bank.config.oracle_setup {
                        OracleSetup::SwitchboardPull => {
                            let mut offsets_data =
                                [0u8; std::mem::size_of::<PullFeedAccountData>()];
                            offsets_data.copy_from_slice(
                                &msg.account.data
                                    [8..std::mem::size_of::<PullFeedAccountData>() + 8],
                            );
                            let swb_feed = load_swb_pull_account_from_bytes(&offsets_data).unwrap();

                            let feed_hash = hex::encode(swb_feed.feed_hash);
                            bank_to_update.oracle_adapter.swb_feed_hash = Some(feed_hash);

                            OraclePriceFeedAdapter::SwitchboardPull(SwitchboardPullPriceFeed {
                                feed: Box::new((&swb_feed).into()),
                            })
                        }
                        OracleSetup::StakedWithPythPush => {
                            let clock =
                                ward!(cached_clock.get_clock(&rpc_client).await.ok(), continue);

                            let keys = &bank_to_update.bank.config.oracle_keys[1..3];

                            let mut accounts_info =
                                vec![(&msg.address, &mut msg.account).into_account_info()];

                            let mut owned_accounts: Vec<_> = keys
                                .iter()
                                .map(|key| {
                                    self.cache_oracle_needed_accounts
                                        .iter()
                                        .find(|(k, _)| *k == key)
                                        .unwrap()
                                        .1
                                        .clone()
                                })
                                .collect();

                            accounts_info.extend(
                                keys.iter()
                                    .zip(owned_accounts.iter_mut())
                                    .map(|(key, account)| (key, account).into_account_info()),
                            );

                            OraclePriceFeedAdapter::try_from_bank_config(
                                &bank_to_update.bank.config,
                                &accounts_info,
                                &clock,
                            )
                            .unwrap()
                        }
                        _ => {
                            let clock =
                                ward!(cached_clock.get_clock(&rpc_client).await.ok(), continue);
                            let oracle_account_info =
                                (&msg.address, &mut msg.account).into_account_info();
                            OraclePriceFeedAdapter::try_from_bank_config(
                                &bank_to_update.bank.config,
                                &[oracle_account_info],
                                &clock,
                            )
                            .unwrap()
                        }
                    };

                    bank_to_update.oracle_adapter.price_adapter = oracle_price_adapter;
                }
                AccountType::Marginfi => {
                    debug!("Received marginfi account update: {:?}", msg.address);
                    if msg.address == self.general_config.liquidator_account {
                        let marginfi_account =
                            bytemuck::from_bytes::<MarginfiAccount>(&msg.account.data[8..]);

                        self.liquidator_account.account_wrapper.lending_account =
                            marginfi_account.lending_account;
                    }
                }
                AccountType::Token => {
                    let mint = accessor::mint(&msg.account.data);
                    let balance = accessor::amount(&msg.account.data);
                    debug!(
                        "Received token account update: mint - {:?}, balance - {}",
                        mint, balance
                    );

                    let account_to_update = self.token_accounts.get_mut(&mint).unwrap();

                    account_to_update.balance = balance;
                    update_balance(
                        &account_to_update.bank.bank.mint.to_string(),
                        balance as f64,
                    )
                    .await;
                }
            }

            if start.elapsed() > max_duration && self.needs_to_be_relanced().await {
                if let Err(e) = self.rebalance_accounts().await {
                    error!("Failed to rebalance account: {:?}", e);
                    ERROR_COUNT.inc();
                }
                start = std::time::Instant::now();
                continue;
            }
        }
        Err(anyhow!("Rebalancer stopped"))
    }

    async fn needs_to_be_relanced(&mut self) -> bool {
        // Update switchboard pull prices with crossbar
        let swb_feed_hashes = self
            .banks
            .values()
            .filter_map(|bank| {
                bank.oracle_adapter
                    .swb_feed_hash
                    .as_ref()
                    .map(|feed_hash| (bank.address, feed_hash.clone()))
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
                SbContext::new(),
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
                debug!("SENDING Rebalancer SWB liquidate");
                self.liquidator_account
                    .transaction_tx
                    .send(TransactionData {
                        transactions: vec![RawTransaction::new(vec![ix]).with_lookup_tables(lut)],
                        ack_id: self.liquidator_account.account_wrapper.address,
                    })
                    .unwrap();
            }
        }

        debug!("Selling non-preferred deposits\n\n");
        self.sell_non_preferred_deposits().await?;

        debug!("Rebalancing: repaying liabilities\n\n");
        self.repay_liabilities().await?;

        debug!("Rebalancing: draining tokens from token accounts\n\n");
        self.drain_tokens_from_token_accounts().await?;

        debug!("Rebalancing: depositing preferred tokens\n\n");
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
            tracked_accounts.insert(token_account.address, AccountType::Token);
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
            .get_deposits(&self.config.preferred_mints, &self.banks);

        for bank_pk in non_preferred_deposits {
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

        // Get the balance for the liability and check if it's valid
        let balance = self
            .liquidator_account
            .account_wrapper
            .get_balance_for_bank(bank);

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
        debug!(
            "Liability {:?} needs to be repaid with {:?} USD",
            bank_pk, liab_usd_value
        );

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

            self.liquidator_account
                .withdraw(
                    bank,
                    self.token_account_manager
                        .get_address_for_mint(bank.bank.mint)
                        .unwrap(),
                    withdraw_amount.to_num(),
                    Some(withdraw_all),
                    &self.banks,
                )
                .await?;

            withdraw_amount
        } else {
            I80F48::ZERO
        };

        let amount_to_swap = min(liab_balance + withdraw_amount, required_swap_token);
        debug!(
            "SWAPPING {:?} of {:?} for {:?}",
            amount_to_swap,
            self.swap_mint_bank_pk.unwrap(),
            bank_pk
        );

        if amount_to_swap.is_positive() {
            self.swap(
                amount_to_swap.to_num(),
                &self.swap_mint_bank_pk.unwrap(),
                &bank_pk,
            )
            .await?;

            self.refresh_token_account(&bank_pk).await?;
        }

        debug!("REPAYING!!!");

        let token_balance = self
            .get_token_balance_for_bank(&bank_pk)?
            .unwrap_or_default();

        let repay_all = token_balance >= liab_balance;

        let bank = self.banks.get(&bank_pk).unwrap();

        self.liquidator_account
            .repay(
                bank,
                &self
                    .token_account_manager
                    .get_address_for_mint(bank.bank.mint)
                    .unwrap(),
                token_balance.to_num(),
                Some(repay_all),
            )
            .await?;

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
            .deposit(bank, token_address, balance.to_num())
            .await?;

        Ok(())
    }

    fn has_tokens_in_token_accounts(&self) -> bool {
        let has_tokens_in_tas = self.token_accounts.values().any(|account| {
            let value = account.get_value().unwrap();
            value > self.config.token_account_dust_threshold
        });
        has_tokens_in_tas
    }

    fn has_non_preferred_deposits(&self) -> bool {
        let has_non_preferred_deposits = self
            .liquidator_account
            .account_wrapper
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

    async fn drain_tokens_from_token_accounts(&mut self) -> anyhow::Result<()> {
        let token_accounts: Vec<TokenAccountWrapper> =
            self.token_accounts.values().cloned().collect();
        for account in token_accounts {
            if account.bank.bank.mint == self.config.swap_mint {
                continue;
            }

            let value = account.get_value().unwrap();

            if value > self.config.token_account_dust_threshold {
                self.swap(
                    account.get_amount().to_num(),
                    &account.bank.address,
                    &self.swap_mint_bank_pk.unwrap(),
                )
                .await?;
            }
        }

        Ok(())
    }

    /// Withdraw and sells a given asset
    async fn withdraw_and_sell_deposit(&mut self, bank_pk: &Pubkey) -> anyhow::Result<()> {
        let bank = self.banks.get(bank_pk).unwrap();
        let balance = self
            .liquidator_account
            .account_wrapper
            .get_balance_for_bank(bank);

        if !matches!(&balance, Some((_, BalanceSide::Assets))) {
            return Ok(());
        }

        let (withdraw_amount, withdrawl_all) = self.get_max_withdraw_for_bank(bank_pk)?;

        let amount = withdraw_amount.to_num::<u64>();

        debug!(
            "Withdrawing {:?} of {:?} from bank {:?}",
            amount, bank.bank.mint, bank_pk
        );
        self.liquidator_account
            .withdraw(
                bank,
                self.token_account_manager
                    .get_address_for_mint(bank.bank.mint)
                    .unwrap(),
                amount,
                Some(withdrawl_all),
                &self.banks,
            )
            .await?;

        debug!("Swapping");
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
        let input_mint = {
            let bank = self.banks.get(src_bank).unwrap();

            bank.bank.mint
        };

        let output_mint = {
            let bank = self.banks.get(dst_bank).unwrap();

            bank.bank.mint
        };

        let jup_swap_client = JupiterSwapApiClient::new(self.config.jup_swap_api_url.clone());

        let quote_response = jup_swap_client
            .quote(&QuoteRequest {
                input_mint,
                output_mint,
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
            .get_balance_for_bank(self.banks.get(bank_pk).unwrap());
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
        let baws = BankAccountWithPriceFeedEva::load(&account.lending_account, self.banks.clone())
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
