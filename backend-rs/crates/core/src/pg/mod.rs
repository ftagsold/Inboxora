//! PostgreSQL access: pool, loosely typed parameters, node-pg compatible row JSON.

pub mod numeric;
pub mod row;
pub mod value;

use std::time::{Duration, Instant};

use deadpool_postgres::{GenericClient, Manager, ManagerConfig, Pool, RecyclingMethod};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value};
use tokio_postgres::types::ToSql;
use tokio_postgres::{NoTls, Row};

pub use row::{NodeJson, row_to_json, rows_to_json};
pub use value::{Val, json};

use crate::config::DbConfig;
use crate::error::{Error, Result};

/// Build a parameter slice: `params![a, b, c]`.
#[macro_export]
macro_rules! params {
    () => { &[] as &[$crate::pg::Val] };
    ($($v:expr),+ $(,)?) => { &[$($crate::pg::Val::from($v)),+] as &[$crate::pg::Val] };
}

/// The application's connection pool.
#[derive(Clone)]
pub struct Db {
    pool: Pool,
}

fn as_sql(params: &[Val]) -> Vec<&(dyn ToSql + Sync)> {
    params.iter().map(|p| p as &(dyn ToSql + Sync)).collect()
}

/// Object-safe subset of `GenericClient` with statement caching.
pub trait GenericClientDyn: Send + Sync {
    fn query_dyn<'a>(&'a self, sql: &'a str, params: &'a [Val]) -> BoxFuture<'a, Result<Vec<Row>>>;
    fn execute_dyn<'a>(&'a self, sql: &'a str, params: &'a [Val]) -> BoxFuture<'a, Result<u64>>;
    fn batch_dyn<'a>(&'a self, sql: &'a str) -> BoxFuture<'a, Result<()>>;
}

impl<C: GenericClient + Send + Sync> GenericClientDyn for C {
    fn query_dyn<'a>(&'a self, sql: &'a str, params: &'a [Val]) -> BoxFuture<'a, Result<Vec<Row>>> {
        Box::pin(async move {
            let start = Instant::now();
            let stmt = self.prepare_cached(sql).await?;
            let rows = self.query(&stmt, &as_sql(params)).await;
            crate::metrics::record_db(start.elapsed());
            Ok(rows?)
        })
    }

    fn execute_dyn<'a>(&'a self, sql: &'a str, params: &'a [Val]) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let start = Instant::now();
            let stmt = self.prepare_cached(sql).await?;
            let n = self.execute(&stmt, &as_sql(params)).await;
            crate::metrics::record_db(start.elapsed());
            Ok(n?)
        })
    }

    fn batch_dyn<'a>(&'a self, sql: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { Ok(self.batch_execute(sql).await?) })
    }
}

/// Query helpers shared by the pool and transactions.
pub trait Q: Sync {
    fn conn(&self) -> BoxFuture<'_, Result<Conn<'_>>>;

    /// All rows.
    fn rows<'a>(&'a self, sql: &'a str, params: &'a [Val]) -> BoxFuture<'a, Result<Vec<Row>>> {
        Box::pin(async move { self.conn().await?.c().query_dyn(sql, params).await })
    }

    /// The first row, if any.
    fn opt<'a>(&'a self, sql: &'a str, params: &'a [Val]) -> BoxFuture<'a, Result<Option<Row>>> {
        Box::pin(async move { Ok(self.rows(sql, params).await?.into_iter().next()) })
    }

    /// Affected row count.
    fn exec<'a>(&'a self, sql: &'a str, params: &'a [Val]) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move { self.conn().await?.c().execute_dyn(sql, params).await })
    }

    /// Rows as node-pg JSON objects.
    fn json_rows<'a>(&'a self, sql: &'a str, params: &'a [Val]) -> BoxFuture<'a, Result<Vec<Value>>> {
        Box::pin(async move { Ok(rows_to_json(&self.rows(sql, params).await?)?) })
    }

    /// First row as a node-pg JSON object.
    fn json_opt<'a>(&'a self, sql: &'a str, params: &'a [Val]) -> BoxFuture<'a, Result<Option<Map<String, Value>>>> {
        Box::pin(async move {
            match self.opt(sql, params).await? {
                Some(r) => Ok(Some(row_to_json(&r)?)),
                None => Ok(None),
            }
        })
    }

    /// Run a multi-statement script without parameters.
    fn batch<'a>(&'a self, sql: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.conn().await?.c().batch_dyn(sql).await })
    }
}

/// A borrowed or pooled connection.
pub enum Conn<'a> {
    Pooled(Box<deadpool_postgres::Object>),
    Borrowed(&'a dyn GenericClientDyn),
}

impl Conn<'_> {
    fn c(&self) -> &dyn GenericClientDyn {
        match self {
            Conn::Pooled(o) => &**o as &dyn GenericClientDyn,
            Conn::Borrowed(c) => *c,
        }
    }
}

impl Q for Db {
    fn conn(&self) -> BoxFuture<'_, Result<Conn<'_>>> {
        Box::pin(async move { Ok(Conn::Pooled(Box::new(self.pool.get().await?))) })
    }
}

/// An open transaction. Dropping it without [`Tx::commit`] rolls back.
pub struct Tx<'a> {
    inner: deadpool_postgres::Transaction<'a>,
}

impl Q for Tx<'_> {
    fn conn(&self) -> BoxFuture<'_, Result<Conn<'_>>> {
        Box::pin(async move { Ok(Conn::Borrowed(&self.inner as &dyn GenericClientDyn)) })
    }
}

impl<'a> Tx<'a> {
    pub async fn commit(self) -> Result<()> {
        Ok(self.inner.commit().await?)
    }

    pub async fn rollback(self) -> Result<()> {
        Ok(self.inner.rollback().await?)
    }
}

/// A dedicated pooled connection (for advisory locks and transactions).
pub struct Client {
    obj: deadpool_postgres::Object,
}

impl Q for Client {
    fn conn(&self) -> BoxFuture<'_, Result<Conn<'_>>> {
        Box::pin(async move { Ok(Conn::Borrowed(&self.obj as &dyn GenericClientDyn)) })
    }
}

impl Client {
    /// Start a transaction (`serializable` selects SERIALIZABLE isolation).
    pub async fn begin(&mut self, serializable: bool) -> Result<Tx<'_>> {
        let builder = self.obj.build_transaction();
        let builder =
            if serializable { builder.isolation_level(tokio_postgres::IsolationLevel::Serializable) } else { builder };
        Ok(Tx { inner: builder.start().await? })
    }

    /// Drop this connection instead of returning it to the pool (its session state is unknown).
    pub fn discard(self) {
        drop(deadpool_postgres::Object::take(self.obj));
    }
}

impl Db {
    /// Connect using `DB_*` settings. Connections are opened lazily.
    pub fn connect(cfg: &DbConfig) -> Result<Self> {
        let mut pg = tokio_postgres::Config::new();
        if cfg.host.starts_with('/') {
            pg.host_path(&cfg.host);
        } else {
            pg.host(&cfg.host);
        }
        pg.port(cfg.port)
            .dbname(&cfg.database)
            .user(&cfg.user)
            .application_name("inboxora")
            .connect_timeout(Duration::from_secs(5))
            // Applied during the handshake, so no query can run before it takes effect.
            .options(format!("-c statement_timeout={}", cfg.statement_timeout_ms));
        if let Some(pw) = &cfg.password {
            pg.password(pw);
        }
        let mgr = Manager::from_config(pg, NoTls, ManagerConfig { recycling_method: RecyclingMethod::Fast });
        let pool = Pool::builder(mgr)
            .max_size(cfg.pool_max)
            .wait_timeout(Some(Duration::from_secs(5)))
            .create_timeout(Some(Duration::from_secs(5)))
            .recycle_timeout(Some(Duration::from_secs(5)))
            .runtime(deadpool_postgres::Runtime::Tokio1)
            .build()
            .map_err(|e| Error::internal(format!("database pool: {e}")))?;
        Ok(Self { pool })
    }

    /// Check out a dedicated connection.
    pub async fn client(&self) -> Result<Client> {
        Ok(Client { obj: self.pool.get().await? })
    }

    /// Run `f` in a transaction; commit on `Ok`, roll back on `Err`. With `serializable`,
    /// serialization failures and deadlocks are retried up to `retries` times (the body must
    /// therefore be free of non-database side effects).
    pub async fn transaction<T, F>(&self, serializable: bool, retries: u32, mut f: F) -> Result<T>
    where
        F: for<'t> FnMut(&'t Tx<'t>) -> BoxFuture<'t, Result<T>>,
    {
        let mut attempt = 0;
        loop {
            let mut client = self.client().await?;
            let tx = client.begin(serializable).await?;
            let result = f(&tx).await;
            match result {
                Ok(v) => {
                    tx.commit().await?;
                    return Ok(v);
                }
                Err(e) => {
                    if let Err(rb) = tx.rollback().await {
                        tracing::warn!("ROLLBACK failed (original error preserved): {rb}");
                        drop(client);
                    }
                    let retryable = serializable && matches!(e.pg_code(), Some("40001" | "40P01"));
                    if retryable && attempt < retries {
                        attempt += 1;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    /// Pool status for diagnostics.
    pub fn status(&self) -> deadpool_postgres::Status {
        self.pool.status()
    }
}
