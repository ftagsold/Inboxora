//! One-time table rewrite after the storage migrations (0095/0096): dropping columns and rows
//! frees nothing on disk until the tables are rewritten, so the first start afterwards runs
//! `VACUUM FULL` on them. It holds an exclusive lock per table and needs free space about the
//! size of the compacted table; a failure is logged and retried on the next start.

use inboxora_core::params;
use inboxora_core::pg::{Db, Q};

const MARKER: &str = "storage_compacted_v1";

/// Biggest first, so the space they release is available to the rest.
const TABLES: &[&str] = &[
    "messages",
    "logical_messages",
    "conversation_evidence",
    "conversations",
    "inbound_calendar_invitations",
    "calendar_events",
    "contacts",
    "calendar_occurrences",
    "calendar_occurrence_state",
    "unresolved_message_references",
    "provider_thread_mappings",
    "conversation_rebuild_audit",
    "conversation_ingest_failures",
    "spam_training_log",
    "auth_events",
    // Every old change-log row also bumped the collection's version row.
    "calendars",
    "address_books",
];

async fn total_size(db: &Db) -> Option<i64> {
    db.opt("SELECT pg_database_size(current_database()) AS n", params![]).await.ok().flatten().map(|r| r.get("n"))
}

fn mib(bytes: i64) -> String {
    format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
}

pub async fn run_once(db: &Db) {
    match db.opt("SELECT 1 FROM system_settings WHERE key = $1", params![MARKER]).await {
        Ok(Some(_)) => return,
        Ok(None) => {}
        Err(e) => {
            tracing::error!("[compaction] cannot read marker: {e}");
            return;
        }
    }
    let before = total_size(db).await;
    tracing::info!(
        "[compaction] rewriting tables to release space freed by the storage migration (database: {}) — this runs once",
        before.map_or_else(|| "?".into(), mib)
    );
    let client = match db.client().await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("[compaction] no database connection: {e}");
            return;
        }
    };
    if let Err(e) = client.batch("SET statement_timeout = 0").await {
        tracing::error!("[compaction] cannot lift the statement timeout: {e}");
        client.discard();
        return;
    }
    let mut failed = false;
    for table in TABLES {
        let started = std::time::Instant::now();
        match client.batch(&format!("VACUUM (FULL, ANALYZE) {table}")).await {
            Ok(()) => tracing::info!("[compaction] {table} done in {:.1}s", started.elapsed().as_secs_f64()),
            Err(e) => {
                failed = true;
                tracing::error!("[compaction] {table} failed (will retry on next start): {e}");
            }
        }
    }
    // The session's timeout was changed: don't hand it back to the pool.
    client.discard();
    let after = total_size(db).await;
    tracing::info!(
        "[compaction] database size {} → {}",
        before.map_or_else(|| "?".into(), mib),
        after.map_or_else(|| "?".into(), mib)
    );
    if !failed
        && let Err(e) = db
            .exec(
                "INSERT INTO system_settings (key, value) VALUES ($1, 'done') ON CONFLICT (key) DO UPDATE SET value = 'done', updated_at = NOW()",
                params![MARKER],
            )
            .await
    {
        tracing::error!("[compaction] cannot record completion: {e}");
    }
}
