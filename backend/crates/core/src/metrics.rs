//! In-memory performance baseline metrics (port of `performanceMetrics.ts`).
//!
//! HTTP latency is aggregated per matched route *pattern* (never a concrete URL, so no ids or
//! PII and bounded cardinality) and DB latency into one histogram. Reset on restart.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{Value, json};

const LATENCY_EDGES_MS: [f64; 9] = [10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0];
const DB_SLOW_MS: f64 = 200.0;

#[derive(Default, Clone)]
struct Histogram {
    count: u64,
    sum_ms: f64,
    max_ms: f64,
    buckets: [u64; LATENCY_EDGES_MS.len() + 1],
}

impl Histogram {
    fn record(&mut self, ms: f64) {
        self.count += 1;
        self.sum_ms += ms;
        if ms > self.max_ms {
            self.max_ms = ms;
        }
        let i = LATENCY_EDGES_MS.iter().position(|edge| ms <= *edge).unwrap_or(LATENCY_EDGES_MS.len());
        self.buckets[i] += 1;
    }

    /// Upper bucket edge containing the percentile, or the observed max for the overflow bucket.
    fn percentile(&self, p: f64) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        let target = ((p / 100.0) * self.count as f64).ceil() as u64;
        let mut cum = 0;
        for (i, n) in self.buckets.iter().enumerate() {
            cum += n;
            if cum >= target {
                return if i < LATENCY_EDGES_MS.len() { LATENCY_EDGES_MS[i] } else { js_round(self.max_ms) };
            }
        }
        js_round(self.max_ms)
    }

    fn summary(&self) -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        m.insert("count".into(), json!(self.count));
        m.insert(
            "meanMs".into(),
            json!(if self.count > 0 { js_round(self.sum_ms / self.count as f64) as i64 } else { 0 }),
        );
        m.insert("p50Ms".into(), json!(self.percentile(50.0) as i64));
        m.insert("p95Ms".into(), json!(self.percentile(95.0) as i64));
        m.insert("maxMs".into(), json!(js_round(self.max_ms) as i64));
        m
    }
}

/// `Math.round` (half rounds toward +∞).
fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

#[derive(Default)]
struct State {
    http: HashMap<String, (Histogram, u64)>,
    db: Histogram,
    db_slow: u64,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::default()));

/// Record one HTTP request under its route key (`"GET /api/mail/messages/:id"`).
pub fn record_http(route_key: &str, elapsed: Duration, is_error: bool) {
    if route_key.is_empty() {
        return;
    }
    let ms = elapsed.as_secs_f64() * 1000.0;
    let mut s = STATE.lock();
    let e = s.http.entry(route_key.to_owned()).or_default();
    e.0.record(ms);
    if is_error {
        e.1 += 1;
    }
}

/// Record one database query.
pub fn record_db(elapsed: Duration) {
    let ms = elapsed.as_secs_f64() * 1000.0;
    let mut s = STATE.lock();
    s.db.record(ms);
    if ms >= DB_SLOW_MS {
        s.db_slow += 1;
    }
}

/// Snapshot for the diagnostics report: the slowest routes (by p95, then volume) and the DB.
pub fn snapshot(top_n: usize) -> Value {
    let s = STATE.lock();
    let mut routes: Vec<(String, u64, Histogram)> =
        s.http.iter().map(|(k, (h, e))| (k.clone(), *e, h.clone())).collect();
    routes.sort_by(|a, b| {
        let (pa, pb) = (a.2.percentile(95.0), b.2.percentile(95.0));
        pb.partial_cmp(&pa).unwrap_or(std::cmp::Ordering::Equal).then(b.2.count.cmp(&a.2.count))
    });
    let route_count = routes.len();
    let slowest: Vec<Value> = routes
        .into_iter()
        .take(top_n)
        .map(|(route, errors, h)| {
            let mut m = serde_json::Map::new();
            m.insert("route".into(), json!(route));
            m.insert("errors".into(), json!(errors));
            m.extend(h.summary());
            Value::Object(m)
        })
        .collect();
    let mut db = s.db.summary();
    db.insert("slowCount".into(), json!(s.db_slow));
    db.insert("slowThresholdMs".into(), json!(DB_SLOW_MS as i64));
    json!({ "http": { "routeCount": route_count, "slowest": slowest }, "db": Value::Object(db) })
}

/// Convert an axum route pattern (`/a/{id}/{*rest}`) to the Express spelling (`/a/:id/*`), so
/// metric keys stay comparable across the migration.
pub fn express_pattern(axum_path: &str) -> String {
    axum_path
        .split('/')
        .map(|seg| {
            if let Some(name) = seg.strip_prefix("{*").and_then(|s| s.strip_suffix('}')) {
                let _ = name;
                "*".to_owned()
            } else if let Some(name) = seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                format!(":{name}")
            } else {
                seg.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_uses_bucket_edges() {
        let mut h = Histogram::default();
        for ms in [5.0, 12.0, 30.0, 40.0, 7000.0] {
            h.record(ms);
        }
        assert_eq!(h.percentile(50.0), 50.0);
        assert_eq!(h.percentile(95.0), 7000.0);
    }

    #[test]
    fn route_patterns_use_express_spelling() {
        assert_eq!(express_pattern("/api/mail/messages/{id}/body"), "/api/mail/messages/:id/body");
    }
}
