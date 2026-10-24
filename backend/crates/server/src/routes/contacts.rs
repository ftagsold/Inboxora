//! `/api/contacts` (port of routes/contacts.ts).

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use inboxora_core::pg::{Q, Val};
use inboxora_core::{Error, Result, crypto, params};
use inboxora_pim::{contacts as cf, vcard};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::http::query::parse_int;
use crate::http::{Auth, Body, Qs};
use crate::routes::util::{empty, err, fail, json, ok, send_image};
use crate::services::remote_dav;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/address-books", get(list_books).post(create_book))
        .route("/address-books/{id}", patch(update_book).delete(delete_book))
        .route("/address-books/{id}/export", get(export_book))
        .route("/address-books/{id}/import/google-csv", post(import_google_csv))
        .route("/", get(list).post(create))
        .route("/photo", get(photo))
        .route("/gravatar", get(gravatar))
        .route("/{id}", get(get_one).patch(update).delete(remove))
}

// ── query helpers (utils/query.ts) ───────────────────────────────────────────────────

pub fn query_string(q: &Qs, key: &str) -> Option<String> {
    match q.get(key)? {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => items.iter().find_map(|v| v.as_str().map(str::to_owned)),
        _ => None,
    }
}

pub fn query_int(q: &Qs, key: &str, fallback: i64) -> i64 {
    query_string(q, key).and_then(|s| parse_int(&s)).unwrap_or(fallback)
}

fn truthy(v: Option<&Value>) -> bool {
    inboxora_core::json::truthy(v)
}

// ── contact dates ────────────────────────────────────────────────────────────────────

/// `normalizeContactDate`: `Ok(None)` for null/empty, `Ok(Some)` for a valid YYYY-MM-DD,
/// `Err` (undefined in Node) otherwise.
fn normalize_contact_date(v: Option<&Value>) -> std::result::Result<Option<String>, ()> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        Some(Value::String(s)) => {
            let b = s.as_bytes();
            let shape = b.len() == 10
                && b[4] == b'-'
                && b[7] == b'-'
                && s.bytes().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
            if !shape {
                return Err(());
            }
            let y: i32 = s[..4].parse().map_err(|_| ())?;
            if (0..=99).contains(&y) {
                return Err(());
            }
            chrono::NaiveDate::from_ymd_opt(y, s[5..7].parse().map_err(|_| ())?, s[8..].parse().map_err(|_| ())?)
                .map(|_| Some(s.clone()))
                .ok_or(())
        }
        Some(_) => Err(()),
    }
}

fn date_key(label: &str, value: &str) -> String {
    format!("{}\\u0000{value}", label.to_lowercase())
}

/// `normalizeContactDates`.
fn normalize_contact_dates(v: &Value) -> Option<Vec<(String, String)>> {
    let arr = v.as_array()?;
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for e in arr {
        let obj = e.as_object()?;
        let label = obj.get("label")?.as_str()?;
        let label = vcard::normalize_contact_date_label(label)?;
        let value = obj.get("value").and_then(Value::as_str).and_then(|d| vcard::normalize_vcard_date(d, true))?;
        let k = date_key(&label, &value);
        if !seen.contains(&k) {
            seen.push(k);
            out.push((label, value));
        }
    }
    Some(out)
}

/// `contactDatesWithLegacy`. Legacy values only count when they are strings (a stored
/// date column arrives as a JS Date in Node and is skipped there).
fn contact_dates_with_legacy(
    dates: &[(String, String)],
    birthday: Option<&str>,
    anniversary: Option<&str>,
    authoritative: bool,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = dates
        .iter()
        .filter(|(label, _)| authoritative || !matches!(label.to_lowercase().as_str(), "birthday" | "anniversary"))
        .cloned()
        .collect();
    if authoritative {
        return out;
    }
    let seen: Vec<String> = out.iter().map(|(l, v)| date_key(l, v)).collect();
    for (label, value) in [("Birthday", birthday), ("Anniversary", anniversary)] {
        if let Some(v) = value.filter(|v| !v.is_empty())
            && !seen.contains(&date_key(label, v))
        {
            out.push((label.to_owned(), v.to_owned()));
        }
    }
    out
}

/// `legacyDatesFromContactDates`.
fn legacy_dates(dates: &[(String, String)]) -> (Option<String>, Option<String>) {
    let (mut b, mut a) = (None, None);
    for (label, value) in dates {
        if value.starts_with("--") {
            continue;
        }
        match label.to_lowercase().as_str() {
            "birthday" if b.is_none() => b = Some(value.clone()),
            "anniversary" if a.is_none() => a = Some(value.clone()),
            _ => {}
        }
    }
    (b, a)
}

fn dates_json(dates: &[(String, String)]) -> Value {
    Value::Array(dates.iter().map(|(l, v)| json!({ "label": l, "value": v })).collect())
}

// ── address books ────────────────────────────────────────────────────────────────────

async fn default_address_book(s: &AppState, user_id: &str) -> Result<String> {
    let row =
        s.db.opt(
            "INSERT INTO address_books (user_id, name) VALUES ($1, 'Personal') \
             ON CONFLICT (user_id, name) DO UPDATE SET updated_at = NOW() RETURNING id",
            params![user_id],
        )
        .await?
        .ok_or_else(|| Error::internal("no address book row"))?;
    Ok(row.get::<_, uuid::Uuid>(0).to_string())
}

async fn bump_sync_token(s: &AppState, id: &str) -> Result<()> {
    s.db.exec(
        "UPDATE address_books SET sync_token = gen_random_uuid()::text, updated_at = NOW() WHERE id = $1",
        params![id],
    )
    .await?;
    Ok(())
}

fn local_book_name(v: Option<&Value>) -> Option<String> {
    let name = v.and_then(Value::as_str).map(|s| inboxora_mail::text::collapse_ws(s.trim())).unwrap_or_default();
    let len = inboxora_mail::text::js_len(&name);
    (1..=120).contains(&len).then_some(name)
}

enum Local {
    Book(String),
    Denied(u16, &'static str),
}

async fn book_source(s: &AppState, user_id: &str, id: &str) -> Result<Option<String>> {
    if !inboxora_mail::util::is_uuid(id) {
        return Ok(None);
    }
    let row =
        s.db.json_opt("SELECT source FROM address_books WHERE id = $1 AND user_id = $2", params![id, user_id]).await?;
    Ok(row.and_then(|r| r.get("source").and_then(Value::as_str).map(str::to_owned)))
}

async fn require_local_book(s: &AppState, user_id: &str, id: &str) -> Result<Local> {
    let row =
        s.db.json_opt(
            "SELECT id, name, source, visible FROM address_books WHERE id = $1 AND user_id = $2",
            params![id, user_id],
        )
        .await?;
    let Some(book) = row else { return Ok(Local::Denied(404, "Address book not found")) };
    if book.get("source").and_then(Value::as_str) != Some("local") {
        return Ok(Local::Denied(403, "This address book is read-only"));
    }
    Ok(Local::Book(book.get("id").and_then(Value::as_str).unwrap_or("").to_owned()))
}

async fn list_books(State(s): State<AppState>, auth: Auth) -> Result<Response> {
    let rows = s
        .db
        .json_rows(
            "SELECT ab.id, ab.name, ab.source, ab.visible, COUNT(c.id)::int AS contact_count FROM address_books ab \
             LEFT JOIN contacts c ON c.address_book_id = ab.id WHERE ab.user_id = $1 GROUP BY ab.id ORDER BY ab.created_at ASC",
            params![&auth.user_id],
        )
        .await
        .map_err(fail("Address book list error", "Failed to fetch address books"))?;
    // CardDAV books are not stored; count their live contacts.
    let mut rows = rows;
    if rows.iter().any(|r| r.get("source").and_then(Value::as_str) == Some("carddav")) {
        let live = remote_dav::contact_rows(&s.db, &auth.user_id, None).await;
        for r in rows.iter_mut().filter(|r| r.get("source").and_then(Value::as_str) == Some("carddav")) {
            let id = r.get("id").cloned();
            let n = live.iter().filter(|c| c.get("address_book_id") == id.as_ref()).count();
            r["contact_count"] = json!(n);
        }
    }
    Ok(ok(json!({ "addressBooks": rows })))
}

async fn create_book(State(s): State<AppState>, auth: Auth, body: Body) -> Result<Response> {
    let Some(name) = local_book_name(body.get("name")) else {
        return Ok(err(400, "Address book name must be 1 to 120 characters"));
    };
    match s
        .db
        .json_opt(
            "INSERT INTO address_books (user_id, name, source, visible) VALUES ($1, $2, 'local', true) RETURNING id, name, source, visible",
            params![&auth.user_id, &name],
        )
        .await
    {
        Ok(row) => Ok(json(201, row.map_or(Value::Null, Value::Object))),
        Err(e) if e.is_unique_violation() => Ok(err(409, "An address book with that name already exists")),
        Err(e) => Err(fail("Address book create error", "Failed to create address book")(e)),
    }
}

async fn update_book(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, body: Body) -> Result<Response> {
    let raw_name = body.get("name");
    let visible = body.get("visible");
    if raw_name.is_some() && local_book_name(raw_name).is_none() {
        return Ok(err(400, "Address book name must be 1 to 120 characters"));
    }
    if visible.is_some_and(|v| !v.is_boolean()) {
        return Ok(err(400, "visible must be a boolean"));
    }
    if raw_name.is_none() && visible.is_none() {
        return Ok(err(400, "No address book changes supplied"));
    }
    let run = async {
        match require_local_book(&s, &auth.user_id, &id).await? {
            Local::Denied(code, msg) => return Ok(err(code, msg)),
            Local::Book(_) => {}
        }
        let row = s
            .db
            .json_opt(
                "UPDATE address_books SET name = COALESCE($1, name), visible = COALESCE($2, visible), updated_at = NOW() \
                 WHERE id = $3 AND user_id = $4 RETURNING id, name, source, visible",
                params![local_book_name(raw_name), visible.and_then(Value::as_bool), &id, &auth.user_id],
            )
            .await?;
        Ok::<_, Error>(ok(row.map_or(Value::Null, Value::Object)))
    };
    match run.await {
        Ok(r) => Ok(r),
        Err(e) if e.is_unique_violation() => Ok(err(409, "An address book with that name already exists")),
        Err(e) => Err(fail("Address book update error", "Failed to update address book")(e)),
    }
}

async fn delete_book(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> Result<Response> {
    let run = async {
        match require_local_book(&s, &auth.user_id, &id).await? {
            Local::Denied(code, msg) => return Ok(err(code, msg)),
            Local::Book(_) => {}
        }
        let count =
            s.db.opt(
                "SELECT COUNT(*)::int AS count FROM address_books WHERE user_id = $1 AND source = 'local'",
                params![&auth.user_id],
            )
            .await?
            .map_or(0, |r| r.get::<_, i32>(0));
        if count <= 1 {
            return Ok(err(409, "At least one local address book is required"));
        }
        s.db.exec("DELETE FROM address_books WHERE id = $1 AND user_id = $2", params![&id, &auth.user_id]).await?;
        Ok::<_, Error>(empty(204))
    };
    run.await.map_err(fail("Address book delete error", "Failed to delete address book"))
}

// ── list / photo / gravatar ──────────────────────────────────────────────────────────

async fn list(State(s): State<AppState>, auth: Auth, q: Qs) -> Result<Response> {
    let search = query_string(&q, "q").unwrap_or_default();
    let limit = query_int(&q, "limit", 50);
    let offset = query_int(&q, "offset", 0);
    let is_auto = query_string(&q, "is_auto");
    let book = query_string(&q, "addressBookId");
    let cap = limit.min(500);
    let off = offset.max(0);
    let mut conditions = vec!["c.user_id = $1".to_owned()];
    let mut p: Vec<Val> = vec![Val::from(&auth.user_id)];
    let trimmed = search.trim();
    if !trimmed.is_empty() {
        p.push(Val::from(format!("%{trimmed}%")));
        let n = p.len();
        conditions.push(format!(
            "(c.display_name ILIKE ${n} OR c.primary_email ILIKE ${n} OR c.organization ILIKE ${n} \
             OR (jsonb_typeof(c.emails) = 'array' AND EXISTS (SELECT 1 FROM jsonb_array_elements(c.emails) ae WHERE ae->>'value' ILIKE ${n})) \
             OR (jsonb_typeof(c.phones) = 'array' AND EXISTS (SELECT 1 FROM jsonb_array_elements(c.phones) ap WHERE ap->>'value' ILIKE ${n})))"
        ));
    }
    match is_auto.as_deref() {
        Some("true") => conditions.push("c.is_auto = true".into()),
        Some("false") => conditions.push("c.is_auto = false".into()),
        _ => {}
    }
    match book.filter(|b| !b.is_empty()) {
        Some(b) => {
            p.push(Val::from(b));
            conditions.push(format!("c.address_book_id = ${}", p.len()));
        }
        None => conditions.push("ab.visible = true".into()),
    }
    let where_ = conditions.join(" AND ");
    let (relation, live) = remote_dav::contacts_relation(&s.db, &auth.user_id, p.len() + 1).await;
    p.push(live);
    let n = p.len();
    let sql = format!(
        "SELECT c.id, c.uid, c.display_name, c.first_name, c.last_name, c.primary_email, c.emails, c.phones, c.organization, \
         c.notes, c.birthday, c.anniversary, c.contact_dates AS \"contactDates\", c.title, c.role, c.nickname, \
         c.urls, c.addresses, c.instant_messages AS \"instantMessages\", c.categories, \
         c.address_book_id, ab.name AS address_book_name, c.is_auto, c.send_count, c.last_sent, \
         c.etag, c.created_at, c.updated_at, (c.photo_data IS NOT NULL) AS has_contact_photo, false AS read_only \
         FROM {relation} c JOIN address_books ab ON ab.id = c.address_book_id WHERE {where_} \
         ORDER BY c.is_auto ASC, c.send_count DESC, lower(coalesce(c.display_name, c.primary_email, '')) ASC LIMIT ${} OFFSET ${}",
        n + 1,
        n + 2
    );
    let run = async {
        let mut list_params = p.clone();
        list_params.push(Val::Int(cap));
        list_params.push(Val::Int(off));
        let rows = s.db.json_rows(&sql, &list_params).await?;
        let total =
            s.db.opt(
                &format!(
                    "SELECT COUNT(*) FROM {relation} c JOIN address_books ab ON ab.id = c.address_book_id WHERE {where_}"
                ),
                &p,
            )
            .await?
            .map_or(0, |r| r.get::<_, i64>(0));
        Ok::<_, Error>(ok(json!({ "contacts": rows, "total": total })))
    };
    run.await.map_err(fail("Contacts list error", "Failed to fetch contacts"))
}

fn photo_response(photo: &str) -> Response {
    let cache = Some("private, max-age=86400");
    let decode = |s: &str| inboxora_mail::headers::base64_lenient(s);
    if let Some(rest) = photo.strip_prefix("data:") {
        let Some(comma) = photo.find(',') else { return empty(404) };
        let meta = &rest[..comma - 5];
        let mime = meta.split(';').next().filter(|m| !m.is_empty()).unwrap_or("image/jpeg");
        let mime = if meta.is_empty() { "image/jpeg" } else { mime };
        return with_cache(send_image(Some(mime), decode(&photo[comma + 1..]), None), cache);
    }
    with_cache(send_image(Some("image/jpeg"), decode(photo), None), cache)
}

fn with_cache(mut r: Response, cache: Option<&'static str>) -> Response {
    if let Some(c) = cache {
        r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static(c));
    }
    r
}

async fn photo(State(s): State<AppState>, auth: Auth, q: Qs) -> Response {
    let Some(email) = q.str("email").filter(|e| !e.is_empty()).map(str::to_owned) else { return empty(400) };
    let row = s
        .db
        .opt(
            "SELECT photo_data FROM contacts WHERE user_id = $1 AND primary_email = lower($2) AND photo_data IS NOT NULL LIMIT 1",
            params![&auth.user_id, email.trim()],
        )
        .await;
    match row {
        Ok(Some(r)) => photo_response(&r.get::<_, String>(0)),
        Ok(None) => match remote_dav::contact_photo(&s.db, &auth.user_id, &email).await {
            Some(p) => photo_response(&p),
            None => empty(404),
        },
        Err(e) => {
            tracing::error!("Contact photo error: {e}");
            empty(500)
        }
    }
}

struct GravatarEntry {
    hit: Option<(Vec<u8>, String)>,
    expires: Instant,
}

static GRAVATAR: LazyLock<Mutex<(HashMap<String, GravatarEntry>, std::collections::VecDeque<String>)>> =
    LazyLock::new(|| Mutex::new((HashMap::new(), std::collections::VecDeque::new())));
const GRAVATAR_TTL: Duration = Duration::from_secs(24 * 3600);
const GRAVATAR_MISS_TTL: Duration = Duration::from_secs(6 * 3600);
const GRAVATAR_MAX: usize = 2000;

fn gravatar_set(hash: &str, entry: GravatarEntry) {
    let mut g = GRAVATAR.lock();
    if !g.0.contains_key(hash) {
        if g.0.len() >= GRAVATAR_MAX
            && let Some(oldest) = g.1.pop_front()
        {
            g.0.remove(&oldest);
        }
        g.1.push_back(hash.to_owned());
    }
    g.0.insert(hash.to_owned(), entry);
}

fn plausible_email(e: &str) -> bool {
    // ^[^\s@]+@[^\s@]+\.[^\s@]+$
    let Some((local, domain)) = e.split_once('@') else { return false };
    let ok = |s: &str| !s.is_empty() && !s.chars().any(|c| c.is_whitespace() || c == '@');
    ok(local) && !domain.contains('@') && domain.rsplit_once('.').is_some_and(|(a, b)| ok(a) && ok(b))
}

async fn gravatar(_auth: Auth, q: Qs) -> Response {
    let email = q.str("email").map(|e| e.trim().to_lowercase()).unwrap_or_default();
    if email.is_empty() || email.len() > 254 || !plausible_email(&email) {
        return empty(400);
    }
    let hash = crypto::sha256_hex(&email);
    let cached = {
        let g = GRAVATAR.lock();
        g.0.get(&hash).filter(|e| e.expires > Instant::now()).map(|e| e.hit.clone())
    };
    let cache = Some("private, max-age=86400");
    if let Some(hit) = cached {
        return match hit {
            None => empty(404),
            Some((buf, t)) => with_cache(send_image(Some(&t), buf, None), cache),
        };
    }
    let url = format!("https://www.gravatar.com/avatar/{hash}?d=404&s=80&r=g");
    let Ok(req) = inboxora_net::safe_get(&url, inboxora_net::Guard::default()) else { return empty(502) };
    let resp = match req.header("User-Agent", "Inboxora/4.0.0").timeout(Duration::from_secs(6)).send().await {
        Ok(r) => r,
        Err(_) => return empty(502),
    };
    if resp.status().as_u16() == 404 {
        gravatar_set(&hash, GravatarEntry { hit: None, expires: Instant::now() + GRAVATAR_MISS_TTL });
        return empty(404);
    }
    let t = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
    if !resp.status().is_success() || inboxora_mail::util::safe_image_type(Some(&t)).is_none() {
        return empty(502);
    }
    let Ok(buf) = resp.bytes().await else { return empty(502) };
    if buf.is_empty() || buf.len() > 512 * 1024 {
        return empty(502);
    }
    gravatar_set(&hash, GravatarEntry { hit: Some((buf.to_vec(), t.clone())), expires: Instant::now() + GRAVATAR_TTL });
    with_cache(send_image(Some(&t), buf.to_vec(), None), cache)
}

// ── export / import ──────────────────────────────────────────────────────────────────

fn attachment(body: String, content_type: &'static str, filename: &str) -> Response {
    let mut res = body.into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    if let Ok(v) = HeaderValue::from_str(&inboxora_mail::util::express_attachment(filename)) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    res
}

async fn export_book(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, q: Qs) -> Result<Response> {
    let format = query_string(&q, "format").unwrap_or_default();
    if !matches!(format.as_str(), "google-csv" | "outlook-csv" | "vcard") {
        return Ok(err(400, "Unsupported export format"));
    }
    let run = async {
        let Some(book) = s
            .db
            .json_opt("SELECT id, name FROM address_books WHERE id = $1 AND user_id = $2", params![&id, &auth.user_id])
            .await?
        else {
            return Ok(err(404, "Address book not found"));
        };
        let book_id = book["id"].as_str().unwrap_or("").to_owned();
        let (relation, live) = remote_dav::contacts_relation(&s.db, &auth.user_id, 2).await;
        let rows = s
            .db
            .json_rows(
                &format!(
                    "SELECT uid, display_name, first_name, last_name, emails, phones, organization, title, notes FROM {relation} c \
                     WHERE address_book_id = $1 ORDER BY lower(coalesce(display_name, primary_email, ''))"
                ),
                &[Val::from(&book_id), live],
            )
            .await?;
        static NON_FILE: LazyLock<regex::Regex> =
            LazyLock::new(|| regex::Regex::new(r"(?i)[^a-z0-9_-]+").expect("regex"));
        let name = NON_FILE.replace_all(book["name"].as_str().unwrap_or(""), "-").into_owned();
        let filename = if name.is_empty() { "contacts".to_owned() } else { name };
        Ok::<_, Error>(match format.as_str() {
            "vcard" => {
                attachment(cf::contacts_to_vcard(&rows), "text/x-vcard; charset=utf-8", &format!("{filename}.vcf"))
            }
            "google-csv" => attachment(
                cf::contacts_to_google_csv(&rows),
                "text/csv; charset=utf-8",
                &format!("{filename}-{format}.csv"),
            ),
            _ => attachment(
                cf::contacts_to_outlook_csv(&rows),
                "text/csv; charset=utf-8",
                &format!("{filename}-{format}.csv"),
            ),
        })
    };
    run.await.map_err(fail("Address book export error", "Failed to export address book"))
}

fn opt_str(v: &Value, k: &str) -> Val {
    match v.get(k) {
        Some(x) if truthy(Some(x)) => Val::from(x.clone()),
        _ => Val::Null,
    }
}

fn stringify(v: Option<&Value>, default: Value) -> String {
    inboxora_core::json::stringify(v.filter(|x| truthy(Some(x))).unwrap_or(&default))
}

async fn import_google_csv(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
    body: Body,
) -> Result<Response> {
    let csv = body.str("csv").unwrap_or("").to_owned();
    if csv.is_empty() || inboxora_mail::text::js_len(&csv) > 900_000 {
        return Ok(err(400, "Google CSV must be a non-empty file smaller than 900 KB"));
    }
    let run = async {
        let book_id = match require_local_book(&s, &auth.user_id, &id).await? {
            Local::Denied(code, msg) => return Ok(err(code, msg)),
            Local::Book(b) => b,
        };
        let contacts = cf::parse_google_csv(&csv);
        if contacts.is_empty() {
            return Ok(err(400, "No contacts found in Google CSV"));
        }
        let user_id = auth.user_id.clone();
        let book = book_id.clone();
        s.db.transaction(false, 0, move |tx| {
            let contacts = contacts.clone();
            let (user_id, book) = (user_id.clone(), book.clone());
            Box::pin(async move {
                for c in &contacts {
                    let uid = crypto::random_uuid();
                    let mut with_uid = Map::new();
                    with_uid.insert("uid".into(), Value::String(uid.clone()));
                    if let Some(o) = c.as_object() {
                        for (k, v) in o {
                            with_uid.insert(k.clone(), v.clone());
                        }
                    }
                    let card = vcard::generate(&Value::Object(with_uid));
                    let etag = crypto::md5_hex(&card);
                    let primary = c.get("emails").and_then(|e| e.get(0)).and_then(|e| e.get("value")).filter(|v| truthy(Some(v))).cloned();
                    tx.exec(
                        "INSERT INTO contacts (address_book_id, user_id, uid, vcard, etag, display_name, first_name, last_name, primary_email, emails, phones, organization, notes, birthday, anniversary, contact_dates, title, role, nickname, urls, instant_messages, categories, addresses, google_fields, is_auto) \
                         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16::jsonb,$17,$18,$19,$20::jsonb,$21::jsonb,$22::jsonb,$23::jsonb,$24::jsonb,false) \
                         ON CONFLICT (address_book_id, primary_email) WHERE primary_email IS NOT NULL DO UPDATE SET vcard = EXCLUDED.vcard, etag = EXCLUDED.etag, display_name = EXCLUDED.display_name, first_name = EXCLUDED.first_name, last_name = EXCLUDED.last_name, emails = EXCLUDED.emails, phones = EXCLUDED.phones, organization = EXCLUDED.organization, notes = EXCLUDED.notes, birthday = EXCLUDED.birthday, anniversary = EXCLUDED.anniversary, contact_dates = EXCLUDED.contact_dates, title = EXCLUDED.title, role = EXCLUDED.role, nickname = EXCLUDED.nickname, urls = EXCLUDED.urls, instant_messages = EXCLUDED.instant_messages, categories = EXCLUDED.categories, addresses = EXCLUDED.addresses, google_fields = EXCLUDED.google_fields, is_auto = false, updated_at = NOW()",
                        &[
                            Val::from(&book),
                            Val::from(&user_id),
                            Val::from(&uid),
                            Val::from(&card),
                            Val::from(&etag),
                            opt_str(c, "displayName"),
                            opt_str(c, "firstName"),
                            opt_str(c, "lastName"),
                            primary.map_or(Val::Null, Val::from),
                            Val::from(stringify(c.get("emails"), json!([]))),
                            Val::from(stringify(c.get("phones"), json!([]))),
                            opt_str(c, "organization"),
                            opt_str(c, "notes"),
                            opt_str(c, "birthday"),
                            opt_str(c, "anniversary"),
                            Val::from(stringify(c.get("contactDates"), json!([]))),
                            opt_str(c, "title"),
                            opt_str(c, "role"),
                            opt_str(c, "nickname"),
                            Val::from(stringify(c.get("urls"), json!([]))),
                            Val::from(stringify(c.get("instantMessages"), json!([]))),
                            Val::from(stringify(c.get("categories"), json!([]))),
                            Val::from(stringify(c.get("addresses"), json!([]))),
                            Val::from(stringify(c.get("sourceFields"), json!({}))),
                        ],
                    )
                    .await?;
                }
                Ok(())
            })
        })
        .await?;
        bump_sync_token(&s, &book_id).await?;
        Ok::<_, Error>(json(201, json!({ "imported": cf::parse_google_csv(&csv).len() })))
    };
    run.await.map_err(fail("Google CSV import error", "Failed to import Google CSV"))
}

// ── single contact ───────────────────────────────────────────────────────────────────

fn empty_or_null(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => true,
        Some(Value::Array(a)) => a.is_empty(),
        _ => false,
    }
}

/// A contact of a live CardDAV book in `get_one`'s shape (with its photo and vCard).
async fn live_contact_detail(s: &AppState, user: &str, id: &str) -> Option<Map<String, Value>> {
    let row = remote_dav::contact_rows(&s.db, user, None)
        .await
        .into_iter()
        .find(|m| m.get("id").and_then(Value::as_str) == Some(id))?;
    let g = |k: &str| row.get(k).cloned().unwrap_or(Value::Null);
    let photo = match row.get("primary_email").and_then(Value::as_str) {
        Some(email) if !g("photo_data").is_null() => remote_dav::contact_photo(&s.db, user, email).await,
        _ => None,
    };
    let mut m = Map::new();
    for k in [
        "id",
        "uid",
        "display_name",
        "first_name",
        "last_name",
        "primary_email",
        "emails",
        "phones",
        "organization",
        "notes",
        "birthday",
        "anniversary",
        "title",
        "role",
        "nickname",
        "urls",
        "addresses",
        "categories",
        "is_auto",
        "send_count",
        "etag",
        "vcard",
    ] {
        m.insert(k.into(), g(k));
    }
    m.insert("contactDates".into(), g("contact_dates"));
    m.insert("instantMessages".into(), g("instant_messages"));
    m.insert("googleFields".into(), Value::Null);
    m.insert("photo_data".into(), json!(photo));
    m.insert("last_sent".into(), Value::Null);
    m.insert("created_at".into(), Value::Null);
    m.insert("updated_at".into(), Value::Null);
    m.insert("address_book_id".into(), g("address_book_id"));
    m.insert("address_book_source".into(), json!("carddav"));
    m.insert("read_only".into(), json!(false));
    Some(m)
}

async fn get_one(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> Result<Response> {
    let run = async {
        let row = s
            .db
            .json_opt(
                "SELECT c.id, c.uid, c.display_name, c.first_name, c.last_name, c.primary_email, c.emails, c.phones, c.organization, \
                 c.notes, c.birthday, c.anniversary, c.contact_dates AS \"contactDates\", c.title, c.role, c.nickname, \
                 c.urls, c.addresses, c.instant_messages AS \"instantMessages\", c.categories, c.google_fields AS \"googleFields\", \
                 c.photo_data, c.is_auto, c.send_count, c.last_sent, c.etag, c.vcard, c.created_at, c.updated_at, \
                 false AS read_only FROM contacts c JOIN address_books ab ON ab.id = c.address_book_id \
                 WHERE c.id = $1 AND c.user_id = $2",
                params![&id, &auth.user_id],
            )
            .await?;
        let row = match row {
            Some(r) => Some(r),
            None => live_contact_detail(&s, &auth.user_id, &id).await,
        };
        let Some(mut contact) = row else { return Ok(err(404, "Contact not found")) };
        if let Some(card) = contact.get("vcard").and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_owned) {
            let parsed = serde_json::to_value(vcard::parse(&card)).unwrap_or(Value::Null);
            for f in ["title", "role", "nickname", "urls", "addresses", "instantMessages"] {
                if empty_or_null(contact.get(f)) {
                    contact.insert(f.into(), parsed.get(f).cloned().unwrap_or(Value::Null));
                }
            }
            let cats_empty = match contact.get("categories") {
                None | Some(Value::Null) => true,
                Some(Value::Array(a)) => a.is_empty(),
                Some(Value::String(s)) => s.is_empty(),
                _ => false,
            };
            if cats_empty {
                contact.insert("categories".into(), parsed.get("categories").cloned().unwrap_or(Value::Null));
            }
        }
        Ok::<_, Error>(ok(Value::Object(contact)))
    };
    run.await.map_err(fail("Contact get error", "Failed to fetch contact"))
}

const RETURNING: &str = "RETURNING id, uid, display_name, first_name, last_name, primary_email, emails, phones, organization, notes, \
    birthday, anniversary, contact_dates AS \"contactDates\", title, role, nickname, urls, addresses, instant_messages AS \"instantMessages\", \
    categories, is_auto, send_count, last_sent, etag, created_at, updated_at";

fn primary_email_of(emails: &Value) -> std::result::Result<Option<String>, Error> {
    match emails.get(0).and_then(|e| e.get("value")) {
        Some(v) if truthy(Some(v)) => match v.as_str() {
            Some(s) => Ok(Some(s.to_lowercase().trim().to_owned())),
            // `emails[0].value.toLowerCase` on a non-string throws outside the try block.
            None => Err(Error::internal("emails[0].value.toLowerCase is not a function")),
        },
        _ => Ok(None),
    }
}

fn js_or_null(v: Option<&Value>) -> Val {
    match v {
        Some(x) if truthy(Some(x)) => Val::from(x.clone()),
        _ => Val::Null,
    }
}

async fn create(State(s): State<AppState>, auth: Auth, body: Body) -> Result<Response> {
    let b = &body.0;
    let get = |k: &str| b.get(k);
    let emails = get("emails").cloned().unwrap_or_else(|| json!([]));
    let phones = get("phones").cloned().unwrap_or_else(|| json!([]));
    if !emails.is_array() {
        return Ok(err(400, "emails must be an array"));
    }
    if !phones.is_array() {
        return Ok(err(400, "phones must be an array"));
    }
    let mut rich_in = Map::new();
    for k in ["title", "role", "nickname"] {
        if let Some(v) = get(k) {
            rich_in.insert(k.into(), v.clone());
        }
    }
    for k in ["urls", "instantMessages", "categories", "addresses"] {
        rich_in.insert(k.into(), get(k).cloned().unwrap_or_else(|| json!([])));
    }
    let Some(rich) = cf::normalize_rich_fields(&Value::Object(rich_in)) else {
        return Ok(err(400, "Rich contact fields are malformed"));
    };
    let (Ok(nb), Ok(na)) = (normalize_contact_date(get("birthday")), normalize_contact_date(get("anniversary"))) else {
        return Ok(err(400, "Contact dates must use YYYY-MM-DD"));
    };
    let dates_given = get("contactDates").is_some();
    let dates_in = match get("contactDates") {
        None | Some(Value::Null) => json!([]),
        Some(v) => v.clone(),
    };
    let Some(normalized) = normalize_contact_dates(&dates_in) else {
        return Ok(err(400, "contactDates must be an array of safe labelled YYYY-MM-DD or --MM-DD dates"));
    };
    let stored_dates = contact_dates_with_legacy(&normalized, nb.as_deref(), na.as_deref(), dates_given);
    let (stored_birthday, stored_anniversary) =
        if dates_given { legacy_dates(&stored_dates) } else { (nb.clone(), na.clone()) };
    let primary = primary_email_of(&emails)?;
    if !truthy(get("displayName")) && primary.is_none() {
        return Ok(err(400, "A name or email address is required"));
    }
    let requested = get("addressBookId").filter(|v| truthy(Some(v))).map(inboxora_mail::text::js_string);
    let run = async {
        let book_id = match &requested {
            Some(r) => r.clone(),
            None => default_address_book(&s, &auth.user_id).await?,
        };
        // A CardDAV address book takes the new contact on its server instead of a local row.
        let carddav = match &requested {
            Some(r) => match book_source(&s, &auth.user_id, r).await?.as_deref() {
                Some("carddav") => true,
                _ => match require_local_book(&s, &auth.user_id, r).await? {
                    Local::Denied(code, msg) => return Ok(err(code, msg)),
                    Local::Book(_) => false,
                },
            },
            None => false,
        };
        let uid = crypto::random_uuid();
        let mut card_in = Map::new();
        card_in.insert("uid".into(), Value::String(uid.clone()));
        for k in ["displayName", "firstName", "lastName"] {
            if let Some(v) = get(k) {
                card_in.insert(k.into(), v.clone());
            }
        }
        card_in.insert("emails".into(), emails.clone());
        card_in.insert("phones".into(), phones.clone());
        for k in ["organization", "notes"] {
            if let Some(v) = get(k) {
                card_in.insert(k.into(), v.clone());
            }
        }
        card_in.insert("birthday".into(), stored_birthday.clone().map_or(Value::Null, Value::String));
        card_in.insert("anniversary".into(), stored_anniversary.clone().map_or(Value::Null, Value::String));
        card_in.insert("contactDates".into(), dates_json(&stored_dates));
        for (k, v) in &rich {
            card_in.insert(k.clone(), v.clone());
        }
        let card = vcard::generate(&Value::Object(card_in));
        if carddav {
            return Ok(match remote_dav::write_card(&s.db, &auth.user_id, &book_id, &uid, &card, None).await {
                Ok(id) => {
                    json(201, live_contact_detail(&s, &auth.user_id, &id).await.map_or(Value::Null, Value::Object))
                }
                Err(e) => err(502, &e),
            });
        }
        let etag = crypto::md5_hex(&card);
        let sql = format!(
            "INSERT INTO contacts (address_book_id, user_id, uid, vcard, etag, display_name, first_name, last_name, primary_email, \
             emails, phones, organization, notes, birthday, anniversary, contact_dates, title, role, nickname, urls, instant_messages, \
             categories, addresses, is_auto) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16::jsonb,$17,$18,$19,$20,$21,$22,$23, false) {RETURNING}"
        );
        let r = |k: &str| rich.get(k).cloned().unwrap_or(Value::Null);
        let row =
            s.db.json_opt(
                &sql,
                &[
                    Val::from(&book_id),
                    Val::from(&auth.user_id),
                    Val::from(&uid),
                    Val::from(&card),
                    Val::from(&etag),
                    js_or_null(get("displayName")),
                    js_or_null(get("firstName")),
                    js_or_null(get("lastName")),
                    Val::from(primary.clone()),
                    Val::from(inboxora_core::json::stringify(&emails)),
                    Val::from(inboxora_core::json::stringify(&phones)),
                    js_or_null(get("organization")),
                    js_or_null(get("notes")),
                    Val::from(stored_birthday.clone()),
                    Val::from(stored_anniversary.clone()),
                    Val::from(inboxora_core::json::stringify(&dates_json(&stored_dates))),
                    Val::from(r("title")),
                    Val::from(r("role")),
                    Val::from(r("nickname")),
                    Val::from(inboxora_core::json::stringify(&r("urls"))),
                    Val::from(inboxora_core::json::stringify(&r("instantMessages"))),
                    Val::from(inboxora_core::json::stringify(&r("categories"))),
                    Val::from(inboxora_core::json::stringify(&r("addresses"))),
                ],
            )
            .await?;
        bump_sync_token(&s, &book_id).await?;
        Ok::<_, Error>(json(201, row.map_or(Value::Null, Value::Object)))
    };
    match run.await {
        Ok(r) => Ok(r),
        Err(e) if e.is_unique_violation() => Ok(err(409, "A contact with that email already exists")),
        Err(e) => Err(fail("Contact create error", "Failed to create contact")(e)),
    }
}

async fn update(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, body: Body) -> Result<Response> {
    let b = &body.0;
    let get = |k: &str| b.get(k);
    if get("emails").is_some_and(|v| !v.is_array()) {
        return Ok(err(400, "emails must be an array"));
    }
    if get("phones").is_some_and(|v| !v.is_array()) {
        return Ok(err(400, "phones must be an array"));
    }
    let (Ok(nb), Ok(na)) = (normalize_contact_date(get("birthday")), normalize_contact_date(get("anniversary"))) else {
        return Ok(err(400, "Contact dates must use YYYY-MM-DD"));
    };
    if let Some(d) = get("contactDates")
        && normalize_contact_dates(d).is_none()
    {
        return Ok(err(400, "contactDates must be an array of safe labelled YYYY-MM-DD or --MM-DD dates"));
    }
    let run = async {
        let cur = s
            .db
            .json_opt(
                "SELECT c.*, ab.source AS book_source FROM contacts c JOIN address_books ab ON ab.id = c.address_book_id \
                 WHERE c.id = $1 AND c.user_id = $2",
                params![&id, &auth.user_id],
            )
            .await?;
        // A contact of a live CardDAV book has the same fields; its vCard is written back below.
        let (c, live) = match cur {
            Some(c) => (c, false),
            None => match remote_dav::live_contact(&s.db, &auth.user_id, &id).await {
                Some(c) => (c, true),
                None => return Ok(err(404, "Contact not found")),
            },
        };
        if !live && c.get("book_source").and_then(Value::as_str) == Some("carddav") {
            return Ok(err(403, "This contact is synced from CardDAV and is read-only"));
        }
        let rich_keys = [
            ("title", "title"),
            ("role", "role"),
            ("nickname", "nickname"),
            ("urls", "urls"),
            ("instantMessages", "instant_messages"),
            ("categories", "categories"),
            ("addresses", "addresses"),
        ];
        let has_rich = rich_keys.iter().any(|(k, _)| get(k).is_some());
        let rich: Map<String, Value> = if has_rich {
            let mut input = Map::new();
            for (k, col) in rich_keys {
                // chooseDefined: the request value (null included) wins over the stored one.
                let v = get(k).cloned().or_else(|| c.get(col).cloned());
                if let Some(v) = v {
                    input.insert(k.into(), v);
                }
            }
            match cf::normalize_rich_fields(&Value::Object(input)) {
                Some(r) => r,
                None => return Ok(err(400, "Rich contact fields are malformed")),
            }
        } else {
            rich_keys.iter().map(|(k, col)| ((*k).to_owned(), c.get(*col).cloned().unwrap_or(Value::Null))).collect()
        };
        let pick = |k: &str, col: &str| get(k).cloned().unwrap_or_else(|| c.get(col).cloned().unwrap_or(Value::Null));
        let new_emails = pick("emails", "emails");
        let new_phones = pick("phones", "phones");
        let new_display = pick("displayName", "display_name");
        let new_first = pick("firstName", "first_name");
        let new_last = pick("lastName", "last_name");
        let new_org = pick("organization", "organization");
        let new_notes = pick("notes", "notes");
        // A stored date column is a JS Date in Node: stored back as-is, never a legacy string.
        enum D {
            Str(Option<String>),
            Stored(Value),
        }
        let new_birthday = if get("birthday").is_some() {
            D::Str(nb.clone())
        } else {
            D::Stored(c.get("birthday").cloned().unwrap_or(Value::Null))
        };
        let new_anniversary = if get("anniversary").is_some() {
            D::Str(na.clone())
        } else {
            D::Stored(c.get("anniversary").cloned().unwrap_or(Value::Null))
        };
        let dates_source = match get("contactDates") {
            Some(d) => d.clone(),
            None => match c.get("contact_dates") {
                Some(v) if truthy(Some(v)) => v.clone(),
                _ => json!([]),
            },
        };
        let Some(normalized) = normalize_contact_dates(&dates_source) else {
            return Ok(err(400, "Stored contactDates contain unsafe labels"));
        };
        let as_str = |d: &D| match d {
            D::Str(s) => s.clone(),
            D::Stored(_) => None,
        };
        let dates_given = get("contactDates").is_some();
        let new_dates = contact_dates_with_legacy(
            &normalized,
            as_str(&new_birthday).as_deref(),
            as_str(&new_anniversary).as_deref(),
            dates_given,
        );
        let to_val = |d: &D| match d {
            D::Str(s) => Val::from(s.clone()),
            D::Stored(Value::String(iso)) => Val::Str(iso.clone()),
            D::Stored(_) => Val::Null,
        };
        let (stored_birthday, stored_anniversary) = if dates_given {
            let (b2, a2) = legacy_dates(&new_dates);
            (Val::from(b2), Val::from(a2))
        } else {
            (to_val(&new_birthday), to_val(&new_anniversary))
        };
        let new_primary = if get("emails").is_none() {
            c.get("primary_email").cloned().map_or(Val::Null, Val::from)
        } else {
            Val::from(primary_email_of(&new_emails)?)
        };
        let mut card_in = Map::new();
        card_in.insert("uid".into(), c.get("uid").cloned().unwrap_or(Value::Null));
        card_in.insert("displayName".into(), new_display.clone());
        card_in.insert("firstName".into(), new_first.clone());
        card_in.insert("lastName".into(), new_last.clone());
        card_in.insert("emails".into(), new_emails.clone());
        card_in.insert("phones".into(), new_phones.clone());
        card_in.insert("organization".into(), new_org.clone());
        card_in.insert("notes".into(), new_notes.clone());
        let legacy_json = |v: &Val| match v {
            Val::Str(s) => Value::String(s.clone()),
            _ => Value::Null,
        };
        card_in.insert("birthday".into(), legacy_json(&stored_birthday));
        card_in.insert("anniversary".into(), legacy_json(&stored_anniversary));
        card_in.insert("contactDates".into(), dates_json(&new_dates));
        for (k, v) in &rich {
            card_in.insert(k.clone(), v.clone());
        }
        let card_value = Value::Object(card_in);
        let card = match c.get("vcard").and_then(Value::as_str).filter(|v| !v.is_empty()) {
            Some(existing) => vcard::merge(existing, &card_value),
            None => vcard::generate(&card_value),
        };
        if live {
            let field = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
            let existing = (field("dav_href"), field("dav_etag"));
            let written = remote_dav::write_card(
                &s.db,
                &auth.user_id,
                &field("address_book_id"),
                &field("uid"),
                &card,
                Some((&existing.0, &existing.1)),
            )
            .await;
            return Ok(match written {
                Ok(id) => ok(live_contact_detail(&s, &auth.user_id, &id).await.map_or(Value::Null, Value::Object)),
                Err(e) => err(502, &e),
            });
        }
        let etag = crypto::md5_hex(&card);
        let r = |k: &str| rich.get(k).cloned().unwrap_or(Value::Null);
        let sql = format!(
            "UPDATE contacts SET display_name = $1, first_name = $2, last_name = $3, primary_email = $4, emails = $5, phones = $6, \
             organization = $7, notes = $8, birthday = $9, anniversary = $10, contact_dates = $11::jsonb, title = $12, role = $13, \
             nickname = $14, urls = $15, instant_messages = $16, categories = $17, addresses = $18, vcard = $19, etag = $20, \
             updated_at = NOW(), is_auto = false WHERE id = $21 AND user_id = $22 {RETURNING}"
        );
        let row =
            s.db.json_opt(
                &sql,
                &[
                    js_or_null(Some(&new_display)),
                    js_or_null(Some(&new_first)),
                    js_or_null(Some(&new_last)),
                    new_primary,
                    Val::from(inboxora_core::json::stringify(&new_emails)),
                    Val::from(inboxora_core::json::stringify(&new_phones)),
                    js_or_null(Some(&new_org)),
                    js_or_null(Some(&new_notes)),
                    stored_birthday,
                    stored_anniversary,
                    Val::from(inboxora_core::json::stringify(&dates_json(&new_dates))),
                    Val::from(r("title")),
                    Val::from(r("role")),
                    Val::from(r("nickname")),
                    Val::from(inboxora_core::json::stringify(&r("urls"))),
                    Val::from(inboxora_core::json::stringify(&r("instantMessages"))),
                    Val::from(inboxora_core::json::stringify(&r("categories"))),
                    Val::from(inboxora_core::json::stringify(&r("addresses"))),
                    Val::from(&card),
                    Val::from(&etag),
                    Val::from(&id),
                    Val::from(&auth.user_id),
                ],
            )
            .await?;
        let book = c.get("address_book_id").and_then(Value::as_str).unwrap_or("").to_owned();
        bump_sync_token(&s, &book).await?;
        Ok::<_, Error>(ok(row.map_or(Value::Null, Value::Object)))
    };
    match run.await {
        Ok(r) => Ok(r),
        Err(e) if e.is_unique_violation() => Ok(err(409, "A contact with that email already exists")),
        Err(e) => Err(fail("Contact update error", "Failed to update contact")(e)),
    }
}

async fn remove(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> Result<Response> {
    let run = async {
        let owner = s
            .db
            .json_opt(
                "SELECT ab.source FROM contacts c JOIN address_books ab ON ab.id = c.address_book_id WHERE c.id = $1 AND c.user_id = $2",
                params![&id, &auth.user_id],
            )
            .await?;
        let Some(owner) = owner else {
            let Some(c) = remote_dav::live_contact(&s.db, &auth.user_id, &id).await else {
                return Ok(err(404, "Contact not found"));
            };
            let field = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
            return Ok(
                match remote_dav::delete_card(
                    &s.db,
                    &auth.user_id,
                    &field("address_book_id"),
                    &field("dav_href"),
                    &field("dav_etag"),
                )
                .await
                {
                    Ok(()) => ok(json!({ "ok": true })),
                    Err(e) => err(502, &e),
                },
            );
        };
        if owner.get("source").and_then(Value::as_str) == Some("carddav") {
            return Ok(err(403, "This contact is synced from CardDAV and is read-only"));
        }
        let deleted =
            s.db.opt(
                "DELETE FROM contacts WHERE id = $1 AND user_id = $2 RETURNING address_book_id",
                params![&id, &auth.user_id],
            )
            .await?;
        let Some(d) = deleted else { return Ok(err(404, "Contact not found")) };
        bump_sync_token(&s, &d.get::<_, uuid::Uuid>(0).to_string()).await?;
        Ok::<_, Error>(ok(json!({ "ok": true })))
    };
    run.await.map_err(fail("Contact delete error", "Failed to delete contact"))
}
