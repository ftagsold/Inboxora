//! End-to-end engine tests against PostgreSQL and a real IMAP server. They run when
//! `TEST_DB_HOST` (see `.env.test`) and `IMAP_TEST_HOST`/`IMAP_TEST_PORT`/`IMAP_TEST_PASS`
//! (e.g. the `dovecot/dovecot` image with `USER_PASSWORD`) are set, and skip otherwise.

use std::time::Duration;

use inboxora_core::config::DbConfig;
use inboxora_core::events::Events;
use inboxora_core::params;
use inboxora_core::pg::{Db, Q};
use inboxora_imap::{Auth, Client, Config, FetchQuery};
use serde_json::Value;

use super::Engine;
use super::connect::{Account, load_account};

fn db_config(database: &str) -> Option<DbConfig> {
    Some(DbConfig {
        host: std::env::var("TEST_DB_HOST").ok()?,
        port: std::env::var("TEST_DB_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(5432),
        database: database.into(),
        user: std::env::var("TEST_DB_USER").unwrap_or_else(|_| "mailflow".into()),
        password: std::env::var("TEST_DB_PASSWORD").ok(),
        pool_max: 8,
        statement_timeout_ms: 30_000,
    })
}

struct Imap {
    host: String,
    port: u16,
    pass: String,
}

fn imap() -> Option<Imap> {
    Some(Imap {
        host: std::env::var("IMAP_TEST_HOST").ok()?,
        port: std::env::var("IMAP_TEST_PORT").ok()?.parse().ok()?,
        pass: std::env::var("IMAP_TEST_PASS").ok()?,
    })
}

async fn fresh_db(name: &str) -> Option<Db> {
    let admin = Db::connect(&db_config("postgres")?).expect("admin pool");
    admin.batch(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")).await.expect("drop test database");
    admin.batch(&format!("CREATE DATABASE {name}")).await.expect("create test database");
    let db = Db::connect(&db_config(name)?).expect("test pool");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../backend/migrations");
    inboxora_core::migrations::run(&db, &dir).await.expect("migrations");
    Some(db)
}

fn msg(id: &str, subject: &str, refs: Option<&str>) -> String {
    let mut h = format!(
        "From: Alice Example <alice@example.test>\r\nTo: Bob <bob@example.test>\r\nSubject: {subject}\r\nMessage-ID: <{id}@example.test>\r\nDate: Tue, 1 Jul 2025 10:52:37 +0200\r\nMIME-Version: 1.0\r\n"
    );
    if let Some(r) = refs {
        h.push_str(&format!("In-Reply-To: {r}\r\nReferences: {r}\r\n"));
    }
    h.push_str("Content-Type: multipart/alternative; boundary=B\r\n\r\n--B\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nHello plain\r\n--B\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Hello <b>html</b></p>\r\n--B--\r\n");
    h
}

async fn other_client(i: &Imap, user: &str) -> Client {
    Client::connect(&Config {
        addresses: vec![i.host.parse().expect("ip")],
        servername: "localhost".into(),
        port: i.port,
        secure: true,
        verify_tls: false,
        auth: Auth::Password { user: user.into(), pass: Some(i.pass.clone()) },
        command_timeout: Duration::from_secs(10),
        compress: false,
    })
    .await
    .expect("imap connect")
}

async fn wait_for(db: &Db, sql: &str, p: &[inboxora_core::pg::Val], want: i64) -> i64 {
    let mut got = -1;
    for _ in 0..100 {
        got = db.opt(sql, p).await.ok().flatten().map_or(-1, |r| r.get::<_, i64>(0));
        if got == want {
            return got;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    got
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_end_to_end() {
    let Some(i) = imap() else { return };
    let Some(db) = fresh_db("engine_live_test").await else { return };
    eprintln!("engine_end_to_end: running against {}:{}", i.host, i.port);
    let _ = inboxora_core::crypto::init_key(&"ab".repeat(32));
    let user_mail = format!("e2e{}@example.test", std::process::id());
    db.batch(
        "INSERT INTO system_settings (key, value) VALUES ('allow_private_hosts', 'true'), ('allow_insecure_tls', 'true') \
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .await
    .unwrap();
    let user: uuid::Uuid = db
        .opt("INSERT INTO users (username, password_hash) VALUES ($1, 'x') RETURNING id", params![&user_mail])
        .await
        .unwrap()
        .unwrap()
        .get("id");
    let pass = inboxora_core::crypto::encrypt(&i.pass).unwrap();
    let account_id: uuid::Uuid = db
        .opt(
            "INSERT INTO email_accounts (user_id, name, email_address, imap_host, imap_port, imap_tls, imap_skip_tls_verify, smtp_host, auth_user, auth_pass) \
             VALUES ($1, 'E2E', $2, $3, $4, true, true, $3, $2, $5) RETURNING id",
            params![user, &user_mail, &i.host, i64::from(i.port), pass],
        )
        .await
        .unwrap()
        .unwrap()
        .get("id");
    let account_id = account_id.to_string();

    // Seed the mailbox: a thread of two plus one unrelated message.
    let mut other = other_client(&i, &user_mail).await;
    other.append("INBOX", &["\\Seen"], None, msg("root", "Plan", None).as_bytes()).await.unwrap();
    other.append("INBOX", &[], None, msg("reply", "Re: Plan", Some("<root@example.test>")).as_bytes()).await.unwrap();
    other.append("INBOX", &[], None, msg("solo", "Other", None).as_bytes()).await.unwrap();
    other.create(&["Archive"]).await.unwrap();

    let engine = Engine::new(db.clone(), Events::new(None));
    let account = load_account(&db, &account_id).await.unwrap().unwrap();
    assert!(engine.connect_account(account.clone()).await, "connect");
    assert!(engine.is_connected(&account_id));

    let count = "SELECT COUNT(*)::bigint FROM messages WHERE account_id = $1::uuid AND folder = 'INBOX'";
    assert_eq!(wait_for(&db, count, params![&account_id], 3).await, 3, "initial sync");
    let convs = db
        .opt(
            "SELECT COUNT(DISTINCT conversation_id)::bigint FROM messages WHERE account_id = $1::uuid AND folder = 'INBOX'",
            params![&account_id],
        )
        .await
        .unwrap()
        .unwrap()
        .get::<_, i64>(0);
    assert_eq!(convs, 2, "the reply joins its root's conversation");
    let folders: Vec<String> = db
        .rows("SELECT path FROM folders WHERE account_id = $1::uuid ORDER BY path", params![&account_id])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get("path"))
        .collect();
    assert!(folders.contains(&"INBOX".to_owned()) && folders.contains(&"Archive".to_owned()), "{folders:?}");
    let read: bool = db
        .opt(
            "SELECT is_read FROM messages WHERE account_id = $1::uuid AND message_id = '<root@example.test>'",
            params![&account_id],
        )
        .await
        .unwrap()
        .unwrap()
        .get("is_read");
    assert!(read);

    // New mail pushed over IDLE.
    other.append("INBOX", &[], None, msg("pushed", "Pushed", None).as_bytes()).await.unwrap();
    assert_eq!(wait_for(&db, count, params![&account_id], 4).await, 4, "IDLE push");

    // A flag change made elsewhere arrives through the flag sync.
    other.select("INBOX").await.unwrap();
    let solo_uid = other.search_uids("HEADER Message-ID \"solo@example.test\"").await.unwrap()[0];
    other.store_flags(&solo_uid.to_string(), true, &["\\Seen", "\\Flagged"]).await.unwrap();
    engine.sync_flags_for_range(&account).await;
    let starred = "SELECT COUNT(*)::bigint FROM messages WHERE account_id = $1::uuid AND is_starred AND is_read";
    assert_eq!(wait_for(&db, starred, params![&account_id], 1).await, 1, "flag pull");

    // Body fetch, flag push, move with UIDPLUS, and append.
    let row = db
        .opt(
            "SELECT uid FROM messages WHERE account_id = $1::uuid AND message_id = '<reply@example.test>'",
            params![&account_id],
        )
        .await
        .unwrap()
        .unwrap();
    let uid: i64 = row.get("uid");
    let body = engine.fetch_message_body(&account, uid, "INBOX").await.unwrap();
    assert!(body.html.as_deref().is_some_and(|h| h.contains("<b>html</b>")), "{body:?}");
    assert_eq!(body.text.as_deref().map(str::trim), Some("Hello plain"));
    engine.set_flag(&account, uid, "INBOX", "\\Flagged", true).await.unwrap();
    let moved = engine.bulk_move_messages(&account, &[uid as u32], "INBOX", "Archive").await;
    assert_eq!(moved.succeeded, vec![uid as u32]);
    assert!(moved.uid_map.contains_key(&(uid as u32)));
    let (appended, validity) = engine
        .append_to_folder(&account, "Archive", msg("app", "Appended", None).as_bytes(), &["\\Seen"])
        .await
        .unwrap();
    assert!(appended.is_some() && validity.is_some());

    // A delete made elsewhere is reconciled away.
    other.select("INBOX").await.unwrap();
    let pushed_uid = other.search_uids("HEADER Message-ID \"pushed@example.test\"").await.unwrap()[0];
    other.delete_messages(&pushed_uid.to_string()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    engine.reconcile_deletes(&account).await.unwrap();
    let gone =
        "SELECT COUNT(*)::bigint FROM messages WHERE account_id = $1::uuid AND message_id = '<pushed@example.test>'";
    assert_eq!(wait_for(&db, gone, params![&account_id], 0).await, 0, "reconcile");

    // On-demand sync of another folder picks up the moved and appended copies.
    engine.sync_folder_on_demand(&account, "Archive").await;
    let archive = "SELECT COUNT(*)::bigint FROM messages WHERE account_id = $1::uuid AND folder = 'Archive'";
    assert_eq!(wait_for(&db, archive, params![&account_id], 2).await, 2, "archive sync");

    // Inbox rules run on new mail: mark Notices read and move them to Archive.
    db.exec(
        "INSERT INTO inbox_rules (user_id, account_id, name, conditions, actions, enabled) VALUES ($1, $2::uuid, 'r', $3::jsonb, $4::jsonb, true)",
        params![
            user,
            &account_id,
            r#"[{"field":"subject","operator":"contains","value":"notice"}]"#,
            r#"[{"type":"mark_read"},{"type":"move","value":"Archive"}]"#
        ],
    )
    .await
    .unwrap();
    other.append("INBOX", &[], None, msg("notice", "Weekly Notice", None).as_bytes()).await.unwrap();
    let ruled = "SELECT COUNT(*)::bigint FROM messages WHERE account_id = $1::uuid AND message_id = '<notice@example.test>' AND folder = 'Archive' AND is_read";
    assert_eq!(wait_for(&db, ruled, params![&account_id], 1).await, 1, "rule moved the new message");

    engine.disconnect_account(&account_id).await;
    assert!(!engine.is_connected(&account_id));
    other.logout().await;
    let _: Option<Value> = None;
    let _ = FetchQuery::default();
    let _ = Account::from_map(Default::default());
}
