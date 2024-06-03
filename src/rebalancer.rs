use crate::{
    config::{GeneralConfig, RebalancerCfg},
    geyser::{AccountType, GeyserUpdate},
    sender::{SenderCfg, TransactionSender},
    token_account_manager::TokenAccountManager,
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
use crossbeam::channel::Receiver;
use fixed::types::I80F48;
use fixed_macro::types::I80F48;
use jupiter_swap_api_client::{
    quote::QuoteRequest,
    swap::SwapRequest,
    transaction_config::{ComputeUnitPriceMicroLamports, TransactionConfig},
    JupiterSwapApiClient,
};
use log::info;
use marginfi::{
    constants::EXP_10_I80F48,
    state::{
        marginfi_account::{BalanceSide, MarginfiAccount, RequirementType},
        price::{OraclePriceFeedAdapter, PriceAdapter, PriceBias},
    },
};
use solana_client::rpc_client::RpcClient;
use solana_program::{pubkey, pubkey::Pubkey};
use solana_sdk::{
    account_info::IntoAccountInfo, commitment_config::CommitmentConfig,
    signature::read_keypair_file, transaction::VersionedTransaction,
};
use std::{
    cmp::min,
    collections::{HashMap, HashSet},
    sync::Arc,
};

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
    receiver: Receiver<GeyserUpdate>,
}

impl Rebalancer {
    pub fn new(
        general_config: GeneralConfig,
        config: RebalancerCfg,
        receiver: Receiver<GeyserUpdate>,
    ) -> anyhow::Result<Self> {
        let rpc_client = Arc::new(RpcClient::new(general_config.rpc_url.clone()));
        let token_account_manager = TokenAccountManager::new(rpc_client.clone())?;

        let liquidator_keypair = read_keypair_file(&general_config.keypair_path).unwrap();

        let liquidator_account = LiquidatorAccount::new(
            liquidator_keypair,
            RpcClient::new(general_config.rpc_url.clone()),
            general_config.liquidator_account,
        )
        .unwrap();

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
            receiver,
        })
    }

    pub fn load_data(
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
            .and_then(|bank| Some(bank.address.clone()));

        Ok(())
    }

    pub async fn start(&mut self) -> anyhow::Result<()> {
        info!("Rebalancer started");
        let max_duration = std::time::Duration::from_secs(15);
        loop {
            let start = std::time::Instant::now();
            while let Ok(mut msg) = self.receiver.recv() {
                match msg.account_type {
                    AccountType::OracleAccount => {
                        if let Some(bank_to_update_pk) = self.oracle_to_bank.get(&msg.address) {
                            let oracle_ai = (&msg.address, &mut msg.account).into_account_info();
                            let bank_to_update: &mut BankWrapper =
                                self.banks.get_mut(bank_to_update_pk).unwrap();

                            bank_to_update.oracle_adapter.price_adapter =
                                OraclePriceFeedAdapter::try_from_bank_config_with_max_age(
                                    &bank_to_update.bank.config,
                                    &[oracle_ai.clone()],
                                    0,
                                    u64::MAX,
                                )
                                .unwrap();
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

                if start.elapsed() > max_duration && self.needs_to_be_relanced() {
                    let _ = self.rebalance_accounts().await;
                    break;
                }
            }
        }
    }

    fn needs_to_be_relanced(&self) -> bool {
        self.has_tokens_in_token_accounts()
            || self.has_non_preferred_deposits()
            || self.has_liabilities()
    }

    async fn rebalance_accounts(&mut self) -> anyhow::Result<()> {
        self.sell_non_preferred_deposits().await?;
        self.repay_liabilities().await?;
        self.handle_tokens_in_token_accounts().await?;
        self.deposit_preferred_tokens().await?;

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

    async fn sell_non_preferred_deposits(&self) -> anyhow::Result<()> {
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

        let balance = self
            .liquidator_account
            .account_wrapper
            .get_balance_for_bank(&bank_pk, &bank)?;

        if matches!(balance, None) || matches!(balance, Some((_, BalanceSide::Assets))) {
            return Ok(());
        }

        let (liab_balance, _) = balance.unwrap();

        let token_balance = self
            .get_token_balance_for_bank(&bank_pk)?
            .unwrap_or_default();

        let liab_to_purchase = liab_balance - token_balance;

        if !liab_to_purchase.is_zero() {
            let liab_usd_value = self.get_value(
                liab_to_purchase,
                &bank_pk,
                RequirementType::Initial,
                BalanceSide::Liabilities,
            )?;

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

                self.liquidator_account.withdraw(
                    bank,
                    self.token_account_manager
                        .get_address_for_mint(bank.bank.mint)
                        .unwrap(),
                    withdraw_amount.to_num(),
                    Some(withdraw_all),
                    self.general_config.get_tx_config(),
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
                self.general_config.get_tx_config(),
            )?;
        }

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

        self.liquidator_account.deposit(
            bank,
            token_address,
            balance.to_num(),
            self.general_config.get_tx_config(),
        )?;

        Ok(())
    }

    fn has_tokens_in_token_accounts(&self) -> bool {
        let has_tokens_in_tas = self.token_accounts.values().any(|account| {
            let bank = self.banks.get(&account.bank_address).unwrap();
            let value = account.get_value(&bank).unwrap();
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
                    .and_then(|bank| Some(bank.bank.mint))
                    .unwrap();

                let has_non_preferred_deposits =
                    matches!(balance.get_side(), Some(BalanceSide::Assets))
                        && !self.preferred_mints.contains(&mint);

                has_non_preferred_deposits
            });

        has_non_preferred_deposits
    }

    fn has_liabilities(&self) -> bool {
        self.liquidator_account.account_wrapper.has_liabs()
    }

    async fn handle_tokens_in_token_accounts(&mut self) -> anyhow::Result<()> {
        let bank_addresses = self
            .banks
            .iter()
            .map(|e| e.0)
            .filter(|bank_pk| self.swap_mint_bank_pk.unwrap() != **bank_pk)
            .collect::<Vec<_>>();

        for bank_pk in bank_addresses {
            self.handle_token_in_token_account(bank_pk).await?;
        }

        self.refresh_token_account(&self.swap_mint_bank_pk.unwrap())
            .await?;

        let bank = self.banks.get(&self.swap_mint_bank_pk.unwrap()).unwrap();

        let balance = self
            .token_accounts
            .get(&bank.bank.mint)
            .and_then(|account| Some(account.get_amount()));

        if let Some(balance) = balance {
            if !balance.is_zero() {
                self.liquidator_account.deposit(
                    bank,
                    self.swap_mint_bank_pk.unwrap(),
                    balance.to_num(),
                    self.general_config.get_tx_config(),
                )?;
            }
        }

        Ok(())
    }

    async fn handle_token_in_token_account(&self, bank_pk: &Pubkey) -> anyhow::Result<()> {
        let bank = self.banks.get(bank_pk).unwrap();
        let amount = self
            .liquidator_account
            .account_wrapper
            .get_balance_for_bank(bank_pk, bank)?;

        if amount.is_none() {
            return Ok(());
        }

        let amount = amount.unwrap();

        let value = self.get_value(
            amount.0,
            &bank_pk,
            RequirementType::Equity,
            BalanceSide::Assets,
        )?;

        if value < self.config.token_account_dust_threshold {
            return Ok(());
        }

        self.swap(amount.0.to_num(), bank_pk, &self.swap_mint_bank_pk.unwrap())
            .await?;

        Ok(())
    }

    /// Withdraw and sells a given asset
    async fn withdraw_and_sell_deposit(&self, bank_pk: &Pubkey) -> anyhow::Result<()> {
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
            self.general_config.get_tx_config(),
            &self.banks,
        )?;

        self.swap(amount, bank_pk, &self.swap_mint_bank_pk.unwrap())
            .await?;

        Ok(())
    }

    async fn swap(&self, amount: u64, src_bank: &Pubkey, dst_bank: &Pubkey) -> anyhow::Result<()> {
        let src_mint = {
            let bank = self.banks.get(&src_bank).unwrap();

            bank.bank.mint
        };

        let dst_mint = {
            let bank = self.banks.get(&dst_bank).unwrap();

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
                        .map(|v| ComputeUnitPriceMicroLamports::MicroLamports(v)),
                    ..Default::default()
                },
            })
            .await?;

        let tx = bincode::deserialize::<VersionedTransaction>(&swap.swap_transaction)
            .map_err(|_| anyhow!("Failed to deserialize"))?;

        TransactionSender::aggressive_send_tx(self.rpc_client.clone(), &tx, SenderCfg::DEFAULT)
            .map_err(|_| anyhow!("Failed to send swap transaction"))?;

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
                    &bank_pk,
                    RequirementType::Initial,
                    BalanceSide::Assets,
                )?;

                (I80F48!(0), value <= free_collateral)
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
            .and_then(|account| Some(account.get_amount()));

        Ok(balance)
    }

    pub fn get_amount(
        &self,
        value: I80F48,
        bank_pk: &Pubkey,
        price_bias: Option<PriceBias>,
    ) -> anyhow::Result<I80F48> {
        let bank = self.banks.get(bank_pk).unwrap();

        let price = bank.oracle_adapter.price_adapter.get_price_of_type(
            marginfi::state::price::OraclePriceType::RealTime,
            price_bias,
        )?;

        let amount_ui = value / price;

        Ok(amount_ui * EXP_10_I80F48[bank.bank.mint_decimals as usize])
    }
}
