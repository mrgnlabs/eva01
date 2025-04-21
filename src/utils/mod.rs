use anyhow::{anyhow, Result};
use backoff::ExponentialBackoff;
use fixed::types::I80F48;
use log::debug;
use marginfi::{
    bank_authority_seed,
    constants::{PYTH_PUSH_MARGINFI_SPONSORED_SHARD_ID, PYTH_PUSH_PYTH_SPONSORED_SHARD_ID},
    prelude::MarginfiResult,
    state::{
        marginfi_account::{calc_value, Balance, BalanceSide, LendingAccount, RequirementType},
        marginfi_group::{Bank, BankConfig, BankVaultType, RiskTier},
        price::{PriceBias, PythPushOraclePriceFeed},
    },
};
use rayon::{iter::ParallelIterator, slice::ParallelSlice};
use serde::{ser::SerializeSeq, Deserialize, Deserializer, Serializer};
use solana_account_decoder::UiAccountEncoding;
use solana_client::rpc_config::RpcAccountInfoConfig;
use solana_program::pubkey::Pubkey;
use solana_sdk::{
    account::Account,
    signature::{read_keypair_file, Keypair},
};
use std::{
    collections::HashMap,
    io::Write,
    mem::MaybeUninit,
    path::PathBuf,
    str::FromStr,
    sync::{atomic::AtomicUsize, Arc},
};
use switchboard_on_demand::PullFeedAccountData;
use url::Url;
use yellowstone_grpc_proto::geyser::SubscribeUpdateAccountInfo;

use crate::wrappers::{bank::BankWrapper, oracle::OracleWrapperTrait};

pub struct BatchLoadingConfig {
    pub max_batch_size: usize,
    pub max_concurrent_calls: usize,
}

impl BatchLoadingConfig {
    pub const DEFAULT: Self = Self {
        max_batch_size: 100,
        max_concurrent_calls: 16,
    };
}

/// Batch load accounts from the RPC client using the getMultipleAccounts RPC call.
///
/// - `max_batch_size`: The maximum number of accounts to load in a single RPC call.
/// - `max_concurrent_calls`: The maximum number of concurrent RPC calls.
///
/// This function will perform multiple RPC calls concurrently, up to `max_concurrent_calls`.
/// If the number of pending RPC calls exceeds `max_concurrent_calls`, the function will
/// await until some calls complete before initiating more, to respect the concurrency limit.
/// Additionally, logs progress information including the number of accounts being fetched,
/// the size of each chunk, and the current progress using trace and debug logs.
pub fn batch_get_multiple_accounts(
    rpc_client: &solana_client::rpc_client::RpcClient,
    addresses: &[Pubkey],
    BatchLoadingConfig {
        max_batch_size,
        max_concurrent_calls,
    }: BatchLoadingConfig,
) -> anyhow::Result<Vec<Option<Account>>> {
    let batched_addresses = addresses.chunks(max_batch_size * max_concurrent_calls);
    let total_addresses = addresses.len();
    let total_batches = batched_addresses.len();

    let mut accounts = Vec::new();
    let fetched_accounts = Arc::new(AtomicUsize::new(0));

    for (batch_index, batch) in batched_addresses.enumerate() {
        let batch_size = batch.len();

        log::trace!(
            "Fetching batch {} / {} with {} addresses.",
            batch_index + 1,
            total_batches,
            batch_size
        );

        let mut batched_accounts = batch
            .par_chunks(max_batch_size)
            .map(|chunk| -> anyhow::Result<Vec<_>> {
                let chunk = chunk.to_vec();
                let chunk_size = chunk.len();

                log::trace!(" - Fetching chunk of size {}", chunk_size);

                let chunk_res = backoff::retry(ExponentialBackoff::default(), move || {
                    let chunk = chunk.clone();

                    rpc_client
                        .get_multiple_accounts_with_config(
                            &chunk,
                            RpcAccountInfoConfig {
                                encoding: Some(UiAccountEncoding::Base64Zstd),
                                ..Default::default()
                            },
                        )
                        .map_err(backoff::Error::transient)
                })?
                .value;

                let fetched_chunk_size = chunk_res.len();

                fetched_accounts
                    .fetch_add(fetched_chunk_size, std::sync::atomic::Ordering::Relaxed);

                log::trace!(
                    " - Fetched chunk with {} accounts. Progress: {} / {}",
                    fetched_chunk_size,
                    fetched_accounts.load(std::sync::atomic::Ordering::Relaxed),
                    total_addresses
                );

                Ok(chunk_res)
            })
            .collect::<Result<Vec<_>>>()?
            .iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();

        accounts.append(&mut batched_accounts);
    }

    log::debug!(
        "Finished fetching all accounts. Total accounts fetched: {}",
        fetched_accounts.load(std::sync::atomic::Ordering::Relaxed)
    );

    Ok(accounts)
}

// Field parsers to save compute. All account validation is assumed to be done
// outside these methods.
pub mod accessor {
    use super::*;

    pub fn amount(bytes: &[u8]) -> u64 {
        let mut amount_bytes = [0u8; 8];
        amount_bytes.copy_from_slice(&bytes[64..72]);
        u64::from_le_bytes(amount_bytes)
    }

    pub fn mint(bytes: &[u8]) -> Pubkey {
        let mut mint_bytes = [0u8; 32];
        mint_bytes.copy_from_slice(&bytes[..32]);
        Pubkey::new_from_array(mint_bytes)
    }
}

pub fn account_update_to_account(account_update: &SubscribeUpdateAccountInfo) -> Result<Account> {
    let SubscribeUpdateAccountInfo {
        lamports,
        owner,
        executable,
        rent_epoch,
        data,
        ..
    } = account_update;

    let owner = Pubkey::try_from(owner.clone())
        .map_err(|e| anyhow!("Invalid pubkey: {:?}, error: {:?}", owner, e))?;

    let account = Account {
        lamports: *lamports,
        data: data.clone(),
        owner,
        executable: *executable,
        rent_epoch: *rent_epoch,
    };

    Ok(account)
}

pub(crate) fn from_pubkey_string<'de, D>(deserializer: D) -> Result<Pubkey, D::Error>
where
    D: Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer)?;
    Pubkey::from_str(&s).map_err(serde::de::Error::custom)
}

pub(crate) fn from_option_vec_pubkey_string<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<Pubkey>>, D::Error>
where
    D: Deserializer<'de>,
{
    let s: Option<Vec<String>> = Deserialize::deserialize(deserializer)?;

    match s {
        Some(a) => Ok(Some(
            a.into_iter()
                .map(|s| Pubkey::from_str(&s).map_err(serde::de::Error::custom))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        None => Ok(None),
    }
}

pub(crate) fn fixed_from_float<'de, D>(deserializer: D) -> Result<I80F48, D::Error>
where
    D: Deserializer<'de>,
{
    let s: f64 = Deserialize::deserialize(deserializer)?;

    Ok(I80F48::from_num(s))
}

pub(crate) fn fixed_to_float<S>(i: &I80F48, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_f64(i.to_num::<f64>())
}

pub(crate) fn pubkey_to_str<S>(p: &Pubkey, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&p.to_string())
}

// TODO: The next functions can be done better

pub(crate) fn vec_pubkey_to_str<S>(ps: &Vec<Pubkey>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut seq = serializer.serialize_seq(Some(ps.len()))?;

    for pubkey in ps {
        seq.serialize_element(&pubkey.to_string())?;
    }

    seq.end()
}

pub(crate) fn vec_pubkey_to_option_vec_str<S>(
    v: &Option<Vec<Pubkey>>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match v {
        Some(pubkeys) => {
            let mut seq = serializer.serialize_seq(Some(pubkeys.len()))?;
            for pubkey in pubkeys {
                seq.serialize_element(&pubkey.to_string())?;
            }
            seq.end()
        }
        None => serializer.serialize_none(),
    }
}

pub(crate) fn from_vec_str_to_pubkey<'de, D>(deserializer: D) -> Result<Vec<Pubkey>, D::Error>
where
    D: Deserializer<'de>,
{
    let s: Vec<String> = Deserialize::deserialize(deserializer)?;
    s.into_iter()
        .map(|s| Pubkey::from_str(&s).map_err(serde::de::Error::custom))
        .collect()
}

pub struct BankAccountWithPriceFeedEva<'a> {
    pub bank: BankWrapper,
    balance: &'a Balance,
}

impl<'a> BankAccountWithPriceFeedEva<'a> {
    pub fn load(
        lending_account: &'a LendingAccount,
        banks: HashMap<Pubkey, BankWrapper>,
    ) -> anyhow::Result<Vec<BankAccountWithPriceFeedEva<'a>>> {
        let active_balances = lending_account
            .balances
            .iter()
            .filter(|balance| balance.is_active());

        active_balances
            .map(move |balance| {
                let bank = banks
                    .get(&balance.bank_pk)
                    .ok_or_else(|| anyhow::anyhow!("Bank {:?} not found", balance.bank_pk))?
                    .clone();

                Ok(BankAccountWithPriceFeedEva { bank, balance })
            })
            .collect::<Result<Vec<_>>>()
    }

    #[inline(always)]
    /// Calculate the value of the assets and liabilities of the account in the form of (assets, liabilities)
    ///
    /// Nuances:
    /// 1. Maintenance requirement is calculated using the real time price feed.
    /// 2. Initial requirement is calculated using the time weighted price feed, if available.
    /// 3. Initial requirement is discounted by the initial discount, if enabled and the usd limit is exceeded.
    /// 4. Assets are only calculated for collateral risk tier.
    /// 5. Oracle errors are ignored for deposits in isolated risk tier.
    pub fn calc_weighted_assets_and_liabilities_values(
        &self,
        requirement_type: RequirementType,
        print_logs: bool,
    ) -> anyhow::Result<(I80F48, I80F48)> {
        match self.balance.get_side() {
            Some(side) => match side {
                BalanceSide::Assets => Ok((
                    self.calc_weighted_assets(requirement_type, &self.bank.bank, print_logs)?,
                    I80F48::ZERO,
                )),
                BalanceSide::Liabilities => Ok((
                    I80F48::ZERO,
                    self.calc_weighted_liabs(requirement_type, &self.bank.bank, print_logs)?,
                )),
            },
            None => Ok((I80F48::ZERO, I80F48::ZERO)),
        }
    }

    #[inline(always)]
    fn calc_weighted_assets(
        &self,
        requirement_type: RequirementType,
        bank: &Bank,
        print_logs: bool,
    ) -> anyhow::Result<I80F48> {
        match bank.config.risk_tier {
            RiskTier::Collateral => {
                let oracle_adapter = &self.bank.oracle_adapter;
                let mut asset_weight = bank
                    .config
                    .get_weight(requirement_type, BalanceSide::Assets);

                let lower_price = oracle_adapter.get_price_of_type(
                    requirement_type.get_oracle_price_type(),
                    Some(PriceBias::Low),
                )?;

                if matches!(requirement_type, RequirementType::Initial) {
                    if let Some(discount) =
                        bank.maybe_get_asset_weight_init_discount(lower_price)?
                    {
                        asset_weight = asset_weight
                            .checked_mul(discount)
                            .ok_or_else(|| anyhow!("math error"))?;
                    }
                }

                if print_logs {
                    let high_price = oracle_adapter
                        .get_price_of_type(
                            requirement_type.get_oracle_price_type(),
                            Some(PriceBias::High),
                        )
                        .unwrap();
                    debug!("Asset mint: {:?}, weight: {:?}, low_price: {:?}, high price: {:?}, shares: {:?}, share_value: {:?}", bank.mint, asset_weight, lower_price, high_price, self.balance.asset_shares, bank.asset_share_value);
                }

                Ok(calc_value(
                    bank.get_asset_amount(self.balance.asset_shares.into())?,
                    lower_price,
                    bank.mint_decimals,
                    Some(asset_weight),
                )?)
            }
            RiskTier::Isolated => Ok(I80F48::ZERO),
        }
    }

    #[inline(always)]
    fn calc_weighted_liabs(
        &self,
        requirement_type: RequirementType,
        bank: &Bank,
        print_logs: bool,
    ) -> MarginfiResult<I80F48> {
        let oracle_adapter = &self.bank.oracle_adapter;

        let liability_weight = bank
            .config
            .get_weight(requirement_type, BalanceSide::Liabilities);

        let higher_price = oracle_adapter
            .get_price_of_type(
                requirement_type.get_oracle_price_type(),
                Some(PriceBias::High),
            )
            .unwrap();

        if print_logs {
            let low_price = oracle_adapter
                .get_price_of_type(
                    requirement_type.get_oracle_price_type(),
                    Some(PriceBias::Low),
                )
                .unwrap();
            debug!("Liability mint: {:?}, weight: {:?}, low_price: {:?}, high price: {:?}, shares: {:?}, share_value: {:?}", bank.mint, liability_weight, low_price, higher_price, self.balance.liability_shares, bank.liability_share_value);
        }

        calc_value(
            bank.get_liability_amount(self.balance.liability_shares.into())?,
            higher_price,
            bank.mint_decimals,
            Some(liability_weight),
        )
    }
}

pub fn find_bank_liquidity_vault_authority(bank_pk: &Pubkey, program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        bank_authority_seed!(BankVaultType::Liquidity, bank_pk),
        program_id,
    )
    .0
}

pub fn calc_weighted_assets_new(
    bank: &BankWrapper,
    amount: I80F48,
    requirement_type: RequirementType,
) -> anyhow::Result<I80F48> {
    let oracle_adapter = &bank.oracle_adapter;
    let mut asset_weight = bank
        .bank
        .config
        .get_weight(requirement_type, BalanceSide::Assets);

    let price_bias = if matches!(requirement_type, RequirementType::Equity) {
        None
    } else {
        Some(PriceBias::Low)
    };

    let lower_price =
        oracle_adapter.get_price_of_type(requirement_type.get_oracle_price_type(), price_bias)?;

    if matches!(requirement_type, RequirementType::Initial) {
        if let Some(discount) = bank
            .bank
            .maybe_get_asset_weight_init_discount(lower_price)?
        {
            asset_weight = asset_weight
                .checked_mul(discount)
                .ok_or_else(|| anyhow!("math error"))?;
        }
    }

    Ok(calc_value(
        amount,
        lower_price,
        bank.bank.mint_decimals,
        Some(asset_weight),
    )?)
}

#[inline(always)]
pub fn calc_weighted_liabs_new(
    bank: &BankWrapper,
    amount: I80F48,
    requirement_type: RequirementType,
) -> anyhow::Result<I80F48> {
    let liability_weight = bank
        .bank
        .config
        .get_weight(requirement_type, BalanceSide::Liabilities);

    let price_bias = if matches!(requirement_type, RequirementType::Equity) {
        None
    } else {
        Some(PriceBias::High)
    };

    let higher_price = bank
        .oracle_adapter
        .get_price_of_type(requirement_type.get_oracle_price_type(), price_bias)?;

    Ok(calc_value(
        amount,
        higher_price,
        bank.bank.mint_decimals,
        Some(liability_weight),
    )?)
}

pub fn find_oracle_keys(bank_config: &BankConfig) -> Vec<Pubkey> {
    match bank_config.oracle_setup {
        marginfi::state::price::OracleSetup::PythPushOracle => {
            let feed_id = bank_config.get_pyth_push_oracle_feed_id().unwrap();
            vec![
                PythPushOraclePriceFeed::find_oracle_address(
                    PYTH_PUSH_MARGINFI_SPONSORED_SHARD_ID,
                    feed_id,
                )
                .0,
                PythPushOraclePriceFeed::find_oracle_address(
                    PYTH_PUSH_PYTH_SPONSORED_SHARD_ID,
                    feed_id,
                )
                .0,
            ]
        }
        marginfi::state::price::OracleSetup::StakedWithPythPush => {
            let feed_id = bank_config.get_pyth_push_oracle_feed_id().unwrap();
            let oracle_addresses = vec![
                PythPushOraclePriceFeed::find_oracle_address(
                    PYTH_PUSH_PYTH_SPONSORED_SHARD_ID,
                    feed_id,
                )
                .0,
                bank_config.oracle_keys[1],
                bank_config.oracle_keys[2],
            ];
            oracle_addresses
        }
        _ => bank_config
            .oracle_keys
            .iter()
            .filter_map(|key| {
                if *key != Pubkey::default() {
                    Some(*key)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>(),
    }
}

pub fn load_swb_pull_account_from_bytes(bytes: &[u8]) -> anyhow::Result<PullFeedAccountData> {
    if bytes
        .as_ptr()
        .align_offset(std::mem::align_of::<PullFeedAccountData>())
        != 0
    {
        return Err(anyhow::anyhow!("Invalid alignment"));
    }

    let num = bytes.len() / std::mem::size_of::<PullFeedAccountData>();
    let mut vec: Vec<MaybeUninit<PullFeedAccountData>> = Vec::with_capacity(num);

    unsafe {
        vec.set_len(num);
        std::ptr::copy_nonoverlapping(
            bytes[..std::mem::size_of::<PullFeedAccountData>()].as_ptr(),
            vec.as_mut_ptr() as *mut u8,
            bytes.len(),
        );

        let vec: Vec<PullFeedAccountData> = std::mem::transmute::<
            Vec<MaybeUninit<PullFeedAccountData>>,
            Vec<PullFeedAccountData>,
        >(vec);

        Ok(vec[0])
    }
}

pub fn expand_tilde(path: &str) -> PathBuf {
    if path.starts_with("~") {
        if let Some(home) = dirs::home_dir() {
            return home.join(&path[2..]);
        }
    }
    PathBuf::from(path)
}

pub fn is_valid_url(input: &str) -> bool {
    Url::parse(input).is_ok()
}

pub fn prompt_user(prompt_text: &str) -> anyhow::Result<String> {
    print!("{}", prompt_text);
    let mut input = String::new();
    std::io::stdout().flush()?;
    std::io::stdin().read_line(&mut input)?;
    input.pop();
    Ok(input)
}

/// Simply asks the keypair path until it is a valid one,
/// Returns (keypair_path, signer_keypair)
pub fn ask_keypair_until_valid() -> anyhow::Result<(PathBuf, Keypair)> {
    loop {
        let keypair_path = expand_tilde(&prompt_user("Keypair file path [required]: ")?);
        match read_keypair_file(&keypair_path) {
            Ok(keypair) => return Ok((keypair_path, keypair)),
            Err(_) => {
                println!("Failed to load the keypair from the provided path. Please try again");
            }
        }
    }
}

#[macro_export]
macro_rules! ward {
    ($res:expr) => {
        match $res {
            Some(value) => value,
            None => return,
        }
    };
    ($res:expr, break) => {
        match $res {
            Some(value) => value,
            None => break,
        }
    };
    ($res:expr, continue) => {
        match $res {
            Some(value) => value,
            None => continue,
        }
    };
}

#[macro_export]
macro_rules! thread_debug {
    ($($arg:tt)*) => {
        log::debug!(
            "Thread {:?}. {}",
            std::thread::current().id(),
            format_args!($($arg)*)
        )
    };
}

#[macro_export]
macro_rules! thread_info {
    ($($arg:tt)*) => {
        log::info!(
            "Thread {:?}. {}",
            std::thread::current().id(),
            format_args!($($arg)*)
        )
    };
}

#[macro_export]
macro_rules! thread_warn {
    ($($arg:tt)*) => {
        log::warn!(
            "Thread {:?}. {}",
            std::thread::current().id(),
            format_args!($($arg)*)
        )
    };
}

#[macro_export]
macro_rules! thread_error {
    ($($arg:tt)*) => {
        log::error!(
            "Thread {:?}. {}",
            std::thread::current().id(),
            format_args!($($arg)*)
        )
    };
}
