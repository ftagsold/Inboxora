//! Calendar projection, occurrence materialisation and contact-date events (ports of
//! calendarProjectionPool.ts, calendarOccurrences.ts and helpers in routes/calendar.ts).

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use chrono::{DateTime, Datelike, TimeZone, Utc};
use inboxora_core::config::env;
use inboxora_core::pg::{Db, Q, Val};
use inboxora_core::{Result, crypto, params};
use inboxora_pim::ical::{self, ProjectOptions, ProjectStatus, Projected};
use parking_lot::Mutex;
use rayon::prelude::*;
use serde_json::{Map, Value, json};

pub const EVENT_COLUMNS: &str = "e.id, e.user_id, e.calendar_id, e.uid, e.etag, e.raw_ical, e.summary, e.description, \
  e.location, e.url, e.organizer, e.attendees, e.starts_at, e.ends_at, e.all_day, e.timezone, e.cancellation_outbox_id";

/// Rows not (fully) covered by materialised occurrences for the window `$2`..`$3`.
pub fn coverage_predicate(alias: &str) -> String {
    format!("({alias}.event_id IS NULL OR {alias}.dirty OR {alias}.built_from > $2 OR {alias}.built_to < $3)")
}

fn env_int(name: &str, fallback: i64, min: i64, max: i64) -> i64 {
    env(name).and_then(|v| crate::http::query::parse_int(&v)).map_or(fallback, |v| v.clamp(min, max))
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Failure {
    pub id: Value,
    pub error: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct Aggregate {
    pub events: Vec<Projected>,
    pub failures: Vec<Failure>,
    pub truncated: bool,
}

struct CacheEntry {
    status: ProjectStatus,
    expires: Instant,
}

static CACHE: LazyLock<Mutex<(HashMap<String, CacheEntry>, std::collections::VecDeque<String>, usize)>> =
    LazyLock::new(|| Mutex::new((HashMap::new(), std::collections::VecDeque::new(), 0)));

fn month_index(t: &DateTime<Utc>) -> i64 {
    i64::from(t.year()) * 12 + i64::from(t.month0())
}

fn month_start(abs: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(abs.div_euclid(12) as i32, (abs.rem_euclid(12) + 1) as u32, 1, 0, 0, 0)
        .single()
        .unwrap_or_default()
}

fn filter_window(status: &ProjectStatus, from: DateTime<Utc>, to: DateTime<Utc>) -> ProjectStatus {
    let mut s = status.clone();
    s.events.retain(|e| e.starts_at < to && e.ends_at > from);
    s
}

fn project_one(
    row: &Map<String, Value>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    max_iterations: u64,
    deadline: Instant,
) -> std::result::Result<ProjectStatus, String> {
    let opts = ProjectOptions { max_iterations: Some(max_iterations), deadline: Some(deadline), ..Default::default() };
    ical::project_calendar_resource_with_status(row, from, to, &opts)
}

/// `projectCalendarResources(rows, from, to, { userId, cache })`.
pub async fn project_resources(
    rows: Vec<Map<String, Value>>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    user_id: Option<&str>,
    use_cache: bool,
) -> Aggregate {
    if rows.is_empty() {
        return Aggregate::default();
    }
    let max_iterations =
        env_int("CALENDAR_PROJECTION_MAX_ITERATIONS", ical::DEFAULT_MAX_ITERATIONS as i64, 1, i64::MAX) as u64;
    let timeout = Duration::from_millis(env_int("CALENDAR_PROJECTION_TIMEOUT_MS", 5000, 100, 120_000) as u64);
    let cache_enabled = use_cache && env("CALENDAR_PROJECTION_CACHE_DISABLED").as_deref() != Some("1");
    let forward = env_int("CALENDAR_PROJECTION_HORIZON_FORWARD_MONTHS", 0, 0, 12);
    let ttl = Duration::from_millis(env_int("CALENDAR_PROJECTION_CACHE_TTL_MS", 1_800_000, 1000, 86_400_000) as u64);
    let failure_ttl =
        Duration::from_millis(env_int("CALENDAR_PROJECTION_FAILURE_CACHE_TTL_MS", 30_000, 1000, 86_400_000) as u64);
    let (lo, hi) = (from.min(to), from.max(to));
    let (start_m, last_m) = (month_index(&lo), month_index(&hi));
    let horizon =
        (format!("m{start_m}:{forward}"), month_start(start_m), month_start((last_m + 1).max(start_m + 1 + forward)));
    let key_of = |row: &Map<String, Value>| {
        let s =
            |k: &str| row.get(k).map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned)).unwrap_or_default();
        format!(
            "{}\u{0}{}\u{0}{}\u{0}{}\u{0}{max_iterations}\u{0}1",
            user_id.unwrap_or(""),
            s("id"),
            s("etag"),
            horizon.0
        )
    };
    let mut statuses: Vec<Option<ProjectStatus>> = vec![None; rows.len()];
    let mut missing: Vec<usize> = Vec::new();
    if cache_enabled {
        let mut c = CACHE.lock();
        for (i, row) in rows.iter().enumerate() {
            let k = key_of(row);
            match c.0.get(&k) {
                Some(e) if e.expires > Instant::now() => statuses[i] = Some(filter_window(&e.status, from, to)),
                Some(_) => {
                    if let Some(old) = c.0.remove(&k) {
                        c.2 -= old.status.events.len();
                    }
                    missing.push(i);
                }
                None => missing.push(i),
            }
        }
    } else {
        missing = (0..rows.len()).collect();
    }
    let (pfrom, pto) = if cache_enabled { (horizon.1, horizon.2) } else { (from, to) };
    let jobs: Vec<(usize, Map<String, Value>)> = missing.iter().map(|i| (*i, rows[*i].clone())).collect();
    let computed: Vec<(usize, std::result::Result<ProjectStatus, String>)> = tokio::task::spawn_blocking(move || {
        jobs.into_par_iter()
            .map(|(i, row)| (i, project_one(&row, pfrom, pto, max_iterations, Instant::now() + timeout)))
            .collect()
    })
    .await
    .unwrap_or_default();
    let mut agg = Aggregate::default();
    let mut errors: HashMap<usize, String> = HashMap::new();
    for (i, result) in computed {
        match result {
            Ok(status) => {
                if cache_enabled {
                    let mut c = CACHE.lock();
                    let max_entries = env_int("CALENDAR_PROJECTION_CACHE_ENTRIES", 500, 1, 100_000) as usize;
                    let max_events = env_int("CALENDAR_PROJECTION_CACHE_MAX_EVENTS", 50_000, 1, i64::MAX) as usize;
                    let k = key_of(&rows[i]);
                    c.2 += status.events.len();
                    let expires = Instant::now() + if status.truncated { failure_ttl } else { ttl };
                    if let Some(old) = c.0.insert(k.clone(), CacheEntry { status: status.clone(), expires }) {
                        c.2 -= old.status.events.len();
                    } else {
                        c.1.push_back(k);
                    }
                    while c.0.len() > max_entries || c.2 > max_events {
                        let Some(oldest) = c.1.pop_front() else { break };
                        if let Some(old) = c.0.remove(&oldest) {
                            c.2 -= old.status.events.len();
                        }
                    }
                }
                statuses[i] = Some(if cache_enabled { filter_window(&status, from, to) } else { status });
            }
            Err(e) => {
                errors.insert(i, e);
            }
        }
    }
    for (i, row) in rows.iter().enumerate() {
        let id = row.get("id").cloned().unwrap_or(Value::Null);
        if let Some(e) = errors.remove(&i) {
            agg.failures.push(Failure { id, error: e, reason: "rule-error".into() });
            continue;
        }
        if let Some(status) = statuses[i].take() {
            if status.truncated {
                agg.failures.push(Failure {
                    id,
                    error: status.error.clone().unwrap_or_else(|| "Calendar projection was truncated".into()),
                    reason: status.reason.unwrap_or("truncated").to_owned(),
                });
            }
            agg.events.extend(status.events);
        }
    }
    agg.truncated = !agg.failures.is_empty();
    agg
}

/// `occurrenceHorizon()`.
pub fn occurrence_horizon(now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    let past = env_int("CALENDAR_OCCURRENCE_HORIZON_PAST_MONTHS", 3, 0, 120);
    let future = env_int("CALENDAR_OCCURRENCE_HORIZON_FUTURE_MONTHS", 18, 1, 120);
    let m = month_index(&now);
    (month_start(m - past), month_start(m + future + 1))
}

fn overrides(event: &Map<String, Value>, row: &Map<String, Value>) -> Vec<Val> {
    let pick = |field: &str| -> Val {
        let master = row.get(field).cloned().unwrap_or(Value::Null);
        let current = event.get(field).cloned().unwrap_or(Value::Null);
        let same = if master.is_array() || current.is_array() {
            let norm = |v: &Value| if v.is_null() { json!([]) } else { v.clone() };
            inboxora_core::json::stringify(&norm(&master)) == inboxora_core::json::stringify(&norm(&current))
        } else {
            master == current
        };
        if same { Val::Null } else { Val::from(current) }
    };
    let attendees = match event.get("attendees") {
        Some(a @ Value::Array(_))
            if inboxora_core::json::stringify(a)
                != inboxora_core::json::stringify(row.get("attendees").unwrap_or(&json!([]))) =>
        {
            Val::from(inboxora_core::json::stringify(a))
        }
        _ => Val::Null,
    };
    vec![pick("summary"), pick("description"), pick("location"), pick("url"), pick("organizer"), attendees]
}

/// `materializeEvent`.
pub async fn materialize_event(
    db: &Db,
    event_id: &str,
    horizon: (DateTime<Utc>, DateTime<Utc>),
) -> Result<&'static str> {
    let Some(row) = db
        .json_opt(&format!("SELECT {EVENT_COLUMNS} FROM calendar_events e WHERE e.id = $1"), params![event_id])
        .await?
    else {
        return Ok("skipped");
    };
    let user = row.get("user_id").and_then(Value::as_str).map(str::to_owned);
    let projection = project_resources(vec![row.clone()], horizon.0, horizon.1, user.as_deref(), false).await;
    let calendar_id = row.get("calendar_id").cloned().unwrap_or(Value::Null);
    let timezone = row.get("timezone").cloned().unwrap_or(Value::Null);
    let etag = row.get("etag").cloned().unwrap_or(Value::Null);
    let truncated = projection.truncated;
    let events = projection.events;
    let id = event_id.to_owned();
    db.transaction(false, 0, move |tx| {
        let (events, row, id) = (events.clone(), row.clone(), id.clone());
        let (calendar_id, timezone, etag, user) = (calendar_id.clone(), timezone.clone(), etag.clone(), user.clone());
        Box::pin(async move {
            tx.exec("DELETE FROM calendar_occurrences WHERE event_id = $1", params![&id]).await?;
            for e in &events {
                let mut p = vec![
                    Val::from(&id),
                    Val::from(calendar_id.clone()),
                    Val::from(user.clone()),
                    Val::from(e.fields.get("recurrence_id").and_then(Value::as_str).unwrap_or("")),
                    Val::Time(e.starts_at),
                    Val::Time(e.ends_at),
                    Val::Bool(inboxora_core::json::truthy(e.fields.get("all_day"))),
                    Val::from(timezone.clone()),
                ];
                p.extend(overrides(&e.fields, &row));
                tx.exec(
                    "INSERT INTO calendar_occurrences (event_id, calendar_id, user_id, recurrence_id, starts_at, ends_at, all_day, timezone, \
                     summary, description, location, url, organizer, attendees) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14::jsonb) \
                     ON CONFLICT (event_id, recurrence_id) DO UPDATE SET starts_at = EXCLUDED.starts_at, ends_at = EXCLUDED.ends_at, \
                     all_day = EXCLUDED.all_day, timezone = EXCLUDED.timezone, summary = EXCLUDED.summary, description = EXCLUDED.description, \
                     location = EXCLUDED.location, url = EXCLUDED.url, organizer = EXCLUDED.organizer, attendees = EXCLUDED.attendees",
                    &p,
                )
                .await?;
            }
            tx.exec(
                "UPDATE calendar_occurrence_state s SET built_from = $2, built_to = $3, built_etag = $4, dirty = ($5 OR e.etag IS DISTINCT FROM $4), \
                 updated_at = NOW() FROM calendar_events e WHERE s.event_id = e.id AND s.event_id = $1",
                &[Val::from(&id), Val::Time(horizon.0), Val::Time(horizon.1), Val::from(etag.clone()), Val::Bool(truncated)],
            )
            .await?;
            Ok(())
        })
    })
    .await?;
    Ok(if truncated { "truncated" } else { "built" })
}

/// `materializePendingOccurrences`: claim dirty events (SKIP LOCKED, safe across processes).
pub async fn materialize_pending(db: &Db, limit: i64) -> Result<Vec<(String, &'static str)>> {
    let pending = db
        .rows(
            "UPDATE calendar_occurrence_state SET dirty = false, updated_at = NOW() WHERE event_id IN (SELECT event_id FROM calendar_occurrence_state \
             WHERE dirty ORDER BY updated_at ASC LIMIT $1 FOR UPDATE SKIP LOCKED) RETURNING event_id",
            params![limit],
        )
        .await?;
    let horizon = occurrence_horizon(Utc::now());
    let mut out = Vec::new();
    for r in pending {
        let id = r.get::<_, uuid::Uuid>(0).to_string();
        match materialize_event(db, &id, horizon).await {
            Ok(res) => out.push((id, res)),
            Err(e) => {
                let _ = db
                    .exec(
                        "UPDATE calendar_occurrence_state SET dirty = true, updated_at = NOW() WHERE event_id = $1",
                        params![&id],
                    )
                    .await;
                tracing::warn!("Calendar occurrence materialisation failed: {id} {e}");
                out.push((id, "failed"));
            }
        }
    }
    Ok(out)
}

/// `startOccurrenceScheduler`.
pub fn start_occurrence_scheduler(db: Db) {
    let tick = Duration::from_millis(env_int("CALENDAR_OCCURRENCE_TICK_MS", 5000, 250, 600_000) as u64);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tick);
        loop {
            interval.tick().await;
            match materialize_pending(&db, 25).await {
                Ok(out) if !out.is_empty() => {
                    let notable: Vec<_> = out.iter().filter(|(_, r)| *r != "built").collect();
                    tracing::info!("Calendar occurrences materialised: {notable:?}");
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("Calendar occurrence scheduler failed: {e}"),
            }
        }
    });
}

/// `contactDateEvents`: yearly all-day events for contact birthdays/anniversaries/dates.
pub fn contact_date_events(
    contacts: &[Value],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Vec<(DateTime<Utc>, Map<String, Value>)> {
    let mut out = Vec::new();
    for c in contacts {
        let mut dates: Vec<Value> = c.get("contact_dates").and_then(Value::as_array).cloned().unwrap_or_default();
        for (label, key) in [("Birthday", "birthday"), ("Anniversary", "anniversary")] {
            if let Some(v) = c.get(key).filter(|v| inboxora_core::json::truthy(Some(v))) {
                dates.push(json!({ "label": label, "value": v, "fromColumn": true }));
            }
        }
        let mut seen: Vec<String> = Vec::new();
        for d in dates {
            let Some(obj) = d.as_object() else { continue };
            let label = obj
                .get("label")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .unwrap_or("Other")
                .to_owned();
            // Column dates are JS Dates in Node (toISOString().slice(0, 10)); JSON values are strings.
            let value = match obj.get("value") {
                Some(Value::String(s)) if obj.contains_key("fromColumn") => s.get(..10).unwrap_or("").to_owned(),
                Some(Value::String(s)) => s.trim().to_owned(),
                _ => String::new(),
            };
            static SHAPE: LazyLock<regex::Regex> =
                LazyLock::new(|| regex::Regex::new(r"^(?:\d{4}|-)-(\d{2})-(\d{2})$").expect("regex"));
            let Some(caps) = SHAPE.captures(&value) else { continue };
            let (month, day): (u32, u32) = (caps[1].parse().unwrap_or(0), caps[2].parse().unwrap_or(0));
            let key = format!("{}\u{0}{value}", label.to_lowercase());
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            let name = ["display_name", "primary_email"]
                .iter()
                .find_map(|k| c.get(*k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned));
            for year in from.year()..=to.year() {
                let Some(start) = Utc.with_ymd_and_hms(year, month, day, 0, 0, 0).single() else { continue };
                if start < from || start >= to {
                    continue;
                }
                let end = start + chrono::Duration::days(1);
                let label_key = &crypto::sha256_hex(&label)[..16];
                let cid = c.get("id").map(inboxora_mail::text::js_string).unwrap_or_else(|| "undefined".into());
                let id = format!("contacts-{cid}-{label_key}-{}-{year}", value.replace('-', ""));
                let mut m = Map::new();
                m.insert("id".into(), json!(id));
                m.insert("calendar_id".into(), json!("contacts-birthdays"));
                m.insert("uid".into(), json!(id));
                m.insert(
                    "summary".into(),
                    json!(format!("{label}: {}", name.clone().unwrap_or_else(|| "Contact".into()))),
                );
                m.insert("contact_date_label".into(), json!(label));
                m.insert("contact_name".into(), name.clone().map_or(Value::Null, Value::String));
                m.insert("starts_at".into(), json!(inboxora_core::json::js_iso(&start)));
                m.insert("ends_at".into(), json!(inboxora_core::json::js_iso(&end)));
                m.insert("all_day".into(), json!(true));
                m.insert("calendar_name".into(), json!("Contact dates"));
                m.insert("calendar_color".into(), json!("#e879f9"));
                m.insert("source".into(), json!("contacts"));
                m.insert("read_only".into(), json!(true));
                out.push((start, m));
            }
        }
    }
    out
}
