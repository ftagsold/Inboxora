//! In-memory diagnostics: recent warnings, broadcast counts, sync signals and WebSocket
//! connection counters (port of `diagnosticsRing.ts`).

use std::collections::{HashMap, VecDeque};
use std::sync::LazyLock;

use parking_lot::Mutex;
use serde_json::{Value, json};

const WARN_CAP: usize = 200;

#[derive(Default)]
struct Ring {
    warnings: VecDeque<(i64, String, Option<String>)>,
    broadcasts: HashMap<String, u64>,
    signals: HashMap<String, Signal>,
    ws_connects: u64,
    ws_disconnects: u64,
    current_sockets: u64,
}

#[derive(Clone)]
struct Signal {
    sig: String,
    account_id: Option<String>,
    count: u64,
    last_t: i64,
    sum_mag: f64,
    max_mag: f64,
}

static RING: LazyLock<Mutex<Ring>> = LazyLock::new(|| Mutex::new(Ring::default()));

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn record_warning(code: &str, account_id: Option<&str>) {
    if code.is_empty() {
        return;
    }
    let mut r = RING.lock();
    r.warnings.push_back((now(), code.to_owned(), account_id.filter(|a| !a.is_empty()).map(str::to_owned)));
    if r.warnings.len() > WARN_CAP {
        r.warnings.pop_front();
    }
}

pub fn record_broadcast(kind: &str) {
    if kind.is_empty() {
        return;
    }
    *RING.lock().broadcasts.entry(kind.to_owned()).or_insert(0) += 1;
}

pub fn record_sync_signal(sig: &str, account_id: Option<&str>, magnitude: Option<f64>) {
    if sig.is_empty() {
        return;
    }
    let account = account_id.filter(|a| !a.is_empty()).map(str::to_owned);
    let key = format!("{sig}|{}", account.as_deref().unwrap_or(""));
    let mut r = RING.lock();
    let s = r.signals.entry(key).or_insert_with(|| Signal {
        sig: sig.to_owned(),
        account_id: account,
        count: 0,
        last_t: 0,
        sum_mag: 0.0,
        max_mag: 0.0,
    });
    s.count += 1;
    s.last_t = now();
    if let Some(m) = magnitude.filter(|m| m.is_finite()) {
        s.sum_mag += m.abs();
        s.max_mag = s.max_mag.max(m.abs());
    }
}

pub fn record_ws_connect() {
    let mut r = RING.lock();
    r.ws_connects += 1;
    r.current_sockets += 1;
}

pub fn record_ws_disconnect() {
    let mut r = RING.lock();
    r.ws_disconnects += 1;
    r.current_sockets = r.current_sockets.saturating_sub(1);
}

pub fn sync_signals() -> Vec<Value> {
    let r = RING.lock();
    let mut v: Vec<Signal> = r.signals.values().cloned().collect();
    v.sort_by(|a, b| b.last_t.cmp(&a.last_t));
    v.into_iter()
        .map(|s| {
            json!({ "sig": s.sig, "accountId": s.account_id, "count": s.count, "lastT": s.last_t,
                    "sumMag": inboxora_core::json::js_float(s.sum_mag), "maxMag": inboxora_core::json::js_float(s.max_mag) })
        })
        .collect()
}

pub fn warnings() -> Vec<Value> {
    let r = RING.lock();
    let mut agg: Vec<(String, Option<String>, u64, i64)> = Vec::new();
    for (t, code, acct) in &r.warnings {
        match agg.iter_mut().find(|(c, a, ..)| c == code && a == acct) {
            Some(e) => {
                e.2 += 1;
                e.3 = e.3.max(*t);
            }
            None => agg.push((code.clone(), acct.clone(), 1, *t)),
        }
    }
    agg.sort_by(|a, b| b.3.cmp(&a.3));
    agg.into_iter()
        .map(|(code, acct, count, last)| json!({ "code": code, "accountId": acct, "count": count, "lastT": last }))
        .collect()
}

pub fn connection_stats() -> Value {
    let r = RING.lock();
    let counts: serde_json::Map<String, Value> = r.broadcasts.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
    json!({
        "wsConnects": r.ws_connects,
        "wsDisconnects": r.ws_disconnects,
        "currentSockets": r.current_sockets,
        "broadcastCounts": counts,
    })
}
