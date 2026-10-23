//! Inboxora core: configuration, PostgreSQL and Redis access, sessions, crypto, metrics.

pub mod config;
pub mod crypto;
pub mod error;
pub mod events;
pub mod json;
pub mod metrics;
pub mod migrations;
pub mod pg;
pub mod redis;
pub mod session;

pub use error::{Error, Result};
