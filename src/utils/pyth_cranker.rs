//! Keeps Pyth push-oracle accounts fresh when their sponsor stops pushing.
//!
//! marginfi prices a Pyth bank from the `PriceUpdateV2` account at
//! `PDA([shard_id, feed_id], pyth-push-oracle)`, and rejects it once it is older than the bank's
//! `oracle_max_age`. Normally Pyth (shard 0) or marginfi (shard 3301) sponsors those updates; if
//! that stops, this service posts the update itself: pull the signed price from Hermes, post its
//! VAA to the Wormhole core bridge, then `update_price_feed` on the push-oracle program.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use anchor_lang::{prelude::borsh, AccountDeserialize};
use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use log::{debug, info, warn};
use marginfi_type_crate::constants::{MARGINFI_SPONSORED_SHARD_ID, PYTH_SPONSORED_SHARD_ID};
use pyth_solana_receiver_sdk::{
    pda::{get_config_address, get_treasury_address},
    price_update::PriceUpdateV2,
    PostUpdateParams, PYTH_PUSH_ORACLE_ID,
};
use pythnet_sdk::wire::v1::{AccumulatorUpdateData, MerklePriceUpdate, Proof};
use reqwest::blocking::Client;
use serde::Deserialize;
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey,
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    transaction::Transaction,
};
use solana_sdk_ids::system_program;
use solana_system_interface::instruction as system_instruction;

use crate::{cache::Cache, clock_manager, config::Eva01Config};

const WORMHOLE_PROGRAM_ID: Pubkey = pubkey!("worm2ZoG2kUd4vFXhvjh93UUH596ayRfgQ2MgjNMTth");
pub const DEFAULT_HERMES_URL: &str = "https://hermes.pyth.network";

/// How often the cached price accounts are checked for staleness (in-memory, no RPC).
const CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// Crank once a feed has burned this much of its `oracle_max_age`, so it is refreshed before it
/// actually goes stale rather than after evaluations have already started failing.
const STALE_FRACTION: f64 = 0.5;

/// Anchor `global:<name>` discriminators of the instructions this builds.
const IX_INIT_ENCODED_VAA: [u8; 8] = [209, 193, 173, 25, 91, 202, 181, 218];
const IX_WRITE_ENCODED_VAA: [u8; 8] = [199, 208, 110, 177, 150, 76, 118, 42];
const IX_VERIFY_ENCODED_VAA_V1: [u8; 8] = [103, 56, 177, 229, 240, 103, 68, 73];
const IX_CLOSE_ENCODED_VAA: [u8; 8] = [48, 221, 174, 198, 231, 7, 152, 38];
const IX_UPDATE_PRICE_FEED: [u8; 8] = [28, 9, 93, 150, 86, 153, 188, 115];

/// Header the Wormhole account keeps in front of the VAA bytes.
const VAA_HEADER_SIZE: usize = 46;
/// The VAA is written in two chunks; the first is as large as fits alongside create + init.
const VAA_SPLIT_INDEX: usize = 721;
const DEFAULT_TREASURY_ID: u8 = 0;

#[derive(Deserialize)]
struct HermesResponse {
    binary: HermesBinary,
}

#[derive(Deserialize)]
struct HermesBinary {
    data: Vec<String>,
}

/// A push-oracle account that needs a new price, and the coordinates to write it.
struct StaleFeed {
    oracle: Pubkey,
    feed_id: [u8; 32],
    shard_id: u16,
    age: i64,
    max_age: u64,
}

pub struct PythCranker {
    hermes_url: String,
    hermes_api_key: Option<String>,
    http_client: Client,
    rpc_client: RpcClient,
    payer: Keypair,
    cache: Arc<Cache>,
    stop: Arc<AtomicBool>,
}

impl PythCranker {
    pub fn new(config: &Eva01Config, cache: Arc<Cache>, stop: Arc<AtomicBool>) -> Result<Self> {
        Ok(Self {
            hermes_url: config.pyth_hermes_url.clone(),
            hermes_api_key: config.pyth_api_key.clone(),
            http_client: Client::new(),
            rpc_client: RpcClient::new_with_commitment(
                config.rpc_url.clone(),
                CommitmentConfig::confirmed(),
            ),
            payer: Keypair::try_from(config.wallet_keypair.as_slice())?,
            cache,
            stop,
        })
    }

    pub fn start(&self) {
        info!("PythCranker starting.");
        while !self.stop.load(Ordering::Relaxed) {
            match self.stale_feeds() {
                Ok(feeds) => {
                    for feed in feeds {
                        warn!(
                            "Pyth feed {} is {}s old (max {}s): the sponsor stopped, cranking it.",
                            feed.oracle, feed.age, feed.max_age
                        );
                        if let Err(e) = self.crank(&feed) {
                            warn!("PythCranker: failed to crank {}: {e}", feed.oracle);
                        }
                    }
                }
                Err(e) => warn!("PythCranker: staleness check failed: {e}"),
            }
            thread::sleep(CHECK_INTERVAL);
        }
        info!("PythCranker stopped.");
    }

    /// Push-oracle accounts that have burned most of their max age, with the feed id read from the
    /// account itself and the shard whose PDA matches the bank's configured oracle.
    fn stale_feeds(&self) -> Result<Vec<StaleFeed>> {
        let now = clock_manager::get_clock(&self.cache.clock)?.unix_timestamp;
        let mut feeds = Vec::new();

        for (oracle, max_age) in self.cache.banks.get_pyth_push_oracles() {
            let account = match self.cache.oracles.try_get_account(&oracle) {
                Ok(account) => account,
                Err(_) => continue,
            };
            let price_update = match PriceUpdateV2::try_deserialize(&mut account.data.as_slice()) {
                Ok(price_update) => price_update,
                Err(_) => continue,
            };

            let age = now.saturating_sub(price_update.price_message.publish_time);
            if (age as f64) < (max_age as f64) * STALE_FRACTION {
                continue;
            }

            let feed_id = price_update.price_message.feed_id;
            let Some(shard_id) = self.shard_for(&oracle, &feed_id) else {
                debug!("Pyth feed {oracle} is on an unknown shard, skipping the crank");
                continue;
            };
            feeds.push(StaleFeed {
                oracle,
                feed_id,
                shard_id,
                age,
                max_age,
            });
        }

        Ok(feeds)
    }

    /// The sponsored shard whose PDA is the bank's oracle account.
    fn shard_for(&self, oracle: &Pubkey, feed_id: &[u8; 32]) -> Option<u16> {
        [PYTH_SPONSORED_SHARD_ID, MARGINFI_SPONSORED_SHARD_ID]
            .into_iter()
            .find(|shard_id| &price_feed_address(*shard_id, feed_id) == oracle)
    }

    /// Latest signed update for a feed, as the accumulator payload Hermes serves.
    fn fetch_update(&self, feed_id: &[u8; 32]) -> Result<AccumulatorUpdateData> {
        let url = format!(
            "{}/v2/updates/price/latest?ids[]=0x{}&encoding=base64",
            self.hermes_url.trim_end_matches('/'),
            hex_encode(feed_id)
        );
        let mut request = self.http_client.get(&url);
        if let Some(api_key) = &self.hermes_api_key {
            request = request.bearer_auth(api_key);
        }
        let response: HermesResponse = request.send()?.error_for_status()?.json()?;
        let encoded = response
            .binary
            .data
            .first()
            .ok_or_else(|| anyhow!("Hermes returned no update for the feed"))?;

        AccumulatorUpdateData::try_from_slice(&BASE64.decode(encoded)?)
            .map_err(|e| anyhow!("Failed to parse the Hermes accumulator update: {e:?}"))
    }

    /// Post the VAA, update the feed account, then reclaim the VAA account's rent. Two transactions
    /// because the VAA doesn't fit in one, and the second depends on the first having landed.
    fn crank(&self, feed: &StaleFeed) -> Result<()> {
        let update = self.fetch_update(&feed.feed_id)?;
        let Proof::WormholeMerkle { vaa, updates } = update.proof;
        let vaa: Vec<u8> = vaa.into();
        let merkle_price_update = updates
            .first()
            .ok_or_else(|| anyhow!("Hermes update carried no merkle price update"))?
            .clone();

        let payer = self.payer.pubkey();
        let encoded_vaa = Keypair::new();
        let vaa_account_size = VAA_HEADER_SIZE + vaa.len();
        let lamports = self
            .rpc_client
            .get_minimum_balance_for_rent_exemption(vaa_account_size)?;

        // A VAA carrying the full guardian set doesn't fit in one transaction alongside the
        // account creation, so the tail is written in the second one.
        let split = VAA_SPLIT_INDEX.min(vaa.len());
        let post = vec![
            system_instruction::create_account(
                &payer,
                &encoded_vaa.pubkey(),
                lamports,
                vaa_account_size as u64,
                &WORMHOLE_PROGRAM_ID,
            ),
            init_encoded_vaa_ix(&payer, &encoded_vaa.pubkey()),
            write_encoded_vaa_ix(&payer, &encoded_vaa.pubkey(), 0, &vaa[..split]),
        ];
        self.send(&post, &[&self.payer, &encoded_vaa])?;

        let mut update = Vec::new();
        if split < vaa.len() {
            update.push(write_encoded_vaa_ix(
                &payer,
                &encoded_vaa.pubkey(),
                split as u32,
                &vaa[split..],
            ));
        }
        update.extend([
            verify_encoded_vaa_ix(&payer, &encoded_vaa.pubkey(), guardian_set_index(&vaa)?),
            update_price_feed_ix(
                &payer,
                &encoded_vaa.pubkey(),
                feed.shard_id,
                feed.feed_id,
                merkle_price_update,
            )?,
            close_encoded_vaa_ix(&payer, &encoded_vaa.pubkey()),
        ]);
        let signature = self.send(&update, &[&self.payer])?;

        info!(
            "PythCranker: cranked the Pyth feed {} (shard {}): {}",
            feed.oracle, feed.shard_id, signature
        );
        Ok(())
    }

    fn send(&self, instructions: &[Instruction], signers: &[&Keypair]) -> Result<String> {
        let blockhash = self.rpc_client.get_latest_blockhash()?;
        let tx = Transaction::new_signed_with_payer(
            instructions,
            Some(&self.payer.pubkey()),
            signers,
            blockhash,
        );
        Ok(self
            .rpc_client
            .send_and_confirm_transaction(&tx)?
            .to_string())
    }
}

fn price_feed_address(shard_id: u16, feed_id: &[u8; 32]) -> Pubkey {
    Pubkey::find_program_address(&[&shard_id.to_le_bytes(), feed_id], &PYTH_PUSH_ORACLE_ID).0
}

/// Guardian set that signed the VAA: bytes 1..5 of the header, big-endian.
fn guardian_set_index(vaa: &[u8]) -> Result<u32> {
    let bytes = vaa
        .get(1..5)
        .ok_or_else(|| anyhow!("VAA is too short to carry a guardian set index"))?;
    Ok(u32::from_be_bytes(bytes.try_into()?))
}

fn guardian_set_address(index: u32) -> Pubkey {
    Pubkey::find_program_address(
        &[b"GuardianSet", &index.to_be_bytes()],
        &WORMHOLE_PROGRAM_ID,
    )
    .0
}

fn init_encoded_vaa_ix(write_authority: &Pubkey, encoded_vaa: &Pubkey) -> Instruction {
    Instruction {
        program_id: WORMHOLE_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*write_authority, true),
            AccountMeta::new(*encoded_vaa, false),
        ],
        data: IX_INIT_ENCODED_VAA.to_vec(),
    }
}

fn write_encoded_vaa_ix(
    write_authority: &Pubkey,
    draft_vaa: &Pubkey,
    index: u32,
    data: &[u8],
) -> Instruction {
    let mut ix_data = IX_WRITE_ENCODED_VAA.to_vec();
    ix_data.extend_from_slice(&index.to_le_bytes());
    ix_data.extend_from_slice(&(data.len() as u32).to_le_bytes());
    ix_data.extend_from_slice(data);

    Instruction {
        program_id: WORMHOLE_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*write_authority, true),
            AccountMeta::new(*draft_vaa, false),
        ],
        data: ix_data,
    }
}

fn verify_encoded_vaa_ix(
    write_authority: &Pubkey,
    draft_vaa: &Pubkey,
    guardian_set_index: u32,
) -> Instruction {
    Instruction {
        program_id: WORMHOLE_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*write_authority, true),
            AccountMeta::new(*draft_vaa, false),
            AccountMeta::new_readonly(guardian_set_address(guardian_set_index), false),
        ],
        data: IX_VERIFY_ENCODED_VAA_V1.to_vec(),
    }
}

fn close_encoded_vaa_ix(write_authority: &Pubkey, encoded_vaa: &Pubkey) -> Instruction {
    Instruction {
        program_id: WORMHOLE_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*write_authority, true),
            AccountMeta::new(*encoded_vaa, false),
        ],
        data: IX_CLOSE_ENCODED_VAA.to_vec(),
    }
}

fn update_price_feed_ix(
    payer: &Pubkey,
    encoded_vaa: &Pubkey,
    shard_id: u16,
    feed_id: [u8; 32],
    merkle_price_update: MerklePriceUpdate,
) -> Result<Instruction> {
    let params = PostUpdateParams {
        merkle_price_update,
        treasury_id: DEFAULT_TREASURY_ID,
    };

    let mut data = IX_UPDATE_PRICE_FEED.to_vec();
    data.extend_from_slice(&borsh::to_vec(&params)?);
    data.extend_from_slice(&shard_id.to_le_bytes());
    data.extend_from_slice(&feed_id);

    Ok(Instruction {
        program_id: PYTH_PUSH_ORACLE_ID,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(pyth_solana_receiver_sdk::ID, false),
            AccountMeta::new_readonly(*encoded_vaa, false),
            AccountMeta::new_readonly(get_config_address(), false),
            AccountMeta::new(get_treasury_address(DEFAULT_TREASURY_ID), false),
            AccountMeta::new(price_feed_address(shard_id, &feed_id), false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data,
    })
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
