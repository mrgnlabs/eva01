use crate::{
    cache::Cache,
    config::{Eva01Config, TokenThresholds},
    utils::{self, find_oracle_keys, swb_cranker::SwbCranker},
    wrappers::{
        liquidator_account::LiquidatorAccount, marginfi_account::MarginfiAccountWrapper,
        oracle::OracleWrapper, token_account::TokenAccountWrapper,
    },
};
use fixed::types::I80F48;
use fixed_macro::types::I80F48;
use jupiter_swap_api_client::{
    quote::QuoteRequest,
    swap::SwapRequest,
    transaction_config::{ComputeUnitPriceMicroLamports, TransactionConfig},
    JupiterSwapApiClient,
};
use log::{error, info};
use marginfi_type_crate::types::OracleSetup;
use solana_client::{rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig};
use solana_program::pubkey::Pubkey;
use solana_sdk::{
    account::ReadableAccount, commitment_config::CommitmentLevel, signature::Keypair,
    transaction::VersionedTransaction,
};
use solana_sdk::{commitment_config::CommitmentConfig, signature::Signer};
use std::{collections::HashMap, sync::Arc};
use tokio::runtime::{Builder, Runtime};

const SLIPPAGE_MULTIPLIER: I80F48 = I80F48!(1.05);

/// The rebalancer is responsible to maintain the appropriate amounts of tokens on token accounts.
/// Guided primarily by token_thresholds and specific requests from the liquidator.
pub struct Rebalancer {
    signer: Keypair,
    liquidator_account: Arc<LiquidatorAccount>,
    swb_cranker: SwbCranker,
    rpc_client: RpcClient,
    swap_mint: Pubkey,
    swap_mint_bank: Pubkey,
    jup_swap_api_url: String,
    slippage_bps: u16,
    compute_unit_price_micro_lamports: ComputeUnitPriceMicroLamports,
    tokio_rt: Runtime,
    cache: Arc<Cache>,
    default_token_max_threshold: I80F48,
    token_thresholds: HashMap<Pubkey, TokenThresholds>,
}

impl Rebalancer {
    pub fn new(
        config: Eva01Config,
        liquidator_account: Arc<LiquidatorAccount>,
        cache: Arc<Cache>,
    ) -> anyhow::Result<Self> {
        let signer = Keypair::from_bytes(&config.wallet_keypair)?;

        let rpc_client =
            RpcClient::new_with_commitment(&config.rpc_url, CommitmentConfig::confirmed());

        let swap_mint = config.swap_mint;
        let swap_mint_bank = cache.banks.try_get_account_for_mint(&config.swap_mint)?;

        let jup_swap_api_url = config.jup_swap_api_url.clone();
        let slippage_bps = config.slippage_bps;
        let compute_unit_price_micro_lamports =
            ComputeUnitPriceMicroLamports::MicroLamports(config.compute_unit_price_micro_lamports);

        let tokio_rt = Builder::new_multi_thread()
            .thread_name("rebalancer")
            .worker_threads(2)
            .enable_all()
            .build()?;

        let swb_cranker = SwbCranker::new(&config)?;
        let default_token_max_threshold = config.default_token_max_threshold;
        let token_thresholds = config.token_thresholds;

        Ok(Self {
            signer,
            liquidator_account,
            swb_cranker,
            rpc_client,
            swap_mint,
            swap_mint_bank,
            jup_swap_api_url,
            slippage_bps,
            compute_unit_price_micro_lamports,
            tokio_rt,
            cache,
            default_token_max_threshold,
            token_thresholds,
        })
    }

    pub fn run(&mut self, missing_tokens: HashMap<Pubkey, I80F48>) -> anyhow::Result<()> {
        info!("Running the Rebalancing process...");

        // TODO: think if this is still necessary?
        if let Err(error) = self.crank_active_swb_oracles() {
            error!(
                "Failed to crank Swb Oracles for the Liquidator banks: {}",
                error
            )
        }

        // TODO: expand directly in this function?
        self.handle_token_accounts(missing_tokens)
            .map_err(|e| error!("Failed to handle the Liquidator's tokens! {}", e))
            .unwrap_or_default();

        if let Err(error) = self.deposit_preferred_token() {
            error!("Failed to deposit preferred token: {}", error)
        }

        info!("The Rebalancing process is complete.");

        Ok(())
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

    fn handle_token_accounts(
        &mut self,
        mut missing_tokens: HashMap<Pubkey, I80F48>,
    ) -> anyhow::Result<()> {
        let necessary_swap_value =
            self.sell_excessive_tokens_and_calculate_necessary_swap_value(&mut missing_tokens)?;

        let swap_token_address = self.cache.tokens.try_get_token_for_mint(&self.swap_mint)?;
        let swap_wrapper = self
            .cache
            .try_get_token_wrapper::<OracleWrapper>(&self.swap_mint, &swap_token_address)?;
        let existing_swap_value = swap_wrapper.get_value()?;

        if necessary_swap_value > existing_swap_value {
            let swap_bank_wrapper = self.cache.banks.try_get_bank(&self.swap_mint_bank)?;

            // Withdraw 5% more to account for slippage and price changes
            let amount = swap_wrapper
                .get_amount_from_value(necessary_swap_value - existing_swap_value)?
                .checked_mul(SLIPPAGE_MULTIPLIER)
                .unwrap();
            self.liquidator_account
                .withdraw(&swap_bank_wrapper, amount.to_num(), false)?;
        }

        self.buy_missing_tokens(swap_wrapper, missing_tokens)
    }

    fn sell_excessive_tokens_and_calculate_necessary_swap_value(
        &mut self,
        missing_tokens: &mut HashMap<Pubkey, I80F48>,
    ) -> anyhow::Result<I80F48> {
        let mut necessary_swap_value = I80F48::ZERO;
        for mint in self.cache.mints.get_mints() {
            if mint == self.swap_mint {
                continue;
            }

            let token = self.cache.tokens.try_get_token_for_mint(&mint)?;
            let wrapper = self
                .cache
                .try_get_token_wrapper::<OracleWrapper>(&mint, &token)?;

            if let Some(&amount) = missing_tokens.get(&wrapper.bank_wrapper.address) {
                let value_to_swap = wrapper.get_value_for_amount(amount)?;
                *(missing_tokens
                    .get_mut(&wrapper.bank_wrapper.address)
                    .unwrap()) = value_to_swap.checked_mul(SLIPPAGE_MULTIPLIER).unwrap();
                necessary_swap_value += value_to_swap;
                continue;
            }

            let value = wrapper.get_value()?;
            let min_value = self
                .token_thresholds
                .get(&mint)
                .map(|t| t.min_value)
                .unwrap_or(I80F48::ZERO);
            let max_value = self
                .token_thresholds
                .get(&mint)
                .map(|t| t.max_value)
                .unwrap_or(self.default_token_max_threshold);

            if value > max_value {
                info!("The value of {} tokens is higher than set threshold: {} > {}. Selling ${} worth of tokens.", mint, value.to_num::<f64>(), max_value.to_num::<f64>(), (value - max_value * 2).to_num::<f64>());
                let amount_to_swap = wrapper.get_amount_from_value(value - max_value * 2)?;
                let swapped_amount = self.swap(amount_to_swap.to_num(), mint, self.swap_mint)?;
                info!("Got {} back from the swap.", swapped_amount);
            } else if value < min_value {
                info!("The value of {} tokens is lower than set threshold: {} < {}. Will buy ${} worth of tokens.", mint, value.to_num::<f64>(), min_value.to_num::<f64>(), min_value.to_num::<f64>());
                missing_tokens.insert(wrapper.bank_wrapper.address, min_value);
                necessary_swap_value += min_value;
            }
        }
        Ok(necessary_swap_value)
    }

    fn buy_missing_tokens(
        &mut self,
        swap_token_wrapper: TokenAccountWrapper<OracleWrapper>,
        missing_tokens: HashMap<Pubkey, I80F48>,
    ) -> anyhow::Result<()> {
        for mint in self.cache.mints.get_mints() {
            if mint == self.swap_mint {
                continue;
            }

            let token = self.cache.tokens.try_get_token_for_mint(&mint)?;
            let wrapper = self
                .cache
                .try_get_token_wrapper::<OracleWrapper>(&mint, &token)?;

            if let Some(&value_to_swap) = missing_tokens.get(&wrapper.bank_wrapper.address) {
                let amount_to_swap = swap_token_wrapper.get_amount_from_value(value_to_swap)?;
                self.swap(amount_to_swap.to_num(), self.swap_mint, mint)?;
            }
        }
        Ok(())
    }

    fn deposit_preferred_token(&self) -> anyhow::Result<()> {
        let amount = self
            .get_token_balance_for_mint(&self.swap_mint)
            .unwrap_or_default();

        let thresholds = self.token_thresholds.get(&self.swap_mint).unwrap();
        if amount < thresholds.max_value {
            return Ok(());
        }

        // Leave the half of the max value on token acc
        let amount = (amount
            - thresholds
                .max_value
                .checked_mul(I80F48::from_num(0.5))
                .unwrap())
        .to_num();

        info!(
            "Depositing {} of preferred token to the Swap mint bank {:?}.",
            amount, &self.swap_mint_bank
        );

        let bank_wrapper = self.cache.banks.try_get_bank(&self.swap_mint_bank)?;
        if let Err(error) = self.liquidator_account.deposit(&bank_wrapper, amount) {
            error!(
                "Failed to deposit to the Bank ({:?}): {:?}",
                &self.swap_mint_bank, error
            );
        }

        Ok(())
    }

    fn swap(&self, amount: u64, input_mint: Pubkey, output_mint: Pubkey) -> anyhow::Result<u64> {
        if input_mint == output_mint {
            return Err(anyhow::anyhow!(
                "Jupiter swap failed: input and output mints cannot be the same: {:?}",
                input_mint
            ));
        }
        info!("Jupiter swap: {} -> {}", input_mint, output_mint);
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
                    wrap_and_unwrap_sol: false,
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

        info!(
            "Swapping unscaled {} tokens of mint {} to {} tokens of mint {} ...",
            amount, input_mint, out_amount, output_mint
        );

        let sig = self
            .rpc_client
            .send_and_confirm_transaction_with_spinner_and_config(
                &tx,
                CommitmentConfig::finalized(),
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    preflight_commitment: Some(CommitmentLevel::Processed),
                    ..Default::default()
                },
            )?;
        info!("The swap txn is finalized. Sig: {:?}", sig);

        Ok(out_amount)
    }

    fn get_token_balance_for_mint(&self, mint_address: &Pubkey) -> Option<I80F48> {
        let token_account_address = self.cache.tokens.get_token_for_mint(mint_address)?;
        match self.cache.tokens.try_get_account(&token_account_address) {
            Ok(account) => match utils::accessor::amount(account.data()) {
                Ok(amount) => Some(I80F48::from_num(amount)),
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
