//! `/api/dav/servers`: CalDAV/CardDAV accounts (port of `routes/davServers.ts`).

use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post};
use inboxora_core::pg::{Q, Row, Val};
use inboxora_core::{Error, Result, crypto, params};
use serde_json::{Map, Value, json};

use super::util::{empty, err, json, ok};
use crate::http::{Auth, Body};
use crate::services::dav_client::{self, Creds};
use crate::services::dav_sync::{self, ADDRESS_BOOKS, CALENDARS, Source};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/discover", post(discover))
        .route("/{id}/sync", post(sync))
        .route("/{id}", axum::routing::patch(update).delete(remove))
}

fn ts(r: &Row, c: &str) -> Value {
    r.try_get::<_, Option<chrono::DateTime<chrono::Utc>>>(c)
        .ok()
        .flatten()
        .map_or(Value::Null, |t| Value::String(inboxora_core::json::js_iso(&t)))
}

fn text(r: &Row, c: &str) -> Option<String> {
    r.try_get::<_, Option<String>>(c).ok().flatten()
}

/// `publicServer(source)`: never exposes the URL or credentials; errors are URL-redacted.
fn public_server(r: &Row, calendar_error: Option<Option<String>>, book_error: Option<Option<String>>) -> Value {
    let url = text(r, "url");
    let decrypted = url.as_deref().filter(|u| !u.is_empty()).and_then(crypto::decrypt);
    let secrets: Vec<String> = match (&url, &decrypted) {
        (Some(u), None) if !u.is_empty() => vec![u.clone()],
        (Some(u), Some(d)) => [u.clone(), d.clone()].into_iter().filter(|s| !s.is_empty()).collect(),
        _ => Vec::new(),
    };
    let redact = |m: Option<String>| -> Value {
        m.map_or(Value::Null, |mut m| {
            for s in &secrets {
                m = m.replace(s.as_str(), "[redacted]");
            }
            Value::String(m)
        })
    };
    // Keys whose stored value is missing are omitted, as JSON.stringify drops `undefined`.
    let pick = |c: &Value, keys: &[(&str, &str)]| -> Value {
        let mut m = Map::new();
        for (out, from) in keys {
            if let Some(v) = c.get(*from) {
                m.insert((*out).to_owned(), v.clone());
            }
        }
        Value::Object(m)
    };
    let calendars = dav_sync::parse_collections(text(r, "collections").as_deref()).map(|list| {
        list.iter()
            .map(|c| {
                pick(
                    c,
                    &[("id", "hrefHash"), ("displayName", "displayName"), ("color", "color"), ("selected", "selected")],
                )
            })
            .collect::<Vec<_>>()
    });
    let books = dav_sync::parse_collections(text(r, "address_book_collections").as_deref()).map(|list| {
        list.iter()
            .map(|c| pick(c, &[("id", "hrefHash"), ("displayName", "displayName"), ("selected", "selected")]))
            .collect::<Vec<_>>()
    });
    let id = r.get::<_, uuid::Uuid>("id").to_string();
    let mut out = Map::new();
    out.insert("id".into(), Value::String(id));
    out.insert("displayName".into(), text(r, "display_name").map_or(Value::Null, Value::String));
    out.insert("username".into(), text(r, "username").map_or(Value::Null, Value::String));
    out.insert(
        "intervalSeconds".into(),
        r.try_get::<_, Option<i32>>("interval_seconds").ok().flatten().map_or(Value::Null, Value::from),
    );
    out.insert(
        "writable".into(),
        Value::Bool(r.try_get::<_, Option<bool>>("writable").ok().flatten().unwrap_or(false)),
    );
    out.insert(
        "enabled".into(),
        r.try_get::<_, Option<bool>>("enabled").ok().flatten().map_or(Value::Null, Value::Bool),
    );
    out.insert(
        "calendarSync".into(),
        json!({ "at": ts(r, "last_sync_at"), "error": redact(calendar_error.unwrap_or_else(|| text(r, "last_error"))) }),
    );
    out.insert(
        "addressBookSync".into(),
        json!({ "at": ts(r, "address_book_last_sync_at"), "error": redact(book_error.unwrap_or_else(|| text(r, "address_book_last_error"))) }),
    );
    out.insert("calendars".into(), calendars.map_or(Value::Null, Value::Array));
    out.insert("addressBooks".into(), books.map_or(Value::Null, Value::Array));
    Value::Object(out)
}

async fn list(State(s): State<AppState>, auth: Auth) -> Result<Response> {
    let rows = s
        .db
        .rows(
            "SELECT id, kind, url, username, display_name, interval_seconds, writable, collections, address_book_collections, enabled, \
             last_sync_at, last_error, address_book_last_sync_at, address_book_last_error \
             FROM calendar_import_sources WHERE user_id = $1 AND kind = 'caldav' ORDER BY created_at ASC",
            params![&auth.user_id],
        )
        .await?;
    let servers: Vec<Value> = rows.iter().map(|r| public_server(r, None, None)).collect();
    Ok(ok(json!({ "servers": servers })))
}

fn truthy(v: Option<&Value>) -> bool {
    inboxora_core::json::truthy(v)
}

/// `String(value)` for request fields used as text.
fn as_text(v: Option<&Value>) -> Option<String> {
    v.filter(|v| truthy(Some(v))).map(crate::services::dav_xml::js_string)
}

async fn discover(State(s): State<AppState>, _auth: Auth, body: Body) -> Result<Response> {
    let (username, password) = (body.get("username"), body.get("password"));
    if !truthy(username) || !truthy(password) {
        return Ok(err(400, "username and password are required"));
    }
    let raw_url = as_text(body.get("url"));
    let resolved = match dav_client::resolve_caldav_url(raw_url.as_deref(), username.and_then(Value::as_str)) {
        Ok(u) => u,
        Err(e) => return Ok(err(400, &e)),
    };
    let (url, policy) = match dav_client::validate_source_url(&s.db, &resolved).await? {
        Ok(v) => v,
        Err(e) => return Ok(err(400, &e)),
    };
    let server = url.to_string();
    let creds = Creds {
        username: crate::services::dav_xml::js_string(username.unwrap_or(&Value::Null)),
        password: crate::services::dav_xml::js_string(password.unwrap_or(&Value::Null)),
        allow_private: policy.allow_private_hosts,
    };
    let (calendars, books) = tokio::join!(
        dav_client::discover_calendars(&server, &creds),
        dav_client::discover_address_books(&server, &creds)
    );
    let mut out = Map::new();
    let failed_both = calendars.is_err() && books.is_err();
    match calendars {
        Ok(c) => {
            out.insert("calendars".into(), serde_json::to_value(c).unwrap_or_default());
        }
        Err(e) => {
            out.insert(
                "calendarsError".into(),
                Value::String(if e.is_empty() { "Calendar discovery failed".into() } else { e }),
            );
        }
    }
    match books {
        Ok(b) => {
            let list: Vec<Value> =
                b.into_iter().map(|b| json!({ "href": b.url, "displayName": b.display_name })).collect();
            out.insert("addressBooks".into(), Value::Array(list));
        }
        Err(e) => {
            out.insert(
                "addressBooksError".into(),
                Value::String(if e.is_empty() { "Address book discovery failed".into() } else { e }),
            );
        }
    }
    if failed_both {
        let mut body = Map::new();
        body.insert("error".into(), Value::String("No calendars or address books found for this account".into()));
        body.extend(out);
        return Ok(json(502, Value::Object(body)));
    }
    Ok(ok(Value::Object(out)))
}

/// One collection entry for storage, after its href passed the SSRF check.
async fn build_collections(
    entries: &[Value],
    policy: dav_client::ConnectionPolicy,
    default_name: &str,
    with_color: bool,
) -> std::result::Result<Vec<Value>, String> {
    let mut out = Vec::new();
    for entry in entries {
        let (href, hash) = dav_client::validated_collection_href(entry.get("href"), policy).await?;
        let display = entry
            .get("displayName")
            .filter(|v| truthy(Some(v)))
            .cloned()
            .unwrap_or_else(|| Value::String(default_name.into()));
        let selected = entry.get("selected") != Some(&Value::Bool(false));
        let mut m = Map::new();
        m.insert("href".into(), Value::String(href));
        m.insert("hrefHash".into(), Value::String(hash));
        m.insert("displayName".into(), display);
        if with_color {
            m.insert("color".into(), entry.get("color").filter(|v| truthy(Some(v))).cloned().unwrap_or(Value::Null));
        }
        m.insert("selected".into(), Value::Bool(selected));
        out.push(Value::Object(m));
    }
    Ok(out)
}

fn enc(v: &Value) -> Result<String> {
    crypto::encrypt(&inboxora_core::json::stringify(v)).map_err(Error::internal)
}

async fn create(State(s): State<AppState>, auth: Auth, body: Body) -> Result<Response> {
    let (username, password, display) = (body.get("username"), body.get("password"), body.get("displayName"));
    if !truthy(display) || !truthy(username) || !truthy(password) {
        return Ok(err(400, "displayName, username and password are required"));
    }
    let calendars_in = body.get("calendars").and_then(Value::as_array);
    let books_in = body.get("addressBooks").and_then(Value::as_array);
    if calendars_in.is_none_or(Vec::is_empty) && books_in.is_none_or(Vec::is_empty) {
        return Ok(err(400, "Select at least one calendar or address book to import"));
    }
    let resolved =
        match dav_client::resolve_caldav_url(as_text(body.get("url")).as_deref(), username.and_then(Value::as_str)) {
            Ok(u) => u,
            Err(e) => return Ok(err(400, &e)),
        };
    let (url, policy) = match dav_client::validate_source_url(&s.db, &resolved).await? {
        Ok(v) => v,
        Err(e) => return Ok(err(400, &e)),
    };
    let calendars = match build_collections(calendars_in.map_or(&[][..], Vec::as_slice), policy, "Calendar", true).await
    {
        Ok(c) => c,
        Err(e) => return Ok(err(400, &e)),
    };
    let books = match build_collections(books_in.map_or(&[][..], Vec::as_slice), policy, "Contacts", false).await {
        Ok(c) => c,
        Err(e) => return Ok(err(400, &e)),
    };
    let interval = dav_client::clamp_interval_seconds(body.get("intervalSeconds"));
    let normalized = url.to_string();
    let fingerprint = crypto::sha256_hex(&normalized);
    let books_value = if books_in.is_some() { Some(enc(&Value::Array(books.clone()))?) } else { None };
    let js = |v: Option<&Value>| crate::services::dav_xml::js_string(v.unwrap_or(&Value::Null));
    let inserted = s
        .db
        .opt(
            "INSERT INTO calendar_import_sources (user_id, kind, url, url_fingerprint, username, password, display_name, interval_seconds, writable, collections, address_book_collections) \
             VALUES ($1,'caldav',$2,$3,$4,$5,$6,$7,$8,$9,$10) RETURNING *",
            &[
                Val::from(&auth.user_id),
                Val::from(crypto::encrypt(&normalized).map_err(Error::internal)?),
                Val::from(fingerprint),
                Val::from(js(username)),
                Val::from(crypto::encrypt(&js(password)).map_err(Error::internal)?),
                Val::from(js(display)),
                Val::from(interval),
                Val::from(truthy(body.get("writable"))),
                Val::from(enc(&Value::Array(calendars.clone()))?),
                Val::from(books_value),
            ],
        )
        .await;
    let row = match inserted {
        Ok(Some(r)) => r,
        Ok(None) => return Err(Error::internal("insert returned no row")),
        Err(e) if e.is_unique_violation() => return Ok(err(409, "A server with this URL already exists")),
        Err(e) if e.pg_code() == Some("23514") => {
            return Ok(err(409, "Server URL could not be stored securely"));
        }
        Err(e) => return Err(e),
    };
    let source = Source::from_row(&row);
    dav_sync::schedule_calendar_source(&s.db, &source);
    dav_sync::schedule_carddav_side(&s.db, &source);
    let ok_result = || json!({ "ok": true });
    let (cal, book) = tokio::join!(
        async {
            if calendars.is_empty() {
                Ok(ok_result())
            } else {
                dav_sync::sync_calendar_source(&s.db, &auth.user_id, &source.id).await
            }
        },
        async {
            if books.is_empty() {
                Ok(ok_result())
            } else {
                dav_sync::sync_carddav_side(&s.db, &auth.user_id, &source.id).await
            }
        }
    );
    let (cal, book) = (cal?, book?);
    let failed = |v: &Value| v.get("ok") != Some(&Value::Bool(true));
    if failed(&cal) || failed(&book) {
        let err_of = |v: &Value| v.get("error").and_then(Value::as_str).map(str::to_owned);
        let cal_err = if failed(&cal) { Some(err_of(&cal)) } else { None };
        let book_err = if failed(&book) { Some(err_of(&book)) } else { None };
        let message = cal_err
            .clone()
            .flatten()
            .filter(|e| !e.is_empty())
            .or_else(|| book_err.clone().flatten().filter(|e| !e.is_empty()))
            .unwrap_or_else(|| "Sync failed".into());
        return Ok(json(
            502,
            json!({ "error": message, "server": public_server(&row, cal_err, book_err), "calendarSync": cal, "addressBookSync": book }),
        ));
    }
    Ok(json(201, json!({ "server": public_server(&row, None, None), "calendarSync": cal, "addressBookSync": book })))
}

async fn sync(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> Result<Response> {
    let (cal, book) = tokio::join!(
        dav_sync::sync_calendar_source(&s.db, &auth.user_id, &id),
        dav_sync::sync_carddav_side(&s.db, &auth.user_id, &id)
    );
    let (cal, book) = (cal?, book?);
    let not_found = |v: &Value| {
        v.get("ok") != Some(&Value::Bool(true))
            && matches!(
                v.get("error").and_then(Value::as_str),
                Some("Calendar source not found" | "DAV server not found")
            )
    };
    if not_found(&cal) && not_found(&book) {
        return Ok(err(404, "DAV server not found"));
    }
    Ok(ok(json!({ "calendarSync": cal, "addressBookSync": book })))
}

/// Selection toggles (`{ id, selected }`) applied to the stored list.
fn apply_toggles(current: Vec<Value>, incoming: &[Value]) -> Vec<Value> {
    let toggles: Vec<(String, bool)> = incoming
        .iter()
        .filter(|e| truthy(e.get("id")))
        .map(|e| (crate::services::dav_xml::js_string(&e["id"]), truthy(e.get("selected"))))
        .collect();
    current
        .into_iter()
        .map(|mut c| {
            let hash = c.get("hrefHash").map(crate::services::dav_xml::js_string).unwrap_or_else(|| "undefined".into());
            // `new Map(...)`: the last toggle for an id wins.
            if let Some((_, sel)) = toggles.iter().rev().find(|(id, _)| *id == hash)
                && let Value::Object(m) = &mut c
            {
                m.insert("selected".into(), Value::Bool(*sel));
            }
            c
        })
        .collect()
}

async fn update(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, body: Body) -> Result<Response> {
    let existing =
        s.db.opt(
            "SELECT * FROM calendar_import_sources WHERE id = $1 AND user_id = $2 AND kind = 'caldav'",
            params![&id, &auth.user_id],
        )
        .await?;
    let Some(existing) = existing else { return Ok(err(404, "DAV server not found")) };
    let mut sets: Vec<String> = Vec::new();
    let mut values: Vec<Val> = Vec::new();
    let mut push = |col: &str, v: Val| {
        values.push(v);
        sets.push(format!("{col} = ${}", values.len()));
    };
    if let Some(d) = body.get("displayName") {
        let trimmed = d.as_str().map(inboxora_mail::text::js_trim).filter(|t| !t.is_empty());
        let Some(t) = trimmed else { return Ok(err(400, "displayName must not be empty")) };
        push("display_name", Val::from(t));
    }
    if let Some(v) = body.get("intervalSeconds") {
        let parsed = inboxora_pim::ical::js_parse_int(&crate::services::dav_xml::js_string(v))
            .filter(|i| (15..=86400).contains(i));
        let Some(i) = parsed else { return Ok(err(400, "intervalSeconds must be between 15 and 86400")) };
        push("interval_seconds", Val::from(i));
    }
    if body.get("enabled").is_some() {
        push("enabled", Val::from(truthy(body.get("enabled"))));
    }
    if truthy(body.get("username")) {
        push("username", Val::from(crate::services::dav_xml::js_string(&body.0["username"])));
    }
    if truthy(body.get("password")) {
        let p = crate::services::dav_xml::js_string(&body.0["password"]);
        push("password", Val::from(crypto::encrypt(&p).map_err(Error::internal)?));
    }
    let writable_changed = body.get("writable").is_some();
    if writable_changed {
        push("writable", Val::from(truthy(body.get("writable"))));
    }
    let policy = dav_client::connection_policy(&s.db).await?;
    let existing_source = Source::from_row(&existing);
    let mut next_calendars: Option<Vec<Value>> = None;
    if let Some(incoming) = body.get("calendars").and_then(Value::as_array) {
        let built = if incoming.iter().any(|e| truthy(e.get("href"))) {
            match build_collections(incoming, policy, "Calendar", true).await {
                Ok(c) => c,
                Err(e) => return Ok(err(400, &e)),
            }
        } else {
            apply_toggles(
                dav_sync::parse_collections(existing_source.collections.as_deref()).unwrap_or_default(),
                incoming,
            )
        };
        push("collections", Val::from(enc(&Value::Array(built.clone()))?));
        next_calendars = Some(built);
    }
    let mut next_books: Option<Vec<Value>> = None;
    if let Some(incoming) = body.get("addressBooks").and_then(Value::as_array) {
        let built = if incoming.iter().any(|e| truthy(e.get("href"))) {
            match build_collections(incoming, policy, "Contacts", false).await {
                Ok(c) => c,
                Err(e) => return Ok(err(400, &e)),
            }
        } else {
            apply_toggles(
                dav_sync::parse_collections(existing_source.address_book_collections.as_deref()).unwrap_or_default(),
                incoming,
            )
        };
        push("address_book_collections", Val::from(enc(&Value::Array(built.clone()))?));
        next_books = Some(built);
    }
    if sets.is_empty() {
        return Ok(err(400, "No editable fields were provided"));
    }
    values.push(Val::from(&id));
    values.push(Val::from(&auth.user_id));
    let sql = format!(
        "UPDATE calendar_import_sources SET {}, updated_at = NOW() WHERE id = ${} AND user_id = ${} RETURNING *",
        sets.join(", "),
        values.len() - 1,
        values.len()
    );
    let Some(row) = s.db.opt(&sql, &values).await? else { return Ok(err(404, "DAV server not found")) };
    let source = Source::from_row(&row);
    if row.try_get::<_, Option<bool>>("enabled").ok().flatten().unwrap_or(false) {
        dav_sync::schedule_calendar_source(&s.db, &source);
        dav_sync::schedule_carddav_side(&s.db, &source);
    } else {
        CALENDARS.stop(&source.id).await;
        CALENDARS.release(&source.id);
        ADDRESS_BOOKS.stop(&source.id).await;
        ADDRESS_BOOKS.release(&source.id);
    }
    if writable_changed {
        s.db.exec(
            "UPDATE calendars SET read_only = $1, updated_at = NOW() WHERE user_id = $2 AND owner_user_id = $2 AND (external_url = $3 OR external_url LIKE $3 || ':%')",
            params![!source.writable, &auth.user_id, format!("source:{}", source.id)],
        )
        .await?;
    }
    let deselected = |list: &[Value]| -> Vec<String> {
        list.iter()
            .filter(|c| !truthy(c.get("selected")))
            .map(|c| {
                format!(
                    "source:{}:{}",
                    source.id,
                    c.get("hrefHash").map(crate::services::dav_xml::js_string).unwrap_or_else(|| "undefined".into())
                )
            })
            .collect()
    };
    if let Some(list) = &next_calendars {
        let gone = deselected(list);
        if !gone.is_empty() {
            s.db.exec(
                "DELETE FROM calendars WHERE user_id = $1 AND owner_user_id = $1 AND external_url = ANY($2::text[])",
                params![&auth.user_id, gone],
            )
            .await?;
        }
    }
    if let Some(list) = &next_books {
        let gone = deselected(list);
        if !gone.is_empty() {
            s.db.exec("DELETE FROM address_books WHERE user_id = $1 AND source = 'carddav' AND external_url = ANY($2::text[])", params![&auth.user_id, gone])
                .await?;
        }
    }
    Ok(ok(json!({ "server": public_server(&row, None, None) })))
}

async fn remove(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> Result<Response> {
    let existing =
        s.db.opt(
            "SELECT * FROM calendar_import_sources WHERE id = $1 AND user_id = $2 AND kind = 'caldav'",
            params![&id, &auth.user_id],
        )
        .await?;
    let Some(existing) = existing else { return Ok(err(404, "DAV server not found")) };
    let release = || {
        CALENDARS.release(&id);
        ADDRESS_BOOKS.release(&id);
    };
    CALENDARS.stop(&id).await;
    ADDRESS_BOOKS.stop(&id).await;
    let mut deleted = false;
    let run = async {
        let row =
            s.db.opt(
                "DELETE FROM calendar_import_sources WHERE id = $1 AND user_id = $2 RETURNING id",
                params![&id, &auth.user_id],
            )
            .await?;
        if row.is_none() {
            return Ok::<_, Error>(false);
        }
        deleted = true;
        let prefix = format!("source:{id}");
        s.db.exec(
            "DELETE FROM calendars WHERE user_id = $1 AND owner_user_id = $1 AND (external_url = $2 OR external_url LIKE $2 || ':%')",
            params![&auth.user_id, &prefix],
        )
        .await?;
        s.db.exec(
            "DELETE FROM address_books WHERE user_id = $1 AND source = 'carddav' AND external_url LIKE $2",
            params![&auth.user_id, format!("{prefix}:%")],
        )
        .await?;
        Ok(true)
    };
    match run.await {
        Ok(true) => {
            release();
            Ok(empty(204))
        }
        Ok(false) => {
            release();
            Ok(err(404, "DAV server not found"))
        }
        Err(e) => {
            release();
            if !deleted {
                let source = Source::from_row(&existing);
                dav_sync::schedule_calendar_source(&s.db, &source);
                dav_sync::schedule_carddav_side(&s.db, &source);
            }
            Err(e)
        }
    }
}
