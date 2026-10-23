//! `/api/diagnostics`: the sanitised, allowlisted report sections the server owns (port of
//! `routes/diagnostics.ts` and `services/diagnosticsReport.ts`). Ids become salted per-report
//! hashes; a PII pattern scrub runs over the assembled report as a second safety net.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Instant;

use axum::Router;
use axum::extract::State;
use axum::response::Response;
use axum::routing::post;
use fred::prelude::*;
use inboxora_core::pg::Q;
use inboxora_core::{Result, crypto, params};
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::http::{Auth, Body};
use crate::routes::util::{err, ok};
use crate::services::diagnostics_ring;
use crate::state::AppState;

/// Process start, forced at startup for `uptimeSeconds`.
pub static STARTED: LazyLock<Instant> = LazyLock::new(Instant::now);

pub fn router() -> Router<AppState> {
    Router::new().route("/report", post(report))
}

/// `hashRef`: salted, truncated, not reversible.
pub fn hash_ref(id: &str, salt: &str) -> String {
    crypto::sha256_hex(format!("{salt}:{id}"))[..8].to_owned()
}

const STANDARD_FOLDERS: [&str; 16] = [
    "INBOX",
    "Sent",
    "Drafts",
    "Trash",
    "Junk",
    "Spam",
    "Archive",
    "Snoozed",
    "Outbox",
    "Deleted Items",
    "Deleted Messages",
    "Junk E-Mail",
    "Sent Items",
    "Sent Messages",
    "Notes",
    "Starred",
];

/// `folderLabel`: standard and special-use names verbatim, custom names hashed.
pub fn folder_label(name: &str, special_use: Option<&str>, salt: &str) -> String {
    if name.is_empty() {
        return format!("custom:{}", hash_ref("(unnamed)", salt));
    }
    if STANDARD_FOLDERS.contains(&name)
        || name.starts_with("[Gmail]/")
        || name.starts_with("[Google Mail]/")
        || special_use.is_some_and(|s| !s.is_empty())
    {
        return name.to_owned();
    }
    format!("custom:{}", hash_ref(name, salt))
}

/// `deriveProvider`: a coarse service label, never the raw host.
pub fn derive_provider(imap_host: Option<&str>, oauth: Option<&str>) -> String {
    if let Some(o) = oauth.filter(|o| !o.is_empty()) {
        return o.to_owned();
    }
    let h = imap_host.unwrap_or("").to_lowercase();
    if h.is_empty() {
        return "unknown".into();
    }
    static RULES: LazyLock<Vec<(Regex, &str)>> = LazyLock::new(|| {
        [
            (r"google|gmail", "gmail"),
            (r"mail\.me\.com|icloud", "icloud"),
            (r"outlook|office365|hotmail|live\.com", "outlook"),
            (r"yahoo", "yahoo"),
            (r"fastmail|messagingengine", "fastmail"),
            (r"purelymail", "purelymail"),
            (r"\bzoho\b", "zoho"),
            (r"proton", "proton"),
        ]
        .into_iter()
        .map(|(p, n)| (Regex::new(p).expect("regex"), n))
        .collect()
    });
    RULES.iter().find(|(re, _)| re.is_match(&h)).map_or("imap", |(_, n)| n).to_owned()
}

/// `categorizeSyncError`.
pub fn categorize_sync_error(e: Option<&str>) -> &'static str {
    let Some(e) = e.filter(|e| !e.is_empty()) else { return "none" };
    static RULES: LazyLock<Vec<(Regex, &str)>> = LazyLock::new(|| {
        [
            (r"auth|login|password|credential|invalid.*(user|pass)|xoauth|token|535|534", "auth"),
            (r"timeout|timed out|etimedout", "timeout"),
            (r"enotfound|getaddrinfo|eai_again|\bdns\b", "dns"),
            (r"tls|ssl|certificate|\bcert\b|self.signed", "tls"),
            (r"econnrefused|econnreset|socket|network|ehostunreach|connect", "connection"),
        ]
        .into_iter()
        .map(|(p, n)| (Regex::new(p).expect("regex"), n))
        .collect()
    });
    let s = e.to_lowercase();
    RULES.iter().find(|(re, _)| re.is_match(&s)).map_or("other", |(_, n)| n)
}

static PII: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"[\w.+-]+@[\w-]+\.[\w.-]+",
        r"\b\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}\b",
        r"\beyJ[\w-]+\.[\w-]+\.[\w-]+",
        r"\b[A-Fa-f0-9]{32,}\b",
        r"(?i)(ghp_|gho_|sk-|xoxb-|Bearer\s+\S)",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("regex"))
    .collect()
});

/// `scrubReport`: strings matching a PII pattern become `[redacted]`.
pub fn scrub(v: &Value, hits: &mut u64) -> Value {
    match v {
        Value::String(s) if PII.iter().any(|re| re.is_match(s)) => {
            *hits += 1;
            json!("[redacted]")
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| scrub(x, hits)).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), scrub(x, hits))).collect()),
        other => other.clone(),
    }
}

fn age_seconds(last_ms: i64) -> i64 {
    ((chrono::Utc::now().timestamp_millis() - last_ms) as f64 / 1000.0).round() as i64
}

/// `buildServerReport`.
async fn build_report(s: &AppState, user: &str, salt: &str) -> Result<Value> {
    let accounts =
        s.db.json_rows(
            "SELECT id, protocol, oauth_provider, imap_host, enabled, include_in_unified_inbox, last_sync, sync_error \
             FROM email_accounts WHERE user_id = $1 ORDER BY sort_order NULLS LAST, created_at",
            params![user],
        )
        .await?;
    let folders = s
        .db
        .json_rows(
            "SELECT f.account_id, f.name, f.special_use, f.total_count, f.unread_count FROM folders f JOIN email_accounts a ON a.id = f.account_id WHERE a.user_id = $1",
            params![user],
        )
        .await?;
    let unread: HashMap<String, i64> = s
        .db
        .rows(
            "SELECT m.account_id, COUNT(*)::int AS count FROM messages m JOIN email_accounts a ON a.id = m.account_id \
             WHERE a.user_id = $1 AND a.enabled = true AND m.folder = 'INBOX' AND m.is_read = false AND m.is_deleted = false GROUP BY m.account_id",
            params![user],
        )
        .await?
        .iter()
        .map(|r| (r.get::<_, uuid::Uuid>("account_id").to_string(), i64::from(r.get::<_, i32>("count"))))
        .collect();
    let str_of = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    let now = chrono::Utc::now();
    let (mut out_accounts, mut out_folders, mut by_ref) = (Vec::new(), Vec::new(), Map::new());
    let mut unread_total = 0;
    let mut ids: Vec<String> = Vec::new();
    for a in &accounts {
        let id = str_of(a, "id").unwrap_or_default();
        ids.push(id.clone());
        let r = hash_ref(&id, salt);
        let own: Vec<&Value> =
            folders.iter().filter(|f| f.get("account_id").and_then(Value::as_str) == Some(id.as_str())).collect();
        let count = unread.get(&id).copied().unwrap_or(0);
        let unified = a.get("include_in_unified_inbox") != Some(&Value::Bool(false));
        let last_sync = str_of(a, "last_sync").and_then(|d| chrono::DateTime::parse_from_rfc3339(&d).ok());
        out_accounts.push(json!({
            "ref": r,
            "provider": derive_provider(str_of(a, "imap_host").as_deref(), str_of(a, "oauth_provider").as_deref()),
            "authType": if str_of(a, "oauth_provider").is_some_and(|o| !o.is_empty()) { "oauth" } else { "password" },
            "enabled": a.get("enabled") != Some(&Value::Bool(false)),
            "inUnifiedInbox": unified,
            "unread": count,
            "folderCount": own.len(),
            "lastSyncAgeSeconds": last_sync.map(|t| ((now.timestamp_millis() - t.timestamp_millis()) as f64 / 1000.0).round() as i64),
            "syncErrorCategory": categorize_sync_error(str_of(a, "sync_error").as_deref()),
        }));
        if unified {
            unread_total += count;
        }
        by_ref.insert(r.clone(), json!(count));
        for f in own {
            out_folders.push(json!({
                "accountRef": r,
                "name": folder_label(&str_of(f, "name").unwrap_or_default(), str_of(f, "special_use").as_deref(), salt),
                "total": f.get("total_count").and_then(Value::as_i64).unwrap_or(0),
                "unread": f.get("unread_count").and_then(Value::as_i64).unwrap_or(0),
            }));
        }
    }
    let (mut ai_enabled, mut ai_provider) = (false, Value::Null);
    if let Ok(Some(c)) = crate::services::ai::load_config(&s.db).await {
        ai_enabled = c["enabled"] == json!(true);
        ai_provider =
            c.get("provider").filter(|p| p.as_str().is_some_and(|p| !p.is_empty())).cloned().unwrap_or(Value::Null);
    }
    let db_ok = s.db.opt("SELECT 1", params![]).await.is_ok();
    let redis_ok = s.redis.ping::<String>(None).await.is_ok();
    let mine = |acct: Option<&str>| acct.is_none_or(|a| ids.iter().any(|i| i == a));
    let warnings: Vec<Value> = diagnostics_ring::warnings()
        .into_iter()
        .filter(|w| mine(w.get("accountId").and_then(Value::as_str)))
        .map(|w| {
            let mut m = Map::new();
            m.insert("code".into(), w["code"].clone());
            if let Some(a) = w.get("accountId").and_then(Value::as_str) {
                m.insert("accountRef".into(), json!(hash_ref(a, salt)));
            }
            m.insert("count".into(), w["count"].clone());
            m.insert("lastSeenAgeSeconds".into(), json!(age_seconds(w["lastT"].as_i64().unwrap_or(0))));
            Value::Object(m)
        })
        .collect();
    let signals: Vec<Value> = diagnostics_ring::sync_signals()
        .into_iter()
        .filter(|x| mine(x.get("accountId").and_then(Value::as_str)))
        .map(|x| {
            let mut m = Map::new();
            m.insert("signal".into(), x["sig"].clone());
            if let Some(a) = x.get("accountId").and_then(Value::as_str) {
                m.insert("accountRef".into(), json!(hash_ref(a, salt)));
            }
            m.insert("count".into(), x["count"].clone());
            if x["sumMag"].as_f64().is_some_and(|v| v != 0.0) {
                m.insert("totalMagnitude".into(), x["sumMag"].clone());
                m.insert("maxMagnitude".into(), x["maxMag"].clone());
            }
            m.insert("lastSeenAgeSeconds".into(), json!(age_seconds(x["lastT"].as_i64().unwrap_or(0))));
            Value::Object(m)
        })
        .collect();
    Ok(json!({
        "versions": { "backend": s.cfg.app_version, "gitSha": s.cfg.build_sha },
        "server": { "uptimeSeconds": STARTED.elapsed().as_secs_f64().round() as i64, "dbOk": db_ok, "redisOk": redis_ok },
        "accounts": out_accounts,
        "folders": out_folders,
        "counts": { "unreadTotal": unread_total, "unreadByAccountRef": by_ref },
        "warnings": warnings,
        "syncSignals": signals,
        "connection": diagnostics_ring::connection_stats(),
        "performance": inboxora_core::metrics::snapshot(12),
        "config": { "aiEnabled": ai_enabled, "aiProvider": ai_provider, "plugins": {} },
    }))
}

async fn report(State(s): State<AppState>, a: Auth, b: Body) -> Result<Response> {
    static SALT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^[0-9a-f]{16,64}$").expect("regex"));
    let salt = b.str("salt").unwrap_or("");
    if !SALT.is_match(salt) {
        return Ok(err(400, "salt must be 16-64 hex characters"));
    }
    let report = build_report(&s, &a.user_id, salt).await?;
    let mut hits = 0;
    let mut scrubbed = scrub(&report, &mut hits);
    scrubbed["scrub"] = json!({ "fieldsDropped": 0, "hitsRedacted": hits });
    Ok(ok(scrubbed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_scrubs() {
        assert_eq!(folder_label("INBOX", None, "s"), "INBOX");
        assert_eq!(folder_label("Clients", Some("\\Archive"), "s"), "Clients");
        assert!(folder_label("Clients", None, "s").starts_with("custom:"));
        assert_eq!(derive_provider(Some("imap.gmail.com"), None), "gmail");
        assert_eq!(derive_provider(Some("mail.corp.test"), None), "imap");
        assert_eq!(derive_provider(None, Some("microsoft")), "microsoft");
        assert_eq!(categorize_sync_error(Some("getaddrinfo ENOTFOUND x")), "dns");
        assert_eq!(categorize_sync_error(Some("Invalid credentials")), "auth");
        let mut hits = 0;
        let out = scrub(&json!({ "a": ["x@y.co", "fine"], "b": "Bearer abc" }), &mut hits);
        assert_eq!(out, json!({ "a": ["[redacted]", "fine"], "b": "[redacted]" }));
        assert_eq!(hits, 2);
    }
}
