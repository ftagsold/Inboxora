//! `/api/webdav`: WebDAV file servers (Settings → Files) and the Files explorer's operations.
//! The URL and password are stored encrypted; the password is never sent back.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use inboxora_core::pg::{Q, Row, Val};
use inboxora_core::{Error, Result, crypto, params};
use serde_json::{Value, json};

use super::dav_servers::{text, ts};
use super::util::{err, json, ok};
use crate::http::query::Qs;
use crate::http::{Auth, Body};
use crate::services::webdav::{self, Conn, DavError};
use crate::state::AppState;

/// Total size of files attached to one message from WebDAV (matches the send limit).
const ATTACH_LIMIT: usize = 25 * 1024 * 1024;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/servers", get(list_servers).post(create))
        .route("/servers/{id}", axum::routing::patch(update).delete(remove))
        .route("/servers/{id}/list", get(list))
        .route("/servers/{id}/file", get(download).put(upload).delete(delete))
        .route("/servers/{id}/mkdir", post(mkdir))
        .route("/servers/{id}/move", post(move_item))
        .route("/servers/{id}/copy", post(copy_item))
        .route("/servers/{id}/attachments", post(attachments))
}

fn is_uuid(v: &str) -> bool {
    uuid::Uuid::parse_str(v).is_ok()
}

fn dav_err(e: DavError) -> Response {
    err(e.status, &e.message)
}

fn public_server(r: &Row) -> Value {
    let url = text(r, "url").and_then(|u| crypto::decrypt(&u)).unwrap_or_default();
    json!({
        "id": r.get::<_, uuid::Uuid>("id").to_string(),
        "displayName": text(r, "display_name"),
        "url": url,
        "username": text(r, "username"),
        "createdAt": ts(r, "created_at"),
    })
}

async fn list_servers(State(s): State<AppState>, auth: Auth) -> Result<Response> {
    let rows = s
        .db
        .rows("SELECT * FROM webdav_file_servers WHERE user_id = $1 ORDER BY created_at ASC", params![&auth.user_id])
        .await?;
    Ok(ok(json!({ "servers": rows.iter().map(public_server).collect::<Vec<_>>() })))
}

fn field(body: &Body, key: &str) -> Option<String> {
    body.str(key).map(inboxora_mail::text::js_trim).filter(|v| !v.is_empty()).map(str::to_owned)
}

/// Validate the URL against the connection policy and check the credentials work.
async fn connect(
    s: &AppState,
    raw_url: &str,
    username: String,
    password: String,
) -> Result<std::result::Result<Conn, Response>> {
    let (url, policy) = match crate::services::dav_client::validate_source_url(&s.db, raw_url).await? {
        Ok(v) => v,
        Err(e) => return Ok(Err(err(400, &e))),
    };
    let conn = Conn { base: url, username, password, allow_private: policy.allow_private_hosts };
    if let Err(e) = webdav::probe(&conn).await {
        return Ok(Err(err(e.status, &e.message)));
    }
    Ok(Ok(conn))
}

async fn create(State(s): State<AppState>, auth: Auth, body: Body) -> Result<Response> {
    let (Some(name), Some(url), Some(username)) =
        (field(&body, "displayName"), field(&body, "url"), field(&body, "username"))
    else {
        return Ok(err(400, "Name, URL and username are required"));
    };
    let Some(password) = body.str("password").filter(|p| !p.is_empty()).map(str::to_owned) else {
        return Ok(err(400, "Password is required"));
    };
    let conn = match connect(&s, &url, username.clone(), password.clone()).await? {
        Ok(c) => c,
        Err(r) => return Ok(r),
    };
    let row = s
        .db
        .opt(
            "INSERT INTO webdav_file_servers (user_id, display_name, url, username, password) VALUES ($1,$2,$3,$4,$5) RETURNING *",
            &[
                Val::from(&auth.user_id),
                Val::from(name),
                Val::from(crypto::encrypt(conn.base.as_str()).map_err(Error::internal)?),
                Val::from(username),
                Val::from(crypto::encrypt(&password).map_err(Error::internal)?),
            ],
        )
        .await?
        .ok_or_else(|| Error::internal("insert returned no row"))?;
    Ok(json(201, json!({ "server": public_server(&row) })))
}

async fn server_row(s: &AppState, user_id: &str, id: &str) -> Result<Option<Row>> {
    if !is_uuid(id) {
        return Ok(None);
    }
    s.db.opt("SELECT * FROM webdav_file_servers WHERE id = $1 AND user_id = $2", params![id, user_id]).await
}

async fn update(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, body: Body) -> Result<Response> {
    let Some(existing) = server_row(&s, &auth.user_id, &id).await? else {
        return Ok(err(404, "WebDAV server not found"));
    };
    if body.has("displayName") && field(&body, "displayName").is_none() {
        return Ok(err(400, "Name must not be empty"));
    }
    let name = field(&body, "displayName").or_else(|| text(&existing, "display_name")).unwrap_or_default();
    let old_url = text(&existing, "url").and_then(|u| crypto::decrypt(&u)).unwrap_or_default();
    let url = field(&body, "url").unwrap_or(old_url.clone());
    let username = field(&body, "username").or_else(|| text(&existing, "username")).unwrap_or_default();
    let new_password = body.str("password").filter(|p| !p.is_empty()).map(str::to_owned);
    let password = match &new_password {
        Some(p) => p.clone(),
        None => text(&existing, "password").and_then(|p| crypto::decrypt(&p)).unwrap_or_default(),
    };
    let connection_changed =
        url != old_url || Some(&username) != text(&existing, "username").as_ref() || new_password.is_some();
    let stored_url = if connection_changed {
        match connect(&s, &url, username.clone(), password.clone()).await? {
            Ok(c) => c.base.to_string(),
            Err(r) => return Ok(r),
        }
    } else {
        old_url
    };
    let row = s
        .db
        .opt(
            "UPDATE webdav_file_servers SET display_name = $1, url = $2, username = $3, password = $4, updated_at = NOW() \
             WHERE id = $5 AND user_id = $6 RETURNING *",
            &[
                Val::from(name),
                Val::from(crypto::encrypt(&stored_url).map_err(Error::internal)?),
                Val::from(username),
                Val::from(crypto::encrypt(&password).map_err(Error::internal)?),
                Val::from(&id),
                Val::from(&auth.user_id),
            ],
        )
        .await?;
    match row {
        Some(r) => Ok(ok(json!({ "server": public_server(&r) }))),
        None => Ok(err(404, "WebDAV server not found")),
    }
}

async fn remove(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> Result<Response> {
    if !is_uuid(&id) {
        return Ok(err(404, "WebDAV server not found"));
    }
    let n =
        s.db.exec("DELETE FROM webdav_file_servers WHERE id = $1 AND user_id = $2", params![&id, &auth.user_id])
            .await?;
    if n == 0 {
        return Ok(err(404, "WebDAV server not found"));
    }
    Ok(ok(json!({ "ok": true })))
}

/// The decrypted connection for one of the user's servers.
async fn conn_for(s: &AppState, user_id: &str, id: &str) -> Result<std::result::Result<Conn, Response>> {
    let Some(row) = server_row(s, user_id, id).await? else { return Ok(Err(err(404, "WebDAV server not found"))) };
    let url = text(&row, "url").and_then(|u| crypto::decrypt(&u));
    let password = text(&row, "password").and_then(|p| crypto::decrypt(&p));
    let (Some(url), Some(password)) = (url, password) else {
        return Ok(Err(err(500, "Stored WebDAV credentials could not be decrypted")));
    };
    let Ok(base) = url::Url::parse(&url) else { return Ok(Err(err(500, "Stored WebDAV URL is invalid"))) };
    let policy = crate::services::dav_client::connection_policy(&s.db).await?;
    Ok(Ok(Conn {
        base,
        username: text(&row, "username").unwrap_or_default(),
        password,
        allow_private: policy.allow_private_hosts,
    }))
}

macro_rules! conn {
    ($s:expr, $auth:expr, $id:expr) => {
        match conn_for(&$s, &$auth.user_id, &$id).await? {
            Ok(c) => c,
            Err(r) => return Ok(r),
        }
    };
}

fn path_param(q: &Qs) -> String {
    q.str("path").unwrap_or("/").to_owned()
}

async fn list(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, q: Qs) -> Result<Response> {
    let conn = conn!(s, auth, id);
    let path = path_param(&q);
    let Ok(normalized) = webdav::normalize(&path, true) else { return Ok(err(400, "Invalid path")) };
    match webdav::list(&conn, &path).await {
        Ok(entries) => Ok(ok(json!({ "path": normalized, "entries": entries }))),
        Err(e) => Ok(dav_err(e)),
    }
}

/// The type of a file the preview can show, by extension.
fn guess_type(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "opus" => "audio/ogg",
        "m4a" => "audio/mp4",
        "flac" => "audio/flac",
        "txt" | "md" | "log" | "ini" | "conf" | "cfg" => "text/plain",
        _ => return None,
    })
}

/// Types a browser may render inline from our origin without running script.
fn inline_safe(content_type: &str) -> bool {
    let t = content_type.to_ascii_lowercase();
    (t.starts_with("image/") && !t.contains("svg"))
        || t.starts_with("video/")
        || t.starts_with("audio/")
        || t == "application/pdf"
        || t == "text/plain"
}

async fn download(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, q: Qs) -> Result<Response> {
    let conn = conn!(s, auth, id);
    let path = path_param(&q);
    let res = match webdav::download(&conn, &path).await {
        Ok(r) => r,
        Err(e) => return Ok(dav_err(e)),
    };
    let name = path.trim_end_matches('/').rsplit('/').next().unwrap_or("download").to_owned();
    // Many servers answer application/octet-stream for everything: guess from the name then.
    let upstream = res.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let essence = upstream.as_deref().map(|v| v.split(';').next().unwrap_or(v).trim().to_ascii_lowercase());
    let (content_type, header_value) = match essence {
        Some(e) if !e.is_empty() && e != "application/octet-stream" => (e, upstream.unwrap_or_default()),
        _ => {
            let guessed = guess_type(&name).unwrap_or("application/octet-stream");
            (guessed.to_owned(), guessed.to_owned())
        }
    };
    let length = res.content_length();
    let inline = q.str("inline") == Some("1") && inline_safe(&content_type);
    let mut out = axum::body::Body::from_stream(res.bytes_stream()).into_response();
    let h = out.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&header_value).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    if let Some(l) = length {
        h.insert(header::CONTENT_LENGTH, HeaderValue::from(l));
    }
    let disposition = inboxora_mail::util::attachment_disposition(&name);
    let disposition = if inline { disposition.replacen("attachment", "inline", 1) } else { disposition };
    if let Ok(v) = HeaderValue::from_str(&disposition) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if inline {
        // The preview dialog embeds PDFs in a same-origin frame.
        h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("SAMEORIGIN"));
    }
    // Chrome's PDF viewer refuses sandboxed documents; every other inline type gets the sandbox.
    if content_type != "application/pdf" {
        h.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("sandbox; default-src 'none'; img-src 'self'; media-src 'self'"),
        );
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    out.extensions_mut().insert(crate::http::middleware::NoEtag);
    Ok(out)
}

async fn upload(
    State(s): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
    q: Qs,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Result<Response> {
    let conn = conn!(s, auth, id);
    let content_type = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let stream = inboxora_net::reqwest::Body::wrap_stream(body.into_data_stream());
    let overwrite = q.str("overwrite") == Some("1");
    match webdav::upload(&conn, &path_param(&q), stream, content_type, overwrite).await {
        Ok(()) => Ok(json(201, json!({ "ok": true }))),
        Err(e) => Ok(dav_err(e)),
    }
}

async fn delete(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, q: Qs) -> Result<Response> {
    let conn = conn!(s, auth, id);
    let dir = q.str("dir") == Some("1");
    match webdav::delete(&conn, &path_param(&q), dir).await {
        Ok(()) => Ok(ok(json!({ "ok": true }))),
        Err(e) => Ok(dav_err(e)),
    }
}

async fn mkdir(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, body: Body) -> Result<Response> {
    let conn = conn!(s, auth, id);
    let Some(path) = body.str("path") else { return Ok(err(400, "path is required")) };
    match webdav::mkdir(&conn, path).await {
        Ok(()) => Ok(json(201, json!({ "ok": true }))),
        Err(e) => Ok(dav_err(e)),
    }
}

async fn transfer(s: AppState, auth: Auth, id: String, body: Body, method: &str) -> Result<Response> {
    let conn = conn!(s, auth, id);
    let (Some(from), Some(to)) = (body.str("from"), body.str("to")) else {
        return Ok(err(400, "from and to are required"));
    };
    match webdav::transfer(&conn, method, from, to, body.truthy("dir"), body.truthy("overwrite")).await {
        Ok(()) => Ok(ok(json!({ "ok": true }))),
        Err(e) => Ok(dav_err(e)),
    }
}

async fn move_item(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, body: Body) -> Result<Response> {
    transfer(s, auth, id, body, "MOVE").await
}

async fn copy_item(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, body: Body) -> Result<Response> {
    transfer(s, auth, id, body, "COPY").await
}

/// Files fetched from the server as base64 compose attachments (25 MB in total).
async fn attachments(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, body: Body) -> Result<Response> {
    let conn = conn!(s, auth, id);
    let paths: Vec<String> = body
        .get("paths")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    if paths.is_empty() {
        return Ok(err(400, "paths is required"));
    }
    let mut remaining = ATTACH_LIMIT;
    let mut out = Vec::new();
    for path in paths {
        let (bytes, content_type) = match webdav::read_bytes(&conn, &path, remaining).await {
            Ok(v) => v,
            Err(e) => return Ok(dav_err(e)),
        };
        remaining -= bytes.len();
        let name = path.trim_end_matches('/').rsplit('/').next().unwrap_or("attachment").to_owned();
        out.push(json!({
            "name": name,
            "size": bytes.len(),
            "type": content_type.unwrap_or_else(|| "application/octet-stream".into()),
            "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
        }));
    }
    Ok(ok(json!({ "attachments": out })))
}
