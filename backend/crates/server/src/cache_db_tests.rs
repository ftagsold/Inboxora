//! The change sequence, delete tombstones and connection-limit columns (migration 0099) against
//! PostgreSQL. Runs when `TEST_DB_HOST` is set (see engine/live_tests.rs), skips otherwise.

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
        statement_timeout_ms: 60_000,
    })
}

const USER: &str = "00000000-0000-0000-0000-0000000000c1";
const ACCOUNT: &str = "00000000-0000-0000-0000-0000000000d1";

async fn seq_of(db: &Db, uid: i64) -> i64 {
    db.opt("SELECT change_seq FROM messages WHERE uid = $1", params![uid]).await.unwrap().unwrap().get(0)
}

async fn tombstones(db: &Db) -> Vec<(String, String)> {
    db.rows("SELECT id::text, folder FROM message_tombstones ORDER BY seq", params![])
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn change_sequence_and_tombstones() {
    let Some(admin_cfg) = db_config("postgres") else { return };
    let admin = Db::connect(&admin_cfg).expect("admin pool");
    admin.batch("DROP DATABASE IF EXISTS cache_db_test WITH (FORCE)").await.unwrap();
    admin.batch("CREATE DATABASE cache_db_test").await.unwrap();
    let db = Db::connect(&db_config("cache_db_test").unwrap()).unwrap();
    let all = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations");
    inboxora_core::migrations::run(&db, &all).await.expect("migrations");

    db.batch(&format!(
        "INSERT INTO users (id, username, password_hash) VALUES ('{USER}', 'cache@example.test', 'x');
         INSERT INTO email_accounts (id, user_id, name, email_address, imap_host, imap_port, smtp_host, auth_user, auth_pass)
           VALUES ('{ACCOUNT}', '{USER}', 'C', 'c@example.test', 'IMAP.Example.test', 993, 'smtp.example.test', 'c', 'x');
         INSERT INTO messages (account_id, uid, folder, message_id, date)
           SELECT '{ACCOUNT}', g, 'INBOX', '<c' || g || '@example.test>', NOW() - (g || ' minutes')::interval FROM generate_series(1, 3) g;"
    ))
    .await
    .expect("seed");

    // Every row starts with a sequence number; list-visible changes bump it, others do not.
    let s1 = seq_of(&db, 1).await;
    assert!(s1 > 0);
    db.exec("UPDATE messages SET is_read = true WHERE uid = 1", params![]).await.unwrap();
    let s1_read = seq_of(&db, 1).await;
    assert!(s1_read > s1, "read flag bumps the sequence");
    db.exec("UPDATE messages SET synced_at = NOW() WHERE uid = 1", params![]).await.unwrap();
    assert_eq!(seq_of(&db, 1).await, s1_read, "bookkeeping columns do not");
    db.exec("UPDATE messages SET folder = 'Archive' WHERE uid = 2", params![]).await.unwrap();
    assert!(seq_of(&db, 2).await > s1_read, "a move bumps it");

    // The current sequence (as `current_change_seq` reads it) covers every row.
    let current: i64 = db
        .opt("SELECT CASE WHEN is_called THEN last_value ELSE 0 END AS seq FROM messages_change_seq", params![])
        .await
        .unwrap()
        .unwrap()
        .get(0);
    let max: i64 = db.opt("SELECT MAX(change_seq) FROM messages", params![]).await.unwrap().unwrap().get(0);
    assert!(current >= max);

    // Deletes leave tombstones, one statement or many, and through the account cascade.
    db.exec("DELETE FROM messages WHERE uid = 3", params![]).await.unwrap();
    let t = tombstones(&db).await;
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].1, "INBOX");
    db.exec("DELETE FROM email_accounts WHERE id = $1", params![ACCOUNT]).await.unwrap();
    let t = tombstones(&db).await;
    assert_eq!(t.len(), 3, "account deletion tombstones its remaining rows");
    assert!(t.iter().any(|(_, f)| f == "Archive"));
    let pending: i64 = db
        .opt("SELECT COUNT(*)::bigint FROM message_tombstones WHERE NOT cache_cleared", params![])
        .await
        .unwrap()
        .unwrap()
        .get(0);
    assert_eq!(pending, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_limit_columns() {
    let Some(admin_cfg) = db_config("postgres") else { return };
    let admin = Db::connect(&admin_cfg).expect("admin pool");
    admin.batch("DROP DATABASE IF EXISTS imap_limit_test WITH (FORCE)").await.unwrap();
    admin.batch("CREATE DATABASE imap_limit_test").await.unwrap();
    let db = Db::connect(&db_config("imap_limit_test").unwrap()).unwrap();
    let all = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations");
    inboxora_core::migrations::run(&db, &all).await.expect("migrations");
    db.batch(&format!(
        "INSERT INTO users (id, username, password_hash) VALUES ('{USER}', 'limit@example.test', 'x');
         INSERT INTO email_accounts (id, user_id, name, email_address, imap_host, smtp_host, auth_user, auth_pass)
           VALUES ('{ACCOUNT}', '{USER}', 'L', 'l@example.test', 'imap.example.test', 'smtp.example.test', 'l', 'x');"
    ))
    .await
    .unwrap();

    // The statements the engine and the admin routes run.
    db.exec(
        "UPDATE email_accounts SET imap_conn_limit = $1::int, imap_conn_limit_at = NOW(), imap_conn_limit_reason = $2 WHERE id = $3",
        params![7i32, "Too many connections".to_owned(), ACCOUNT],
    )
    .await
    .unwrap();
    let learned: Option<i16> = db
        .opt("SELECT imap_conn_limit FROM email_accounts WHERE id = $1", params![ACCOUNT])
        .await
        .unwrap()
        .unwrap()
        .get(0);
    assert_eq!(learned, Some(7));

    let upsert = "INSERT INTO imap_server_limits (host, port, start_limit, auto_lower, updated_at, updated_by) VALUES ($1, $2, $3::int, $4, NOW(), $5) \
                  ON CONFLICT (host, port) DO UPDATE SET start_limit = $3::int, auto_lower = $4, updated_at = NOW(), updated_by = $5";
    db.exec(upsert, params!["imap.example.test", 993i32, Some(12i32), false, USER]).await.unwrap();
    db.exec(upsert, params!["imap.example.test", 993i32, None::<i32>, true, USER]).await.unwrap();
    let row = db
        .opt(
            "SELECT start_limit, auto_lower FROM imap_server_limits WHERE host = $1 AND port = $2",
            params!["imap.example.test", 993i32],
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.get::<_, Option<i16>>("start_limit"), None);
    assert!(row.get::<_, bool>("auto_lower"));
    assert!(
        db.exec(upsert, params!["imap.example.test", 143i32, Some(1i32), true, USER]).await.is_err(),
        "below the floor"
    );
}
