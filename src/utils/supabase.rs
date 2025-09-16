use anchor_lang::prelude::Pubkey;
use anyhow::Result;
use chrono::{DateTime, Utc};
use native_tls::TlsConnector;
use postgres::{types::ToSql, Client};
use postgres_native_tls::MakeTlsConnector;
use std::{cmp::min, env};

const BATCH_SIZE: usize = 2000;
const COLS_PER_ROW: usize = 7;

pub struct SupabasePublisher {
    client: Client,
    table: String,
    buf: Vec<AccountHealthRow>,
}

#[derive(Clone)]
pub struct AccountHealthRow {
    pub account_address: String,
    pub assets_usd: f64,
    pub liabilities_usd: f64,
    pub maintenance_health: f64,
    pub percentage_health: f64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl SupabasePublisher {
    pub fn from_env() -> Result<Self> {
        let db_url = env::var("SUPABASE_URL").expect("SUPABASE_URL env var not set");
        let table = env::var("SUPABASE_TABLE").expect("SUPABASE_TABLE env var not set");

        let tls = MakeTlsConnector::new(TlsConnector::new()?);
        let client = Client::connect(&db_url, tls)?;

        Ok(Self {
            client,
            table,
            buf: Vec::with_capacity(BATCH_SIZE),
        })
    }

    /// Buffer a row; flush automatically when buffer is full.
    /// If `force_flush` is true, flush whatever remains (end of cycle).
    pub fn publish_health(
        &mut self,
        account_address: Pubkey,
        assets_usd: f64,
        liabilities_usd: f64,
        maintenance_health: f64,
        percentage_health: f64,
        force_flush: bool,
    ) -> Result<()> {
        let now = Utc::now();
        self.buf.push(AccountHealthRow {
            account_address: account_address.to_string(),
            assets_usd,
            liabilities_usd,
            maintenance_health,
            percentage_health,
            created_at: now,
            updated_at: now,
        });

        // Flush full batches (may flush multiple batches if buffer grew a lot)
        self.flush_full_batches()?;

        // End-of-batch: flush remainder
        if force_flush {
            self.flush_all()?;
        }
        Ok(())
    }

    fn flush_full_batches(&mut self) -> Result<()> {
        while self.buf.len() >= BATCH_SIZE {
            self.exec_chunk(BATCH_SIZE)?;
        }
        Ok(())
    }

    fn flush_all(&mut self) -> Result<()> {
        while !self.buf.is_empty() {
            let take = min(BATCH_SIZE, self.buf.len());
            self.exec_chunk(take)?;
        }
        Ok(())
    }

    /// Execute INSERT for the first `take` rows in the buffer.
    /// Leaves buffer intact on error; drains only on success.
    fn exec_chunk(&mut self, take: usize) -> Result<()> {
        let chunk = &self.buf[..take];

        // Build VALUES list and parameters
        let mut params: Vec<&(dyn ToSql + Sync)> = Vec::with_capacity(take * COLS_PER_ROW);
        let mut values = String::with_capacity(take * 32);

        for (i, r) in chunk.iter().enumerate() {
            if i > 0 {
                values.push(',');
            }
            let base = i * COLS_PER_ROW;
            values.push_str(&format!(
                "(${},${},${},${},${},${},${})",
                base + 1,
                base + 2,
                base + 3,
                base + 4,
                base + 5,
                base + 6,
                base + 7
            ));
            params.extend_from_slice(&[
                &r.account_address,
                &r.assets_usd,
                &r.liabilities_usd,
                &r.maintenance_health,
                &r.percentage_health,
                &r.created_at, // timestamptz column -> DateTime<Utc>
                &r.updated_at,
            ]);
        }

        let sql = format!(
            "INSERT INTO {} \
             (account_address, assets_usd, liabilities_usd, maintenance_health, percentage_health, created_at, updated_at) \
             VALUES {}",
            self.table, values
        );

        self.client.execute(&sql, &params)?;
        // Only remove on success
        self.buf.drain(0..take);
        Ok(())
    }
}
