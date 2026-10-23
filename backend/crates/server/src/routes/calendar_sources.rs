//! `/api/calendar/sources`: imported calendars (CalDAV accounts and iCalendar URLs), the
//! `/sources` half of `routes/calendar.ts`.

use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, patch, post};
use inboxora_core::pg::{Q, Row, Val};
use inboxora_core::{Error, Result, crypto, params};
use serde_json::{Map, Value, json};

use super::dav_servers::{apply_toggles, as_text, build_collections, enc, text, ts};
use super::util::{empty, err, json, ok};
use crate::http::{Auth, Body};
use crate::services::dav_client;
use crate::services::dav_sync::{self, CALENDARS, Source};
use crate::services::dav_xml::js_string;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/sources", get(list).post(create))
        .route("/sources/{source_id}/sync", post(sync))
        .route("/sources/{source_id}", patch(update).delete(remove))
}

fn truthy(v: Option<&Value>) -> bool {
    inboxora_core::json::truthy(v)
}

/// `publicSource(source)`; `last_error` may be overridden by a just-failed sync.
fn public_source(r: &Row, last_error: Option<Option<String>>) -> Value {
    let url = text(r, "url");
    let decrypted = url.as_deref().filter(|u| !u.is_empty()).and_then(crypto::decrypt);
    let secrets: Vec<String> = match (&url, &decrypted) {
        (Some(u), None) if !u.is_empty() => vec![u.clone()],
        (Some(u), Some(d)) => [u.clone(), d.clone()].into_iter().filter(|s| !s.is_empty()).collect(),
        _ => Vec::new(),
    };
    let last_error = last_error.unwrap_or_else(|| text(r, "last_error")).map(|mut m| {
        for s in &secrets {
            m = m.replace(s.as_str(), "[redacted]");
        }
        m
    });
    let collections = dav_sync::parse_collections(text(r, "collections").as_deref()).map(|list| {
        list.iter()
            .map(|c| {
                let mut m = Map::new();
                for (out, from) in
                    [("id", "hrefHash"), ("displayName", "displayName"), ("color", "color"), ("selected", "selected")]
                {
                    if let Some(v) = c.get(from) {
                        m.insert(out.into(), v.clone());
                    }
                }
                Value::Object(m)
            })
            .collect::<Vec<_>>()
    });
    let opt = |v: Option<String>| v.map_or(Value::Null, Value::String);
    json!({
        "id": r.get::<_, uuid::Uuid>("id").to_string(),
        "kind": opt(text(r, "kind")),
        "displayName": opt(text(r, "display_name")),
        "color": opt(text(r, "color")),
        "intervalSeconds": r.try_get::<_, Option<i32>>("interval_seconds").ok().flatten(),
        "writable": r.try_get::<_, Option<bool>>("writable").ok().flatten().unwrap_or(false),
        "enabled": r.try_get::<_, Option<bool>>("enabled").ok().flatten(),
        "lastSyncAt": ts(r, "last_sync_at"),
        "lastError": opt(last_error),
        "collections": collections.map_or(Value::Null, Value::Array),
    })
}

async fn list(State(s): State<AppState>, auth: Auth) -> Result<Response> {
    let rows = s
        .db
        .rows(
            "SELECT id, kind, url, username, display_name, color, interval_seconds, writable, collections, enabled, last_sync_at, last_error \
             FROM calendar_import_sources WHERE user_id = $1 ORDER BY created_at ASC",
            params![&auth.user_id],
        )
        .await?;
    let sources: Vec<Value> = rows.iter().map(|r| public_source(r, None)).collect();
    Ok(ok(json!({ "sources": sources })))
}

async fn create(State(s): State<AppState>, auth: Auth, body: Body) -> Result<Response> {
    let kind = body.get("kind").and_then(Value::as_str).filter(|k| matches!(*k, "caldav" | "ical_url"));
    let Some(kind) = kind.filter(|_| truthy(body.get("displayName"))) else {
        return Ok(err(400, "kind and displayName are required"));
    };
    let (url, username, password) = (body.get("url"), body.get("username"), body.get("password"));
    if kind == "ical_url" && !truthy(url) {
        return Ok(err(400, "url is required"));
    }
    if kind == "caldav" && (!truthy(username) || !truthy(password)) {
        return Ok(err(400, "CalDAV sources require username and password"));
    }
    let resolved = if kind == "caldav" {
        match dav_client::resolve_caldav_url(as_text(url).as_deref(), username.and_then(Value::as_str)) {
            Ok(u) => u,
            Err(e) => return Ok(err(400, &e)),
        }
    } else {
        js_string(url.unwrap_or(&Value::Null))
    };
    let (parsed, policy) = match dav_client::validate_source_url(&s.db, &resolved).await? {
        Ok(v) => v,
        Err(e) => return Ok(err(400, &e)),
    };
    let mut collections: Option<String> = None;
    if kind == "caldav"
        && let Some(incoming) = body.get("collections").and_then(Value::as_array).filter(|a| !a.is_empty())
    {
        let built = match build_collections(incoming, policy, "Calendar", true).await {
            Ok(b) => b,
            Err(e) => return Ok(err(400, &e)),
        };
        collections = Some(enc(&Value::Array(built))?);
    }
    let interval = dav_client::clamp_interval_seconds(body.get("intervalSeconds"));
    let normalized = parsed.to_string();
    let password_value = if truthy(password) {
        Some(crypto::encrypt(&js_string(password.unwrap_or(&Value::Null))).map_err(Error::internal)?)
    } else {
        None
    };
    let inserted = s
        .db
        .opt(
            "INSERT INTO calendar_import_sources (user_id, kind, url, url_fingerprint, username, password, display_name, color, interval_seconds, writable, collections) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) RETURNING *",
            &[
                Val::from(&auth.user_id),
                Val::from(kind),
                Val::from(crypto::encrypt(&normalized).map_err(Error::internal)?),
                Val::from(crypto::sha256_hex(&normalized)),
                Val::from(as_text(username)),
                Val::from(password_value),
                Val::from(js_string(body.get("displayName").unwrap_or(&Value::Null))),
                Val::from(body.get("color").cloned().filter(|c| !c.is_null()).map(|c| js_string(&c))),
                Val::from(interval),
                Val::from(kind == "caldav" && truthy(body.get("writable"))),
                Val::from(collections),
            ],
        )
        .await;
    let row = match inserted {
        Ok(Some(r)) => r,
        Ok(None) => return Err(Error::internal("insert returned no row")),
        Err(e) if e.is_unique_violation() => return Ok(err(409, "A source with this URL already exists")),
        Err(e) if e.pg_code() == Some("23514") => {
            return Ok(err(409, "Calendar source URL could not be stored securely"));
        }
        Err(e) => return Err(e),
    };
    let source = Source::from_row(&row);
    dav_sync::schedule_calendar_source(&s.db, &source);
    let sync = dav_sync::sync_calendar_source(&s.db, &auth.user_id, &source.id).await?;
    if sync.get("ok") != Some(&Value::Bool(true)) {
        let error = sync.get("error").and_then(Value::as_str).map(str::to_owned);
        return Ok(json(
            502,
            json!({ "error": error, "source": public_source(&row, Some(error.clone())), "sync": sync }),
        ));
    }
    Ok(json(201, json!({ "source": public_source(&row, None), "sync": sync })))
}

async fn sync(State(s): State<AppState>, auth: Auth, Path(source_id): Path<String>) -> Result<Response> {
    let result = dav_sync::sync_calendar_source(&s.db, &auth.user_id, &source_id).await?;
    if result.get("ok") != Some(&Value::Bool(true))
        && result.get("error").and_then(Value::as_str) == Some("Calendar source not found")
    {
        return Ok(err(404, "Calendar source not found"));
    }
    Ok(ok(result))
}

async fn update(State(s): State<AppState>, auth: Auth, Path(source_id): Path<String>, body: Body) -> Result<Response> {
    let Some(existing) = s
        .db
        .opt("SELECT * FROM calendar_import_sources WHERE id = $1 AND user_id = $2", params![&source_id, &auth.user_id])
        .await?
    else {
        return Ok(err(404, "Calendar source not found"));
    };
    let kind = text(&existing, "kind").unwrap_or_default();
    let mut sets: Vec<String> = Vec::new();
    let mut values: Vec<Val> = Vec::new();
    let mut push = |col: &str, v: Val| {
        values.push(v);
        sets.push(format!("{col} = ${}", values.len()));
    };
    if let Some(d) = body.get("displayName") {
        let Some(t) = d.as_str().map(inboxora_mail::text::js_trim).filter(|t| !t.is_empty()) else {
            return Ok(err(400, "displayName must not be empty"));
        };
        push("display_name", Val::from(t));
    }
    if let Some(c) = body.get("color") {
        push("color", Val::from(truthy(Some(c)).then(|| js_string(c))));
    }
    if let Some(v) = body.get("intervalSeconds") {
        let Some(i) = inboxora_pim::ical::js_parse_int(&js_string(v)).filter(|i| (15..=86400).contains(i)) else {
            return Ok(err(400, "intervalSeconds must be between 15 and 86400"));
        };
        push("interval_seconds", Val::from(i));
    }
    if body.get("enabled").is_some() {
        push("enabled", Val::from(truthy(body.get("enabled"))));
    }
    if let Some(u) = body.get("username") {
        push("username", Val::from(truthy(Some(u)).then(|| js_string(u))));
    }
    if truthy(body.get("password")) {
        push("password", Val::from(crypto::encrypt(&js_string(&body.0["password"])).map_err(Error::internal)?));
    }
    let writable_changed = body.get("writable").is_some() && kind == "caldav";
    if writable_changed {
        push("writable", Val::from(truthy(body.get("writable"))));
    }
    let mut next: Option<Vec<Value>> = None;
    if kind == "caldav"
        && let Some(incoming) = body.get("collections").and_then(Value::as_array)
    {
        let policy = dav_client::connection_policy(&s.db).await?;
        let built = if incoming.iter().any(|e| truthy(e.get("href"))) {
            match build_collections(incoming, policy, "Calendar", true).await {
                Ok(b) => b,
                Err(e) => return Ok(err(400, &e)),
            }
        } else {
            apply_toggles(
                dav_sync::parse_collections(text(&existing, "collections").as_deref()).unwrap_or_default(),
                incoming,
            )
        };
        push("collections", Val::from(enc(&Value::Array(built.clone()))?));
        next = Some(built);
    }
    if sets.is_empty() {
        return Ok(err(400, "No editable fields were provided"));
    }
    values.push(Val::from(&source_id));
    values.push(Val::from(&auth.user_id));
    let sql = format!(
        "UPDATE calendar_import_sources SET {}, updated_at = NOW() WHERE id = ${} AND user_id = ${} RETURNING *",
        sets.join(", "),
        values.len() - 1,
        values.len()
    );
    let Some(row) = s.db.opt(&sql, &values).await? else { return Ok(err(404, "Calendar source not found")) };
    let source = Source::from_row(&row);
    if row.try_get::<_, Option<bool>>("enabled").ok().flatten().unwrap_or(false) {
        dav_sync::schedule_calendar_source(&s.db, &source);
    } else {
        CALENDARS.stop(&source.id).await;
        CALENDARS.release(&source.id);
    }
    if writable_changed {
        s.db.exec(
            "UPDATE calendars SET read_only = $1, updated_at = NOW() WHERE user_id = $2 AND owner_user_id = $2 AND (external_url = $3 OR external_url LIKE $3 || ':%')",
            params![!source.writable, &auth.user_id, format!("source:{}", source.id)],
        )
        .await?;
    }
    if let Some(list) = next {
        let gone: Vec<String> = list
            .iter()
            .filter(|c| !truthy(c.get("selected")))
            .map(|c| {
                format!(
                    "source:{}:{}",
                    source.id,
                    c.get("hrefHash").map(js_string).unwrap_or_else(|| "undefined".into())
                )
            })
            .collect();
        if !gone.is_empty() {
            s.db.exec(
                "DELETE FROM calendars WHERE user_id = $1 AND owner_user_id = $1 AND external_url = ANY($2::text[])",
                params![&auth.user_id, gone],
            )
            .await?;
        }
    }
    Ok(ok(json!({ "source": public_source(&row, None) })))
}

async fn remove(State(s): State<AppState>, auth: Auth, Path(source_id): Path<String>) -> Result<Response> {
    let Some(existing) = s
        .db
        .opt("SELECT * FROM calendar_import_sources WHERE id = $1 AND user_id = $2", params![&source_id, &auth.user_id])
        .await?
    else {
        return Ok(err(404, "Calendar source not found"));
    };
    CALENDARS.stop(&source_id).await;
    let mut deleted = false;
    let run = async {
        let row =
            s.db.opt(
                "DELETE FROM calendar_import_sources WHERE id = $1 AND user_id = $2 RETURNING id",
                params![&source_id, &auth.user_id],
            )
            .await?;
        if row.is_none() {
            return Ok::<_, Error>(false);
        }
        deleted = true;
        s.db.exec(
            "DELETE FROM calendars WHERE user_id = $1 AND owner_user_id = $1 AND (external_url = $2 OR external_url LIKE $2 || ':%')",
            params![&auth.user_id, format!("source:{source_id}")],
        )
        .await?;
        Ok(true)
    };
    let result = run.await;
    CALENDARS.release(&source_id);
    match result {
        Ok(true) => Ok(empty(204)),
        Ok(false) => Ok(err(404, "Calendar source not found")),
        Err(e) => {
            if !deleted {
                dav_sync::schedule_calendar_source(&s.db, &Source::from_row(&existing));
            }
            Err(e)
        }
    }
}
