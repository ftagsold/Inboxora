//! OpenID Connect sign-in (port of `routes/oidc.ts`): the JSON API under `/api/auth/oidc` and
//! the browser flow under `/auth/oidc/:slug/{start,callback}` (authorization code + PKCE).

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use axum::routing::{delete, get};
use base64::Engine;
use inboxora_core::pg::{Q, Val};
use inboxora_core::session::Session;
use inboxora_core::{Error, Result, crypto, params};
use serde_json::{Map, Value, json};

use super::dav::express_redirect;
use super::util::{err, ok};
use crate::http::{Auth, ClientIp, Qs, Sess};
use crate::services::auth::{ensure_user_dav_resources, log_auth_event};
use crate::services::dav_xml::js_string;
use crate::services::oidc;
use crate::state::AppState;

/// `/api/auth/oidc`.
pub fn api_router() -> Router<AppState> {
    Router::new()
        .route("/providers", get(providers))
        .route("/identities", get(identities))
        .route("/identities/{id}", delete(unlink_identity))
}

/// `/auth/oidc`.
pub fn browser_router() -> Router<AppState> {
    Router::new().route("/{slug}/start", get(start)).route("/{slug}/callback", get(callback))
}

async fn providers(State(s): State<AppState>) -> Response {
    let rows =
        s.db.json_rows("SELECT id, name, slug FROM oidc_providers WHERE enabled = true ORDER BY name ASC", params![])
            .await;
    let Ok(rows) = rows else { return err(500, "Failed to load providers") };
    let requested = inboxora_core::config::env("OIDC_AUTO_LOGIN")
        .map(|v| inboxora_mail::text::js_trim(&v).to_lowercase())
        .filter(|v| !v.is_empty());
    let auto = requested.filter(|r| rows.iter().any(|p| p.get("slug").and_then(Value::as_str) == Some(r.as_str())));
    ok(json!({ "providers": rows, "autoLoginSlug": auto }))
}

async fn identities(State(s): State<AppState>, auth: Auth) -> Response {
    let rows = s
        .db
        .json_rows(
            "SELECT ui.id, ui.issuer, ui.email, ui.created_at, ui.last_used_at, op.name AS provider_name, op.slug AS provider_slug \
             FROM user_identities ui JOIN oidc_providers op ON op.id = ui.provider_id WHERE ui.user_id = $1 ORDER BY ui.created_at ASC",
            params![&auth.user_id],
        )
        .await;
    match rows {
        Ok(rows) => ok(json!({ "identities": rows })),
        Err(_) => err(500, "Failed to load identities"),
    }
}

async fn unlink_identity(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> Response {
    let run = async {
        let has_password =
            s.db.opt("SELECT password_hash FROM users WHERE id = $1", params![&auth.user_id])
                .await?
                .and_then(|r| r.get::<_, Option<String>>("password_hash"))
                .is_some_and(|h| !h.is_empty());
        if !has_password {
            let count: i64 =
                s.db.opt("SELECT COUNT(*) AS count FROM user_identities WHERE user_id = $1", params![&auth.user_id])
                    .await?
                    .map_or(0, |r| r.get("count"));
            if count <= 1 {
                return Ok(err(400, "Cannot unlink your only login method. Set a password first."));
            }
        }
        s.db.exec("DELETE FROM user_identities WHERE id = $1 AND user_id = $2", params![id, &auth.user_id]).await?;
        Ok::<_, Error>(ok(json!({ "ok": true })))
    };
    run.await.unwrap_or_else(|_| err(500, "Failed to unlink identity"))
}

// ── browser flow ─────────────────────────────────────────────────────────────────────

struct Req<'a> {
    headers: &'a HeaderMap,
    method: &'a Method,
}

impl Req<'_> {
    fn redirect(&self, location: &str) -> Response {
        express_redirect(self.headers, self.method, StatusCode::FOUND, location)
    }

    /// `oidcError(res, action, message)`.
    fn fail(&self, action: &str, message: &str) -> Response {
        let base = if action == "link" { "/" } else { "/login" };
        self.redirect(&format!("{base}?oidc_error={}", inboxora_mail::sanitize::encode_uri_component(message)))
    }
}

fn redirect_uri(slug: &str) -> String {
    format!("{}/auth/oidc/{slug}/callback", inboxora_core::config::env("APP_URL").unwrap_or_else(|| "undefined".into()))
}

fn now_ms() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64
}

async fn start(
    State(s): State<AppState>,
    Sess(session): Sess,
    Path(slug): Path<String>,
    q: Qs,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let r = Req { headers: &headers, method: &method };
    let is_link = q.get("action").and_then(Value::as_str) == Some("link");
    let action = if is_link { "link" } else { "login" };
    if is_link && session.user_id().is_none_or(|u| u.is_empty()) {
        return r.fail(action, "Not authenticated");
    }
    let run = async {
        let provider =
            s.db.json_opt("SELECT * FROM oidc_providers WHERE slug = $1 AND enabled = true", params![&slug])
                .await
                .map_err(|e| e.to_string())?;
        let Some(provider) = provider else { return Ok(r.fail(action, "SSO provider not found")) };
        let text = |k: &str| provider.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        let insecure = provider.get("allow_insecure").and_then(Value::as_bool).unwrap_or(false);
        let doc = oidc::discovery(&text("issuer_url"), insecure).await?;
        let state = crypto::random_hex(16);
        let nonce = crypto::random_hex(16);
        let verifier = crypto::random_base64url(32);
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(<sha2::Sha256 as sha2::Digest>::digest(verifier.as_bytes()));
        let mut pending = Map::new();
        pending.insert("state".into(), Value::String(state.clone()));
        pending.insert("nonce".into(), Value::String(nonce.clone()));
        pending.insert("verifier".into(), Value::String(verifier));
        pending.insert("providerId".into(), provider.get("id").cloned().unwrap_or(Value::Null));
        pending.insert("action".into(), Value::String(action.into()));
        if is_link {
            pending.insert("linkUserId".into(), session.get("userId").unwrap_or(Value::Null));
        }
        pending.insert("expiresAt".into(), json!(now_ms() + 10.0 * 60_000.0));
        session.set("oidcPending", Value::Object(pending));
        let scope = provider
            .get("scopes")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("openid email profile")
            .to_owned();
        let params = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", &text("client_id"))
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &redirect_uri(&text("slug")))
            .append_pair("scope", &scope)
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .finish();
        s.sessions.save(&session).await.map_err(|e| e.to_string())?;
        Ok::<_, String>(
            r.redirect(&format!("{}?{params}", js_string(doc.get("authorization_endpoint").unwrap_or(&Value::Null)))),
        )
    };
    match run.await {
        Ok(res) => res,
        Err(e) => {
            tracing::error!("OIDC start error: {e}");
            r.fail(action, "Failed to initiate SSO")
        }
    }
}

/// `resolveAdminFromClaim(payload, provider)`: `None` when the provider maps no admin group.
fn admin_from_claim(payload: &Map<String, Value>, provider: &Map<String, Value>) -> Option<bool> {
    let claim = provider.get("admin_group_claim").and_then(Value::as_str).filter(|c| !c.is_empty())?;
    let value = provider.get("admin_group_value").and_then(Value::as_str).filter(|v| !v.is_empty())?;
    Some(match payload.get(claim) {
        None => false,
        Some(Value::Array(items)) => items.iter().any(|i| i.as_str() == Some(value)),
        Some(Value::String(s)) => s == value,
        Some(_) => false,
    })
}

/// A verified SSO login, ready to sign the session in.
struct Login<'a> {
    user: &'a Map<String, Value>,
    provider: &'a Map<String, Value>,
    claims: &'a Map<String, Value>,
    email: Option<&'a str>,
    tokens: &'a Map<String, Value>,
    ip: &'a str,
}

/// Sign the session in after a successful SSO login and run the follow-ups.
async fn complete_login(s: &AppState, session: &Session, l: Login<'_>) -> std::result::Result<(), String> {
    let Login { user, provider, claims, email, tokens, ip } = l;
    s.sessions.regenerate(session).await.map_err(|e| e.to_string())?;
    let uid = user.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
    session.set("userId", uid.clone());
    session.set("username", user.get("username").cloned().unwrap_or(Value::Null));
    session.set("isAdmin", user.get("is_admin").cloned().unwrap_or(Value::Null));
    session.set("oidcProviderId", provider.get("id").cloned().unwrap_or(Value::Null));
    if let Some(id_token) = tokens.get("id_token").filter(|t| inboxora_core::json::truthy(Some(t))) {
        session.set("oidcIdToken", id_token.clone());
    }
    s.sessions.save(session).await.map_err(|e| e.to_string())?;
    let text = |k: &str| provider.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let (pid, pname, slug) = (text("id"), text("name"), text("slug"));
    let m = oidc::MailProvision {
        user_id: &uid,
        provider_id: &pid,
        provider_name: &pname,
        slug: &slug,
        payload: claims,
        email,
        tokens,
    };
    if let Err(e) = oidc::provision_mail_account(&s.db, m).await {
        tracing::error!("OIDC mail auto-provision failed: {e}");
    }
    if let Some(hook) = &s.on_user_connect {
        hook(uid.clone());
    }
    log_auth_event(&s.db, "sso_login", user.get("username").and_then(Value::as_str), Some(&uid), Some(ip), true);
    Ok(())
}

async fn backfill_display_name(q: &impl Q, user_id: &str, claims: &Map<String, Value>) -> Result<()> {
    let name = claims.get("name").and_then(Value::as_str).map(inboxora_mail::text::js_trim).unwrap_or("");
    if name.is_empty() {
        return Ok(());
    }
    let units: Vec<u16> = name.encode_utf16().take(100).collect();
    q.exec(
        "UPDATE users SET display_name = $1 WHERE id = $2 AND (display_name IS NULL OR display_name = '')",
        params![String::from_utf16_lossy(&units), user_id],
    )
    .await?;
    Ok(())
}

const IDENTITY_UPSERT: &str = "INSERT INTO user_identities (user_id, provider_id, issuer, subject, email, email_verified, last_used_at) \
     VALUES ($1, $2, $3, $4, $5, $6, NOW()) ON CONFLICT (issuer, subject) DO UPDATE SET last_used_at = NOW()";

#[allow(clippy::too_many_lines)]
async fn callback(
    State(s): State<AppState>,
    Sess(session): Sess,
    ClientIp(ip): ClientIp,
    Path(_slug): Path<String>,
    q: Qs,
    headers: HeaderMap,
    method: Method,
) -> Response {
    let r = Req { headers: &headers, method: &method };
    let (code, state, error, description) =
        (q.first("code"), q.first("state"), q.first("error"), q.first("error_description"));
    let pending = session.get("oidcPending").filter(Value::is_object);
    let action = pending
        .as_ref()
        .and_then(|p| p.get("action"))
        .filter(|a| inboxora_core::json::truthy(Some(a)))
        .map_or_else(|| "login".to_owned(), js_string);
    if let Some(e) = error.filter(|e| !e.is_empty()) {
        tracing::error!("OIDC provider error: {e} {}", description.clone().unwrap_or_default());
        return r.fail(&action, description.as_deref().filter(|d| !d.is_empty()).unwrap_or(&e));
    }
    let expired =
        pending.as_ref().is_none_or(|p| p.get("expiresAt").and_then(Value::as_f64).is_none_or(|e| now_ms() > e));
    if expired {
        session.remove("oidcPending");
        return r.fail(&action, "SSO session expired — please try again");
    }
    let pending = pending.unwrap_or_default();
    let p_str = |k: &str| pending.get(k).and_then(Value::as_str).map(str::to_owned);
    if state.as_deref().is_none_or(|st| st.is_empty() || Some(st) != p_str("state").as_deref()) {
        session.remove("oidcPending");
        return r.fail(&action, "Invalid SSO state — please try again");
    }
    let Some(code) = code.filter(|c| !c.is_empty()) else {
        session.remove("oidcPending");
        return r.fail(&action, "No authorization code received");
    };
    session.remove("oidcPending");
    let pending_action = p_str("action").unwrap_or_else(|| "login".into());
    let run = async {
        let provider =
            s.db.json_opt(
                "SELECT * FROM oidc_providers WHERE id = $1 AND enabled = true",
                params![p_str("providerId").unwrap_or_default()],
            )
            .await
            .map_err(|e| e.to_string())?;
        let Some(provider) = provider else { return Ok(r.fail(&pending_action, "SSO provider not found or disabled")) };
        let text = |k: &str| provider.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        let insecure = provider.get("allow_insecure").and_then(Value::as_bool).unwrap_or(false);
        let doc = oidc::discovery(&text("issuer_url"), insecure).await?;
        let stored_secret = provider.get("client_secret").and_then(Value::as_str);
        let secret = match stored_secret {
            Some(v) if v.starts_with("enc:v1:") => match crypto::decrypt(v) {
                Some(d) => Some(d),
                None => {
                    tracing::error!("OIDC client secret decryption failed");
                    return Ok(r.fail(&pending_action, "OIDC provider configuration error"));
                }
            },
            Some(v) => Some(v.to_owned()),
            None => None,
        };
        let Some(token_endpoint) =
            doc.get("token_endpoint").filter(|v| inboxora_core::json::truthy(Some(v))).map(js_string)
        else {
            return Ok(r.fail(&pending_action, "OIDC discovery missing required field: token_endpoint"));
        };
        let redirect = redirect_uri(&text("slug"));
        let verifier = p_str("verifier").unwrap_or_default();
        let client_id = text("client_id");
        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect),
            ("client_id", &client_id),
            ("code_verifier", &verifier),
        ];
        if let Some(sec) = &secret {
            form.push(("client_secret", sec));
        }
        let (status, body) = oidc::token_request(&token_endpoint, &form, insecure).await?;
        if !(200..300).contains(&status) {
            tracing::error!("OIDC token exchange failed: {status} {body}");
            return Ok(r.fail(&pending_action, "Token exchange failed"));
        }
        let tokens: Map<String, Value> = serde_json::from_str(&body).map_err(|e| e.to_string())?;
        let issuer_doc = js_string(doc.get("issuer").unwrap_or(&Value::Null));
        let jwks_uri = js_string(doc.get("jwks_uri").unwrap_or(&Value::Null));
        let claims = match tokens.get("id_token").and_then(Value::as_str).filter(|t| !t.is_empty()) {
            None => Err("OIDC token response did not include an id_token".to_owned()),
            Some(t) => oidc::verify_id_token(t, &jwks_uri, Some(&issuer_doc), &client_id, insecure).await,
        };
        let claims = match claims {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("OIDC id_token verification failed: {e}");
                return Ok(r.fail(&pending_action, "Token verification failed"));
            }
        };
        if claims.get("nonce").and_then(Value::as_str) != p_str("nonce").as_deref() {
            return Ok(r.fail(&pending_action, "Nonce mismatch — please try again"));
        }
        let subject = claims.get("sub").cloned().unwrap_or(Value::Null);
        let issuer = claims.get("iss").cloned().unwrap_or(Value::Null);
        let email = claims.get("email").filter(|e| inboxora_core::json::truthy(Some(e))).map(js_string);
        let verified = matches!(claims.get("email_verified"), Some(Value::Bool(true)))
            || claims.get("email_verified").and_then(Value::as_str) == Some("true");
        let require_verified = provider.get("require_email_verified") != Some(&Value::Bool(false));
        if !verified && pending_action != "link" && require_verified {
            return Ok(r.fail(&pending_action, "Your email address must be verified with this SSO provider"));
        }
        if let Some(domains) = provider.get("allowed_domains").and_then(Value::as_str).filter(|d| !d.is_empty()) {
            let allowed: Vec<String> = domains
                .split(',')
                .map(|d| inboxora_mail::text::js_trim(d).to_lowercase())
                .filter(|d| !d.is_empty())
                .collect();
            if !allowed.is_empty() {
                let Some(e) = email.as_deref().filter(|_| verified || !require_verified) else {
                    return Ok(r.fail(&pending_action, "A verified email address is required for this SSO provider"));
                };
                let domain = e.split('@').nth(1).map(str::to_lowercase);
                if !domain.is_some_and(|d| !d.is_empty() && allowed.contains(&d)) {
                    return Ok(r.fail(&pending_action, "Your email domain is not permitted for this SSO provider"));
                }
            }
        }
        let provider_id = text("id");
        if pending_action == "link" {
            let Some(link_user) = p_str("linkUserId").filter(|u| !u.is_empty()) else {
                return Ok(r.fail("link", "Link session invalid"));
            };
            let inserted = s
                .db
                .exec(
                    "INSERT INTO user_identities (user_id, provider_id, issuer, subject, email, email_verified, last_used_at) VALUES ($1, $2, $3, $4, $5, $6, NOW())",
                    &[Val::from(&link_user), Val::from(&provider_id), Val::from(&issuer), Val::from(&subject), Val::from(email.clone()), Val::from(verified)],
                )
                .await;
            return match inserted {
                Ok(_) => Ok(r.redirect("/?oidc_success=linked")),
                Err(e) if e.is_unique_violation() => {
                    Ok(r.fail("link", "This identity is already linked to another account"))
                }
                Err(e) => Err(e.to_string()),
            };
        }
        let id_params = [Val::from(&issuer), Val::from(&subject)];
        let existing =
            s.db.opt("SELECT user_id FROM user_identities WHERE issuer = $1 AND subject = $2", &id_params)
                .await
                .map_err(|e| e.to_string())?;
        if let Some(existing) = existing {
            let uid = existing.get::<_, uuid::Uuid>("user_id").to_string();
            let Some(mut user) =
                s.db.json_opt("SELECT id, username, is_admin FROM users WHERE id = $1", params![&uid])
                    .await
                    .map_err(|e| e.to_string())?
            else {
                return Ok(r.fail("login", "Account not found"));
            };
            if let Some(admin) = admin_from_claim(&claims, &provider)
                && user.get("is_admin").and_then(Value::as_bool) != Some(admin)
            {
                s.db.exec("UPDATE users SET is_admin = $1 WHERE id = $2", params![admin, &uid])
                    .await
                    .map_err(|e| e.to_string())?;
                user.insert("is_admin".into(), Value::Bool(admin));
            }
            s.db.exec("UPDATE user_identities SET last_used_at = NOW() WHERE issuer = $1 AND subject = $2", &id_params)
                .await
                .map_err(|e| e.to_string())?;
            complete_login(
                &s,
                &session,
                Login {
                    user: &user,
                    provider: &provider,
                    claims: &claims,
                    email: email.as_deref(),
                    tokens: &tokens,
                    ip: &ip,
                },
            )
            .await?;
            return Ok(r.redirect("/?oidc_success=login"));
        }
        let mode = provider.get("provisioning_mode").and_then(Value::as_str).unwrap_or_default().to_owned();
        let identity = |uid: &str| {
            [
                Val::from(uid),
                Val::from(&provider_id),
                Val::from(&issuer),
                Val::from(&subject),
                Val::from(email.clone()),
                Val::from(verified),
            ]
        };
        match mode.as_str() {
            "disabled" => Ok(r.fail("login", "SSO login is not enabled for new users on this server")),
            "login_existing_only" => {
                if email.is_none() || (!verified && require_verified) {
                    return Ok(r.fail("login", "No account found. Contact your administrator."));
                }
                let Some(matched) = oidc::resolve_claim_value(
                    provider.get("login_match_claim").and_then(Value::as_str),
                    &claims,
                    email.as_deref(),
                ) else {
                    return Ok(r.fail("login", "No account found matching this identity. Contact your administrator."));
                };
                let Some(mut user) =
                    s.db.json_opt("SELECT id, username, is_admin FROM users WHERE username = $1", params![&matched])
                        .await
                        .map_err(|e| e.to_string())?
                else {
                    return Ok(r.fail("login", "No account found matching this identity. Contact your administrator."));
                };
                let uid = user.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
                if let Some(admin) = admin_from_claim(&claims, &provider)
                    && user.get("is_admin").and_then(Value::as_bool) != Some(admin)
                {
                    s.db.exec("UPDATE users SET is_admin = $1 WHERE id = $2", params![admin, &uid])
                        .await
                        .map_err(|e| e.to_string())?;
                    user.insert("is_admin".into(), Value::Bool(admin));
                }
                s.db.exec(IDENTITY_UPSERT, &identity(&uid)).await.map_err(|e| e.to_string())?;
                backfill_display_name(&s.db, &uid, &claims).await.map_err(|e| e.to_string())?;
                complete_login(
                    &s,
                    &session,
                    Login {
                        user: &user,
                        provider: &provider,
                        claims: &claims,
                        email: email.as_deref(),
                        tokens: &tokens,
                        ip: &ip,
                    },
                )
                .await?;
                Ok(r.redirect("/?oidc_success=login"))
            }
            "open" => {
                let Some(mail) = email.clone().filter(|_| verified || !require_verified) else {
                    return Ok(r.fail("login", "A verified email address is required to create an account via SSO"));
                };
                let admin = admin_from_claim(&claims, &provider);
                let lower = mail.to_lowercase();
                let user = s
                    .db
                    .transaction(false, 0, |tx| {
                        let (lower, identity) = (lower.clone(), identity("").to_vec());
                        let claims = claims.clone();
                        Box::pin(async move {
                            tx.exec("SELECT pg_advisory_xact_lock(7936352)", params![]).await?;
                            let existing = tx.json_opt("SELECT id, username, is_admin FROM users WHERE username = $1", params![&lower]).await?;
                            let user = match existing {
                                Some(mut u) => {
                                    if let Some(a) = admin
                                        && u.get("is_admin").and_then(Value::as_bool) != Some(a)
                                    {
                                        tx.exec("UPDATE users SET is_admin = $1 WHERE id = $2", &[Val::from(a), Val::from(u.get("id").cloned().unwrap_or(Value::Null))]).await?;
                                        u.insert("is_admin".into(), Value::Bool(a));
                                    }
                                    u
                                }
                                None => {
                                    let count: i64 = tx.opt("SELECT COUNT(*) AS count FROM users", params![]).await?.map_or(0, |r| r.get("count"));
                                    let u = tx
                                        .json_opt("INSERT INTO users (username, password_hash, is_admin) VALUES ($1, NULL, $2) RETURNING id, username, is_admin", params![&lower, admin == Some(true)])
                                        .await?
                                        .ok_or_else(|| Error::internal("insert returned no row"))?;
                                    if count == 0 {
                                        tx.exec(
                                            "INSERT INTO system_settings (key, value, updated_at) VALUES ('registration_open', 'false', NOW()) ON CONFLICT (key) DO UPDATE SET value = 'false', updated_at = NOW()",
                                            params![],
                                        )
                                        .await?;
                                    }
                                    u
                                }
                            };
                            let uid = user.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
                            ensure_user_dav_resources(tx, &uid).await?;
                            let mut p = identity;
                            p[0] = Val::from(&uid);
                            tx.exec(IDENTITY_UPSERT, &p).await?;
                            backfill_display_name(tx, &uid, &claims).await?;
                            Ok(user)
                        })
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                complete_login(
                    &s,
                    &session,
                    Login {
                        user: &user,
                        provider: &provider,
                        claims: &claims,
                        email: email.as_deref(),
                        tokens: &tokens,
                        ip: &ip,
                    },
                )
                .await?;
                Ok(r.redirect("/?oidc_success=login"))
            }
            _ => Ok(r.fail("login", "Unknown provisioning configuration")),
        }
    };
    match run.await {
        Ok(res) => res,
        Err(e) => {
            tracing::error!("OIDC callback error: {e}");
            r.fail(&pending_action, "Authentication failed. Please try again.")
        }
    }
}
