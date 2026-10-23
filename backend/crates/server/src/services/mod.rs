//! Background and shared services used by several route groups.

pub mod ai;
pub mod ai_http;
pub mod auth;
pub mod calendar;
pub mod calendar_invite;
pub mod categorizer;
pub mod codex_auth;
pub mod contact_backfill;
pub mod conversation;
pub mod conversation_ops;
pub mod credentials;
pub mod dav_client;
pub mod dav_sync;
pub mod dav_xml;
pub mod diagnostics_ring;
pub mod inbox_rules;
pub mod invitations;
pub mod mail_compose;
pub mod mail_utils;
pub mod oauth;
pub mod oidc;
pub mod push;
pub mod push_dispatch;
pub mod sender_favicon;
pub mod smtp;
pub mod totp;
