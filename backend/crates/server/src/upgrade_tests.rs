//! The storage upgrade (0095/0096 + compaction) against PostgreSQL. Runs when `TEST_DB_HOST` is
//! set (see engine/live_tests.rs), skips otherwise.

use inboxora_core::config::DbConfig;
use inboxora_core::params;
use inboxora_core::pg::{Db, Q};

fn db_config(database: &str) -> Option<DbConfig> {
    Some(DbConfig {
        host: std::env::var("TEST_DB_HOST").ok()?,
        port: std::env::var("TEST_DB_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(5432),
        database: database.into(),
        user: std::env::var("TEST_DB_USER").unwrap_or_else(|_| "mailflow".into()),
        password: std::env::var("TEST_DB_PASSWORD").ok(),
        pool_max: 4,
        statement_timeout_ms: 120_000,
    })
}

/// Migrations up to (and including) `last`, copied to a temporary directory.
fn migrations_until(last: &str) -> std::path::PathBuf {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations");
    let dir = std::env::temp_dir().join(format!("inboxora-migrations-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    for e in std::fs::read_dir(&src).expect("migrations") {
        let e = e.expect("entry");
        let name = e.file_name().into_string().expect("name");
        if name.ends_with(".sql") && name.as_str() <= last {
            std::fs::copy(e.path(), dir.join(&name)).expect("copy");
        }
    }
    dir
}

const SEED: &str = r#"
INSERT INTO users (id, username, password_hash) VALUES ('00000000-0000-0000-0000-0000000000a1', 'upgrade@example.test', 'x');
INSERT INTO email_accounts (id, user_id, name, email_address, imap_host, smtp_host, auth_user, auth_pass)
  VALUES ('00000000-0000-0000-0000-0000000000b1', '00000000-0000-0000-0000-0000000000a1', 'U', 'u@example.test', 'imap.example.test', 'smtp.example.test', 'u', 'x');
INSERT INTO conversations (id, user_id, account_id, kind, subject_snapshot, canonical_subject, logical_message_count, copy_count, unread_count)
  SELECT md5('c' || g)::uuid, '00000000-0000-0000-0000-0000000000a1', '00000000-0000-0000-0000-0000000000b1', 'human_reply_chain',
         'Subject ' || g, 'subject ' || g, 1, 1, 0
    FROM generate_series(1, 4000) g;
INSERT INTO logical_messages (id, user_id, account_id, conversation_id, canonical_message_id, message_id_collision_key, raw_headers, subject,
                              canonical_subject, parsed_references, parsed_in_reply_to, direction, message_date)
  SELECT md5('l' || g)::uuid, '00000000-0000-0000-0000-0000000000a1', '00000000-0000-0000-0000-0000000000b1', md5('c' || (g % 4000 + 1))::uuid,
         '<m' || g || '@example.test>', md5('k' || g), repeat('Received: from somewhere by somewhere-else; ', 60), 'Subject ' || g,
         'subject ' || g, '["<root@example.test>"]', '[]', 'incoming', NOW() - (g || ' minutes')::interval
    FROM generate_series(1, 8000) g;
INSERT INTO messages (account_id, uid, folder, message_id, subject, from_name, from_email, to_addresses, date, snippet, body_text, body_html,
                      is_read, conversation_raw_headers, logical_message_id, conversation_id, conversation_user_id, thread_id)
  SELECT '00000000-0000-0000-0000-0000000000b1', g, CASE WHEN g % 3 = 0 THEN 'Archive' ELSE 'INBOX' END, '<m' || g || '@example.test>',
         'Subject ' || g, 'Sender ' || g, 's' || g || '@example.test', '[{"name":"Me","email":"u@example.test"}]',
         NOW() - (g || ' minutes')::interval, 'Snippet text ' || g, repeat('Body line ' || g || '. ', 200), repeat('<p>Body ' || g || '</p>', 400),
         g % 2 = 0, repeat('Received: from somewhere by somewhere-else; ', 60), md5('l' || g)::uuid, md5('c' || (g % 4000 + 1))::uuid,
         '00000000-0000-0000-0000-0000000000a1', '<m' || g || '@example.test>'
    FROM generate_series(1, 8000) g;
INSERT INTO conversation_evidence (user_id, account_id, conversation_id, logical_message_id, evidence_type, evidence_value_hash, weight, details)
  SELECT '00000000-0000-0000-0000-0000000000a1', '00000000-0000-0000-0000-0000000000b1', md5('c' || (g % 4000 + 1))::uuid, md5('l' || g)::uuid,
         t, md5(t || g), 0.9, '{"relationType":"in-reply-to"}'
    FROM generate_series(1, 8000) g, unnest(ARRAY['new-root', 'rfc-parent', 'provider-thread-id']) t;
INSERT INTO calendars (id, user_id, owner_user_id, name, source, external_url, read_only)
  VALUES ('00000000-0000-0000-0000-0000000000c1', '00000000-0000-0000-0000-0000000000a1', '00000000-0000-0000-0000-0000000000a1', 'Two-way', 'caldav', 'source:00000000-0000-0000-0000-0000000000d1:abc', false),
         ('00000000-0000-0000-0000-0000000000c2', '00000000-0000-0000-0000-0000000000a1', '00000000-0000-0000-0000-0000000000a1', 'Feed', 'ical_url', 'source:00000000-0000-0000-0000-0000000000d2', true);
INSERT INTO calendar_events (calendar_id, user_id, uid, raw_ical, etag, summary, starts_at, ends_at)
  SELECT '00000000-0000-0000-0000-0000000000c2', '00000000-0000-0000-0000-0000000000a1', 'f' || g, 'BEGIN:VCALENDAR END:VCALENDAR', 'v0', 'Feed ' || g, NOW(), NOW() + interval '1 hour'
    FROM generate_series(1, 50) g;
INSERT INTO address_books (id, user_id, name, source, external_url)
  VALUES ('00000000-0000-0000-0000-0000000000e1', '00000000-0000-0000-0000-0000000000a1', 'Remote', 'carddav', 'source:00000000-0000-0000-0000-0000000000d1:def');
INSERT INTO contacts (address_book_id, user_id, uid, vcard, display_name)
  SELECT '00000000-0000-0000-0000-0000000000e1', '00000000-0000-0000-0000-0000000000a1', 'p' || g, 'BEGIN:VCARD END:VCARD', 'Person ' || g
    FROM generate_series(1, 30) g;
INSERT INTO calendar_events (calendar_id, user_id, uid, raw_ical, etag, summary, starts_at, ends_at)
  SELECT '00000000-0000-0000-0000-0000000000c1', '00000000-0000-0000-0000-0000000000a1', 'e' || g,
         'BEGIN:VCALENDAR' || repeat(' padding', 120) || 'END:VCALENDAR', 'v0', 'Event ' || g, NOW() + (g || ' hours')::interval, NOW() + (g || ' hours')::interval + interval '1 hour'
    FROM generate_series(1, 400) g;
"#;

/// Twenty unconditional refreshes of every two-way event, as the old sync did.
const REFRESH: &str = "UPDATE calendar_events SET raw_ical = raw_ical || ' ', etag = md5(raw_ical), updated_at = NOW() \
    WHERE calendar_id = '00000000-0000-0000-0000-0000000000c1'";

/// Bytes held by the application's tables (with indexes and TOAST); the catalogs are a constant.
async fn size(db: &Db) -> i64 {
    db.opt(
        "SELECT SUM(pg_total_relation_size(c.oid))::bigint AS n FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'public' AND c.relkind = 'r'",
        params![],
    )
    .await
    .unwrap()
    .unwrap()
    .get("n")
}

async fn count(db: &Db, sql: &str) -> i64 {
    db.opt(sql, params![]).await.unwrap().unwrap().get::<_, i64>(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_upgrade_preserves_links_and_shrinks() {
    let Some(admin_cfg) = db_config("postgres") else { return };
    let admin = Db::connect(&admin_cfg).expect("admin pool");
    admin.batch("DROP DATABASE IF EXISTS storage_upgrade_test WITH (FORCE)").await.unwrap();
    admin.batch("CREATE DATABASE storage_upgrade_test").await.unwrap();
    let db = Db::connect(&db_config("storage_upgrade_test").unwrap()).unwrap();

    let old = migrations_until("0094_calendar_invitation_responses.sql");
    inboxora_core::migrations::run(&db, &old).await.expect("old migrations");
    db.batch(SEED).await.expect("seed");
    for _ in 0..20 {
        db.batch(REFRESH).await.unwrap();
    }
    let changes_before = count(&db, "SELECT COUNT(*)::bigint FROM calendar_sync_changes").await;
    assert!(changes_before >= 8000, "{changes_before}");
    db.batch("VACUUM ANALYZE").await.unwrap();
    let before = size(&db).await;

    let all = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations");
    inboxora_core::migrations::run(&db, &all).await.expect("new migrations");
    crate::services::compaction::run_once(&db).await;
    let after = size(&db).await;
    eprintln!("tables {:.1} MiB → {:.1} MiB", before as f64 / 1_048_576.0, after as f64 / 1_048_576.0);

    assert_eq!(count(&db, "SELECT COUNT(*)::bigint FROM messages").await, 8000, "every index row survives");
    assert_eq!(
        count(&db, "SELECT COUNT(*)::bigint FROM messages m JOIN logical_messages l ON l.id = m.logical_message_id")
            .await,
        8000,
        "threading links survive"
    );
    assert_eq!(
        count(&db, "SELECT COUNT(*)::bigint FROM logical_messages WHERE subject_hash IS NOT NULL").await,
        8000,
        "subject hashes are derived"
    );
    assert_eq!(
        count(&db, "SELECT COUNT(*)::bigint FROM logical_messages WHERE references_anchor = '<root@example.test>'")
            .await,
        8000
    );
    assert_eq!(count(&db, "SELECT COUNT(*)::bigint FROM conversation_evidence").await, 0, "routine evidence dropped");
    assert_eq!(count(&db, "SELECT COUNT(*)::bigint FROM calendar_sync_changes").await, 400, "one change row per event");
    assert_eq!(
        count(&db, "SELECT COUNT(*)::bigint FROM calendar_events").await,
        400,
        "two-way events stay, feed events go"
    );
    assert_eq!(count(&db, "SELECT COUNT(*)::bigint FROM contacts").await, 0, "CardDAV contacts are served live");
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*)::bigint FROM information_schema.tables WHERE table_name = 'calendar_import_documents'"
        )
        .await,
        0
    );
    let content_columns = count(
        &db,
        "SELECT COUNT(*)::bigint FROM information_schema.columns WHERE table_name IN ('messages', 'logical_messages', 'conversations') \
         AND column_name IN ('subject', 'snippet', 'body_text', 'body_html', 'from_email', 'raw_headers', 'conversation_raw_headers', 'subject_snapshot')",
    )
    .await;
    assert_eq!(content_columns, 0, "no content columns remain");
    assert_eq!(count(&db, "SELECT COUNT(*)::bigint FROM system_settings WHERE key = 'storage_compacted_v1'").await, 1);
    assert!(after * 4 < before, "the rewrite releases the dropped content: {before} → {after}");

    // A no-op refresh no longer writes change rows.
    db.batch(REFRESH.replace("raw_ical || ' '", "raw_ical").replace("md5(raw_ical)", "etag").as_str()).await.unwrap();
    assert_eq!(count(&db, "SELECT COUNT(*)::bigint FROM calendar_sync_changes").await, 400);
    let _ = std::fs::remove_dir_all(&old);
}
