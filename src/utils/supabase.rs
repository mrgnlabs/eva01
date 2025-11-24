use anchor_lang::prelude::Pubkey;
use anyhow::Result;
use chrono::{DateTime, Utc};
use log::debug;
use postgres::{types::ToSql, Client};
use rustls::pki_types::CertificateDer;
use rustls::{ClientConfig, RootCertStore};
use rustls_native_certs;
use serde::{Deserialize, Deserializer};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::time::{Duration, Instant};
use std::{cmp::min, env};
use tokio_postgres_rustls::MakeRustlsConnect;

const BATCH_SIZE: usize = 2000;
const COLS_PER_ROW: usize = 8;

pub struct SupabasePublisher {
    db_url: String,
    client: Client,
    table: String,
}

// #[derive(Clone)]
// pub struct AccountHealthRow {
//     pub account_address: String,
//     pub assets_usd: f64,
//     pub liabilities_usd: f64,
//     pub maintenance_health: f64,
//     pub percentage_health: f64,
//     pub created_at: DateTime<Utc>,
//     pub updated_at: DateTime<Utc>,
//     pub schedule: i64,
// }

impl SupabasePublisher {
    pub fn from_env() -> Result<Self> {
        let db_url = env::var("SUPABASE_URL").expect("SUPABASE_URL env var not set");
        let table = env::var("SUPABASE_TABLE").expect("SUPABASE_TABLE env var not set");

        let tls = make_tls()?;
        let client = postgres::Client::connect(&db_url, tls)?;
        Ok(Self {
            db_url,
            client,
            table,
        })
    }

    /// Execute INSERT for the first `take` rows in the buffer.
    /// Leaves buffer intact on error; drains only on success.
    fn exec_chunk(&mut self, _take: usize) -> Result<()> {
        // let rows: Vec<AccountHealthRow> = self.buf[..take].to_vec();
        // let mut params: Vec<&(dyn ToSql + Sync)> = Vec::with_capacity(rows.len() * COLS_PER_ROW);
        // let mut values = String::with_capacity(rows.len() * 32);

        // for (i, r) in rows.iter().enumerate() {
        //     if i > 0 {
        //         values.push(',');
        //     }
        //     let base = i * COLS_PER_ROW;
        //     values.push_str(&format!(
        //         "(${},${},${},${},${},${},${},${})",
        //         base + 1,
        //         base + 2,
        //         base + 3,
        //         base + 4,
        //         base + 5,
        //         base + 6,
        //         base + 7,
        //         base + 8
        //     ));
        //     params.extend_from_slice(&[
        //         &r.account_address,
        //         &r.assets_usd,
        //         &r.liabilities_usd,
        //         &r.maintenance_health,
        //         &r.percentage_health,
        //         &r.created_at,
        //         &r.updated_at,
        //         &r.schedule,
        //     ]);
        // }

        // let sql = format!(
        //     "INSERT INTO {} \
        //      (account_address, assets_usd, liabilities_usd, maintenance_health, percentage_health, created_at, updated_at, schedule) \
        //      VALUES {}",
        //     self.table, values
        // );

        // self.exec_with_retry(&sql, &params)?;
        // // Only remove on success
        // self.buf.drain(0..take);
        Ok(())
    }

    fn exec_with_retry(
        &mut self,
        sql: &str,
        params: &[&(dyn postgres::types::ToSql + Sync)],
    ) -> anyhow::Result<()> {
        let mut backoff = std::time::Duration::from_millis(100);
        for _ in 0..5 {
            match self.client.execute(sql, params) {
                Ok(n) => {
                    debug!("Wrote {} rows to DB", n);
                    return Ok(());
                }
                Err(e) if is_transient(&e) => {
                    self.reconnect()?;
                    std::thread::sleep(backoff);
                    backoff = backoff.saturating_mul(2);
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(anyhow::anyhow!("retry budget exhausted"))
    }

    fn reconnect(&mut self) -> anyhow::Result<()> {
        let tls = make_tls()?;
        let mut cfg: postgres::Config = self.db_url.parse()?;
        cfg.keepalives(true)
            .keepalives_idle(std::time::Duration::from_secs(30))
            .keepalives_interval(std::time::Duration::from_secs(10))
            .keepalives_retries(3);
        self.client = cfg.connect(tls)?;
        Ok(())
    }
}

fn is_transient(e: &postgres::Error) -> bool {
    let s = e.to_string();
    s.contains("close_notify") || s.contains("UnexpectedEof") || s.contains("connection closed")
}

fn make_tls() -> anyhow::Result<MakeRustlsConnect> {
    // A) load OS trust anchors
    let mut roots = RootCertStore::empty();
    let native_res = rustls_native_certs::load_native_certs();
    roots.add_parsable_certificates(native_res.certs);

    // B) load Supabase CA from PEM (required: their chain anchors at a Supabase Root CA)
    let pem = std::env::var("SUPABASE_CA_CERT")?.replace("\\n", "\n");
    let mut cursor = std::io::Cursor::new(pem.into_bytes());
    let extra: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cursor).collect::<Result<Vec<_>, _>>()?;
    let (_added, _ignored) = roots.add_parsable_certificates(extra);

    // C) build rustls client config
    let tls_config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    Ok(MakeRustlsConnect::new(tls_config))
}
