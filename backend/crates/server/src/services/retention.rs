//! Hourly retention for append-only and expiring tables, so nothing grows without bound.

use std::time::Duration;

use inboxora_core::params;
use inboxora_core::pg::{Db, Q};

/// `(label, statement)`; each runs on its own so one failure doesn't stop the rest.
const PRUNE: &[(&str, &str)] = &[
    ("auth events", "DELETE FROM auth_events WHERE created_at < NOW() - INTERVAL '90 days'"),
    ("spam training log", "DELETE FROM spam_training_log WHERE created_at < NOW() - INTERVAL '90 days'"),
    ("rebuild audit", "DELETE FROM conversation_rebuild_audit WHERE created_at < NOW() - INTERVAL '30 days'"),
    (
        "resolved ingest failures",
        "DELETE FROM conversation_ingest_failures WHERE resolved_at IS NOT NULL AND resolved_at < NOW() - INTERVAL '7 days'",
    ),
    (
        "send idempotency",
        "DELETE FROM send_idempotency WHERE status <> 'pending' AND updated_at < NOW() - INTERVAL '7 days'",
    ),
    (
        "invitation outbox",
        "DELETE FROM calendar_invitation_outbox WHERE (status = 'sent' OR (status = 'failed' AND attempts >= 5)) \
         AND COALESCE(delivered_at, created_at) < NOW() - INTERVAL '30 days'",
    ),
    ("email OTP tokens", "DELETE FROM email_otp_tokens WHERE expires_at < NOW() - INTERVAL '1 day'"),
    ("password reset tokens", "DELETE FROM password_reset_tokens WHERE expires_at < NOW() - INTERVAL '1 day'"),
    ("trusted devices", "DELETE FROM trusted_devices WHERE expires_at < NOW()"),
    // Tombstones older than 30 days are dropped; tokens from before them are then refused
    // (valid-sync-token) and the client resyncs the collection.
    (
        "calendar tombstones",
        "WITH pruned AS (DELETE FROM calendar_sync_changes WHERE deleted AND created_at < NOW() - INTERVAL '30 days' \
           RETURNING calendar_id, version) \
         UPDATE calendars c SET sync_floor = GREATEST(c.sync_floor, p.v) \
           FROM (SELECT calendar_id, MAX(version) AS v FROM pruned GROUP BY calendar_id) p WHERE c.id = p.calendar_id",
    ),
    (
        "contact tombstones",
        "WITH pruned AS (DELETE FROM contact_sync_changes WHERE deleted AND created_at < NOW() - INTERVAL '30 days' \
           RETURNING address_book_id, version) \
         UPDATE address_books b SET sync_floor = GREATEST(b.sync_floor, p.v) \
           FROM (SELECT address_book_id, MAX(version) AS v FROM pruned GROUP BY address_book_id) p WHERE b.id = p.address_book_id",
    ),
];

pub async fn prune_once(db: &Db) {
    for (label, sql) in PRUNE {
        match db.exec(sql, params![]).await {
            Ok(n) if n > 0 => tracing::info!("[retention] pruned {n} {label}"),
            Ok(_) => {}
            Err(e) => tracing::error!("[retention] failed to prune {label}: {e}"),
        }
    }
}

pub fn start(db: Db) {
    tokio::spawn(async move {
        let mut i = tokio::time::interval(Duration::from_secs(3600));
        loop {
            i.tick().await;
            prune_once(&db).await;
        }
    });
}
