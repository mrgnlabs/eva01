use anchor_lang::prelude::Pubkey;
use anyhow::Result;
use chrono::Utc;
use native_tls::TlsConnector;
use postgres::Client;
use postgres_native_tls::MakeTlsConnector;
use std::env;

pub struct SupabasePublisher {
    client: Client,
    table: String,
}

impl SupabasePublisher {
    pub fn from_env() -> Result<Self> {
        let db_url = env::var("SUPABASE_URL").expect("SUPABASE_URL env variable not set");
        let table = env::var("SUPABASE_TABLE").expect("SUPABASE_TABLE env variable not set");
        let tls = MakeTlsConnector::new(TlsConnector::new()?);
        let client = Client::connect(&db_url, tls)?;
        Ok(Self { client, table })
    }

    pub fn publish_health(
        &mut self,
        account_address: Pubkey,
        assets_usd: f64,
        liabilities_usd: f64,
        maintenance_health: f64,
        percentage_health: f64,
    ) -> Result<u64> {
        let sql = format!(
            "INSERT INTO {} \
             (account_address, assets_usd, liabilities_usd, maintenance_health, percentage_health, created_at, updated_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7)",
            self.table
        );
        let now = Utc::now();
        let n = self.client.execute(
            &sql,
            &[
                &account_address.to_string(),
                &assets_usd,
                &liabilities_usd,
                &maintenance_health,
                &percentage_health,
                &now,
                &now,
            ],
        )?;
        Ok(n)
    }
}
