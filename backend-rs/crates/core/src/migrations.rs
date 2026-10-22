//! Schema migrations (port of `migrations.ts`): same `schema_migrations` table, sha256
//! checksums, advisory lock id and `-- no-transaction` handling, so a database migrated by
//! either backend is accepted by the other.

use std::path::{Path, PathBuf};

use crate::crypto::sha256_hex;
use crate::error::{Error, Result};
use crate::params;
use crate::pg::{Db, Q};

const LOCK_ID: i64 = 7_418_291_834;

/// Checksums of migrations that were intentionally replaced after release.
const ACCEPTED_REPLACED: &[(&str, &str)] =
    &[("0072_calendar_source_url_secrets", "5ca9454166b08d0af82935dd4032510e07b1836c61fec61223c305b6608f17a4")];

#[derive(Debug, Clone)]
pub struct Migration {
    pub version: String,
    pub sha256: String,
    pub sql: String,
}

fn is_migration_file(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() > 9 && b[..4].iter().all(u8::is_ascii_digit) && b[4] == b'_' && name.ends_with(".sql")
}

/// Locate the migrations directory: MIGRATIONS_DIR, `./migrations`, or the Node tree.
pub fn migrations_dir() -> PathBuf {
    if let Some(d) = crate::config::env("MIGRATIONS_DIR") {
        return d.into();
    }
    for candidate in ["migrations", "../backend/migrations", "../../backend/migrations", "../../../backend/migrations"]
    {
        if Path::new(candidate).is_dir() {
            return candidate.into();
        }
    }
    "migrations".into()
}

/// Read all migrations sorted by file name.
pub fn load(dir: &Path) -> Result<Vec<Migration>> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map_err(|e| Error::internal(format!("cannot read migrations in {}: {e}", dir.display())))?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| is_migration_file(n))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let sql = std::fs::read_to_string(dir.join(&name)).map_err(|e| Error::internal(e.to_string()))?;
            Ok(Migration { version: name.trim_end_matches(".sql").to_owned(), sha256: sha256_hex(&sql), sql })
        })
        .collect()
}

fn no_transaction(sql: &str) -> bool {
    // `/^--\s*no-transaction\b/im`: the marker must start the line.
    sql.lines().any(|l| {
        l.strip_prefix("--").is_some_and(|rest| {
            let rest = rest.trim_start().as_bytes();
            const MARKER: &[u8] = b"no-transaction";
            rest.len() >= MARKER.len()
                && rest[..MARKER.len()].eq_ignore_ascii_case(MARKER)
                && !rest.get(MARKER.len()).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
        })
    })
}

/// Statements of a `-- no-transaction` migration: comment-only lines removed, split on `;`.
fn split_statements(sql: &str) -> Vec<String> {
    let stripped: String =
        sql.split('\n').map(|l| if l.starts_with("--") { "" } else { l }).collect::<Vec<_>>().join("\n");
    stripped.split(';').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()
}

fn accepted_replaced(version: &str, sha: Option<&str>) -> bool {
    ACCEPTED_REPLACED.iter().any(|(v, s)| *v == version && Some(*s) == sha)
}

/// Apply pending migrations under the advisory lock. Returns the versions applied.
pub async fn run(db: &Db, dir: &Path) -> Result<Vec<String>> {
    let migrations = load(dir)?;
    let client = db.client().await?;
    client.exec("SELECT pg_advisory_lock($1)", params![LOCK_ID]).await?;
    let result = run_locked(&client, &migrations).await;
    if let Err(e) = client.exec("SELECT pg_advisory_unlock($1)", params![LOCK_ID]).await {
        tracing::warn!("advisory unlock failed: {e}");
        client.discard();
    }
    result
}

async fn run_locked(client: &crate::pg::Client, migrations: &[Migration]) -> Result<Vec<String>> {
    client.batch("SET statement_timeout = 0").await?;
    client.batch("CREATE TABLE IF NOT EXISTS schema_migrations (version VARCHAR(255) PRIMARY KEY, applied_at TIMESTAMPTZ DEFAULT NOW())").await?;
    client.batch("ALTER TABLE schema_migrations ADD COLUMN IF NOT EXISTS sha256 TEXT").await?;
    let applied = applied_versions(client).await?;
    let mut done = Vec::new();
    for m in migrations {
        if let Some(prev) = applied.iter().find(|(v, _)| *v == m.version) {
            let prev_sha = prev.1.as_deref();
            if prev_sha.is_some_and(|s| s != m.sha256) && !accepted_replaced(&m.version, prev_sha) {
                return Err(Error::internal(format!("Migration checksum mismatch: {}", m.version)));
            }
            if prev_sha.is_none() {
                client
                    .exec("UPDATE schema_migrations SET sha256 = $1 WHERE version = $2", params![&m.sha256, &m.version])
                    .await?;
            }
            continue;
        }
        if no_transaction(&m.sql) {
            for stmt in split_statements(&m.sql) {
                client.batch(&stmt).await?;
            }
            client
                .exec("INSERT INTO schema_migrations (version, sha256) VALUES ($1, $2)", params![&m.version, &m.sha256])
                .await?;
        } else {
            // One simple-protocol script: BEGIN; <migration>; record; COMMIT. A failure leaves the
            // connection in an aborted transaction, which we roll back explicitly.
            let script = format!(
                "BEGIN;\n{}\n;INSERT INTO schema_migrations (version, sha256) VALUES ('{}', '{}');\nCOMMIT;",
                m.sql,
                m.version.replace('\'', "''"),
                m.sha256
            );
            if let Err(e) = client.batch(&script).await {
                let _ = client.batch("ROLLBACK").await;
                return Err(e);
            }
        }
        tracing::info!("Applied migration {}", m.version);
        done.push(m.version.clone());
    }
    Ok(done)
}

async fn applied_versions(q: &impl Q) -> Result<Vec<(String, Option<String>)>> {
    let rows = q.rows("SELECT version, sha256 FROM schema_migrations", params![]).await?;
    Ok(rows.iter().map(|r| (r.get::<_, String>(0), r.get::<_, Option<String>>(1))).collect())
}

/// Verify (without changing anything) that every migration on disk is applied with a
/// matching checksum. Used while another process owns the migration step.
pub async fn verify(db: &Db, dir: &Path) -> Result<()> {
    let migrations = load(dir)?;
    let applied = applied_versions(db).await?;
    let mut missing = Vec::new();
    for m in &migrations {
        match applied.iter().find(|(v, _)| *v == m.version) {
            None => missing.push(m.version.clone()),
            Some((_, Some(sha))) if *sha != m.sha256 && !accepted_replaced(&m.version, Some(sha)) => {
                return Err(Error::internal(format!("Migration checksum mismatch: {}", m.version)));
            }
            _ => {}
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(Error::internal(format!("Database schema is missing migrations: {}", missing.join(", "))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_no_transaction_marker() {
        assert!(no_transaction("-- no-transaction\nCREATE INDEX CONCURRENTLY x ON t(a);"));
        assert!(!no_transaction("-- no-transactional stuff\nSELECT 1;"));
        assert!(!no_transaction("SELECT 1;"));
        // Multi-byte characters right where the marker would end must not panic.
        assert!(!no_transaction("-- gtd_folders) — a core↔plugin leak\nSELECT 1;"));
    }

    #[test]
    fn splits_statements_without_comment_lines() {
        let s = split_statements(
            "-- no-transaction\nCREATE INDEX a ON t(x);\n-- note; with semicolon\nCREATE INDEX b ON t(y);\n",
        );
        assert_eq!(s, vec!["CREATE INDEX a ON t(x)", "CREATE INDEX b ON t(y)"]);
    }

    #[test]
    fn loads_every_node_migration() {
        let all = load(Path::new("../../../backend/migrations")).unwrap();
        assert_eq!(all.len(), 100);
        assert_eq!(all[0].version, "0001_baseline");
    }
}
