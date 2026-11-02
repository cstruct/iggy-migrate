//! The Postgres ledger: events as JSON rows in one table, keyed by namespace
//! so several Iggy servers can share it.
//!
//! `up` holds a session-level advisory lock for its whole run, so concurrent
//! runners on the same namespace wait for each other. The lock belongs to the
//! connection, so it is released when a runner exits or dies, never left
//! behind.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio_postgres::error::SqlState;
use tokio_postgres::{Client, Config};

use super::Event;

pub struct Ledger {
    client: Client,
    namespace: String,
    table: String,
    lock_timeout: Duration,
}

pub struct Options<'a> {
    pub url: &'a str,
    pub password: Option<&'a str>,
    pub namespace: &'a str,
    pub table: &'a str,
    pub lock_timeout: Duration,
    /// Shown in `pg_stat_activity`, so whoever holds the lock can be found.
    pub runner: &'a str,
}

impl Ledger {
    pub async fn connect(options: Options<'_>) -> Result<Self> {
        let mut config: Config = options.url.parse().context("parsing ledger.url")?;
        if config.get_password().is_some() {
            bail!("ledger.url must not contain a password; set ledger.password in the secrets file instead");
        }
        if let Some(password) = options.password {
            config.password(password);
        }
        config.application_name(format!("iggy-migrate ({})", options.runner));
        if !options.table.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') {
            bail!("ledger.table `{}` may only contain letters, digits, `_` and `.`", options.table);
        }
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let mut roots = rustls::RootCertStore::empty();
        roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
        let tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let (client, connection) = config
            .connect(tokio_postgres_rustls::MakeRustlsConnect::new(tls))
            .await
            .context("connecting to the Postgres ledger")?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("error: Postgres ledger connection: {e}");
            }
        });
        Ok(Ledger {
            client,
            namespace: options.namespace.to_string(),
            table: options.table.to_string(),
            lock_timeout: options.lock_timeout,
        })
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub async fn ensure(&self) -> Result<()> {
        let table = &self.table;
        let index = format!("{}_namespace_seq", table.replace('.', "_"));
        self.client
            .batch_execute(&format!(
                "CREATE TABLE IF NOT EXISTS {table} (
                     seq bigserial PRIMARY KEY,
                     namespace text NOT NULL,
                     event jsonb NOT NULL,
                     recorded_at timestamptz NOT NULL DEFAULT now()
                 );
                 CREATE INDEX IF NOT EXISTS {index} ON {table} (namespace, seq);"
            ))
            .await
            .with_context(|| format!("creating the ledger table {table}"))
    }

    pub async fn read(&self) -> Result<Vec<Event>> {
        let exists: bool = self
            .client
            .query_one("SELECT to_regclass($1) IS NOT NULL", &[&self.table])
            .await
            .context("reading the Postgres ledger")?
            .get(0);
        if !exists {
            return Ok(Vec::new());
        }
        let rows = self
            .client
            .query(
                &format!("SELECT seq, event::text FROM {} WHERE namespace = $1 ORDER BY seq", self.table),
                &[&self.namespace],
            )
            .await
            .context("reading the Postgres ledger")?;
        rows.iter()
            .map(|row| {
                let seq: i64 = row.get(0);
                let json: &str = row.get(1);
                serde_json::from_str(json)
                    .with_context(|| format!("ledger row {seq} in {} is not a valid event", self.table))
            })
            .collect()
    }

    pub async fn append(&self, event: &Event) -> Result<()> {
        let json = serde_json::to_string(event)?;
        self.client
            .execute(
                &format!("INSERT INTO {} (namespace, event) VALUES ($1, $2::text::jsonb)", self.table),
                &[&self.namespace, &json],
            )
            .await
            .context("writing ledger event")?;
        Ok(())
    }

    /// Takes the namespace's advisory lock, waiting up to `lock_timeout` for
    /// another runner to release it.
    pub async fn lock(&self) -> Result<()> {
        let key: i64 = self
            .client
            .query_one("SELECT hashtextextended($1, 0)", &[&format!("iggy-migrate:{}:{}", self.table, self.namespace)])
            .await?
            .get(0);
        let acquired: bool = self.client.query_one("SELECT pg_try_advisory_lock($1)", &[&key]).await?.get(0);
        if acquired {
            return Ok(());
        }
        let holder = self.holder(key).await?.unwrap_or_else(|| "another runner".into());
        eprintln!("waiting for {holder} to finish with ledger namespace `{}`", self.namespace);
        let timeout_ms = i64::try_from(self.lock_timeout.as_millis()).unwrap_or(i64::MAX);
        self.client.batch_execute(&format!("SET lock_timeout = {timeout_ms}")).await?;
        let locked = self.client.query_one("SELECT pg_advisory_lock($1)", &[&key]).await;
        self.client.batch_execute("RESET lock_timeout").await?;
        match locked {
            Ok(_) => Ok(()),
            Err(e) if e.code() == Some(&SqlState::LOCK_NOT_AVAILABLE) => Err(anyhow!(
                "gave up after {} waiting for {holder} (ledger.lock_timeout)",
                humantime::format_duration(self.lock_timeout)
            )),
            Err(e) => Err(e.into()),
        }
    }

    /// Who holds the advisory lock `key`, for the waiting message.
    async fn holder(&self, key: i64) -> Result<Option<String>> {
        let row = self
            .client
            .query_opt(
                "SELECT a.application_name, a.client_addr::text, a.backend_start::text
                 FROM pg_locks l JOIN pg_stat_activity a USING (pid)
                 WHERE l.locktype = 'advisory' AND l.granted AND l.objsubid = 1
                   AND l.classid::bigint = ($1::bigint >> 32) & 4294967295
                   AND l.objid::bigint = $1::bigint & 4294967295",
                &[&key],
            )
            .await?;
        Ok(row.map(|row| {
            let name: Option<String> = row.get(0);
            let addr: Option<String> = row.get(1);
            let since: Option<String> = row.get(2);
            let mut holder = name.filter(|n| !n.is_empty()).unwrap_or_else(|| "another runner".into());
            if let Some(addr) = addr {
                holder.push_str(&format!(" from {addr}"));
            }
            if let Some(since) = since {
                holder.push_str(&format!(", connected since {since}"));
            }
            holder
        }))
    }
}
