//! Session interop probe used by `tests/interop/session.mjs`.
//! `session_probe load '<Cookie header>'` prints the session data as JSON (or `null`).
//! `session_probe create '<json data>'` stores a new session and prints its Set-Cookie value.
//! `session_probe touch '<Cookie header>' key value` modifies and commits an existing session.

use inboxora_core::session::{Session, SessionStore};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let redis = inboxora_core::redis::connect(&std::env::var("TEST_REDIS_URL").expect("TEST_REDIS_URL"), 1)
        .await
        .expect("redis");
    let store = SessionStore::new(redis, &std::env::var("SESSION_SECRET").expect("SESSION_SECRET"));
    match args[1].as_str() {
        "load" => {
            let s = store.load(Some(&args[2]), false).await.expect("load");
            let data = s.data();
            println!("{}", if data.is_empty() { "null".into() } else { serde_json::to_string(&data).unwrap() });
        }
        "create" => {
            let s = store.load(None, false).await.expect("load");
            let data: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&args[2]).expect("json");
            for (k, v) in data {
                s.set(&k, v);
            }
            println!("{}", store.commit(&s).await.expect("commit").expect("cookie"));
        }
        "touch" => {
            let s: Session = store.load(Some(&args[2]), false).await.expect("load");
            s.set(&args[3], serde_json::Value::String(args[4].clone()));
            let cookie = store.commit(&s).await.expect("commit");
            println!("{}", cookie.unwrap_or_default());
        }
        other => panic!("unknown command {other}"),
    }
}
