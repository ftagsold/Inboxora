//! Shared helpers for integration tests that need PostgreSQL/Redis.
//! Tests skip (pass trivially) unless TEST_DB_HOST is set.

#![allow(dead_code)]

use inboxora_core::config::DbConfig;
use inboxora_core::pg::Db;

pub fn db() -> Option<Db> {
    let host = std::env::var("TEST_DB_HOST").ok()?;
    let cfg = DbConfig {
        host,
        port: std::env::var("TEST_DB_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(5432),
        database: std::env::var("TEST_DB_NAME").unwrap_or_else(|_| "mailflow".into()),
        user: std::env::var("TEST_DB_USER").unwrap_or_else(|_| "mailflow".into()),
        password: std::env::var("TEST_DB_PASSWORD").ok(),
        pool_max: 4,
        statement_timeout_ms: 30_000,
    };
    Some(Db::connect(&cfg).expect("pool"))
}

pub fn redis_url() -> Option<String> {
    std::env::var("TEST_REDIS_URL").ok()
}
