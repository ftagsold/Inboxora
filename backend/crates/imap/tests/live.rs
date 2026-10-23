//! Against a real server: `IMAP_TEST_HOST=127.0.0.1 IMAP_TEST_PORT=31993 IMAP_TEST_PASS=pass`
//! (e.g. the `dovecot/dovecot` image with `USER_PASSWORD=pass`). Skipped when unset.

use std::time::Duration;

use inboxora_imap::{Auth, Client, Config, Event, FetchQuery};

fn config(user: &str, compress: bool) -> Option<Config> {
    let host = std::env::var("IMAP_TEST_HOST").ok()?;
    Some(Config {
        addresses: vec![host.parse().ok()?],
        servername: "localhost".into(),
        port: std::env::var("IMAP_TEST_PORT").ok()?.parse().ok()?,
        secure: true,
        verify_tls: false,
        auth: Auth::Password { user: user.into(), pass: std::env::var("IMAP_TEST_PASS").ok() },
        command_timeout: Duration::from_secs(10),
        compress,
    })
}

const MSG: &str = "From: Alice <alice@example.test>\r\nTo: Bob <bob@example.test>\r\nSubject: =?UTF-8?Q?Gr=C3=BC=C3=9Fe?=\r\nMessage-ID: <m1@example.test>\r\nDate: Tue, 1 Jul 2003 10:52:37 +0200\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=XX\r\n\r\n--XX\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nHall=C3=B6\r\n--XX\r\nContent-Type: application/pdf; name=\"r.pdf\"\r\nContent-Disposition: attachment; filename*=utf-8''%C3%A4.pdf\r\nContent-Transfer-Encoding: base64\r\n\r\nJVBERg==\r\n--XX--\r\n";

#[tokio::test]
async fn session_against_live_server() {
    let user = format!("t{}@example.test", std::process::id());
    let Some(cfg) = config(&user, true) else { return };
    let mut c = Client::connect(&cfg).await.expect("connect");
    assert!(c.has_capability("IMAP4rev1"), "{:?}", c.capabilities());
    let boxes = c.list().await.unwrap();
    assert!(boxes.iter().any(|b| b.path == "INBOX" && b.special_use.as_deref() == Some("\\Inbox")));

    let made = c.create(&["Projekte", "Entwürfe"]).await.unwrap();
    assert!(made.created);
    assert_eq!(made.path, "Projekte/Entwürfe");
    assert!(!c.create(&["Projekte", "Entwürfe"]).await.unwrap().created);

    let (validity, uid) = c.append("INBOX", &["\\Seen"], None, MSG.as_bytes()).await.unwrap().expect("APPENDUID");
    assert!(validity > 0 && uid > 0);
    c.append("INBOX", &[], None, MSG.replace("m1@", "m2@").as_bytes()).await.unwrap();

    let mb = c.select("INBOX").await.unwrap().clone();
    assert_eq!(mb.exists, 2);
    assert!(mb.highest_modseq.is_some());
    let q = FetchQuery {
        flags: true,
        envelope: true,
        body_structure: true,
        internal_date: true,
        size: true,
        headers: true,
        body_parts: vec!["1".into()],
        ..Default::default()
    };
    let msgs = c.fetch("1:*", &q, true, None).await.unwrap();
    assert_eq!(msgs.len(), 2);
    let m = &msgs[0];
    assert_eq!(m.uid, Some(uid));
    assert!(m.has_flag("\\Seen"));
    let env = m.envelope.as_ref().unwrap();
    assert_eq!(env.subject.as_deref(), Some("Grüße"));
    assert_eq!(env.from[0].address, "alice@example.test");
    assert_eq!(env.message_id.as_deref(), Some("<m1@example.test>"));
    let bs = m.body_structure.as_ref().unwrap();
    assert_eq!(bs.mime_type, "multipart/mixed");
    assert_eq!(bs.children[1].part, "2");
    assert_eq!(bs.children[1].filename.as_deref(), Some("ä.pdf"));
    assert_eq!(m.body_parts.get("1").map(|b| b.as_slice()), Some(&b"Hall=C3=B6"[..]));
    assert!(m.headers.as_ref().unwrap().starts_with(b"From:"));
    assert!(m.modseq.is_some());

    let unseen = c.search_uids("UNSEEN").await.unwrap();
    assert_eq!(unseen, vec![uid + 1]);
    c.store_flags(&(uid + 1).to_string(), true, &["\\Flagged"]).await.unwrap();
    let changed =
        c.fetch("1:*", &FetchQuery { flags: true, ..Default::default() }, true, mb.highest_modseq).await.unwrap();
    assert_eq!(changed.len(), 1);
    assert!(changed[0].has_flag("\\Flagged"));

    let moved = c.mv(&(uid + 1).to_string(), "Projekte/Entwürfe").await.unwrap();
    assert_eq!(moved.uid_map.as_ref().map(|m| m.len()), Some(1));
    let st = c.status("Projekte/Entwürfe").await.unwrap();
    assert_eq!(st.messages, Some(1));

    // IDLE sees a delivery from a second session.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    c.set_events(tx);
    let other_cfg = cfg.clone();
    let deliver = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut o = Client::connect(&other_cfg).await.unwrap();
        o.append("INBOX", &[], None, MSG.replace("m1@", "m3@").as_bytes()).await.unwrap();
        o.logout().await;
    });
    let notified = async move {
        loop {
            if let Some(Event::Exists { count, prev_count, .. }) = rx.recv().await {
                break (count, prev_count);
            }
        }
    };
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let waiter = tokio::spawn(async move {
        let got = tokio::time::timeout(Duration::from_secs(5), notified).await;
        let _ = stop_tx.send(());
        got
    });
    c.idle(Duration::from_secs(10), async {
        let _ = stop_rx.await;
    })
    .await
    .unwrap();
    let (count, prev) = waiter.await.unwrap().expect("EXISTS during IDLE");
    assert_eq!((count, prev), (2, 1));
    deliver.await.unwrap();
    // The session is still usable after DONE.
    assert_eq!(c.search_uids("ALL").await.unwrap().len(), 2);

    c.delete_messages(&format!("{uid}")).await.unwrap();
    assert_eq!(c.search_uids("ALL").await.unwrap().len(), 1);
    c.rename_mailbox("Projekte/Entwürfe", "Projekte/Drafts2").await.unwrap();
    c.delete_mailbox("Projekte/Drafts2").await.unwrap();
    c.logout().await;
}

#[tokio::test]
async fn wrong_password_is_an_auth_error() {
    let Some(mut cfg) = config("nobody@example.test", false) else { return };
    cfg.auth = Auth::Password { user: "nobody@example.test".into(), pass: Some("wrong".into()) };
    let err = Client::connect(&cfg).await.err().expect("must fail");
    assert_eq!(err.to_string(), "Authentication failed");
}
