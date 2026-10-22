//! Microsoft OAuth for mail accounts (the service half of `routes/oauth.ts`): token exchange,
//! id_token validation, account upsert and access-token refresh.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use inboxora_core::config::env;
use inboxora_core::pg::{Db, Q};
use inboxora_core::{Error, crypto, params};
use parking_lot::Mutex;
use serde_json::{Map, Value};

use super::oidc;

pub const SCOPES: &str = "https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send offline_access openid email profile";
const REFRESH_SCOPES: &str =
    "https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send offline_access";

/// The Microsoft identity platform base URL. Fixed in Node; `MS_AUTHORITY_URL` exists so the
/// Rust tests can point the flows at a mock authority.
pub fn authority() -> String {
    env("MS_AUTHORITY_URL").unwrap_or_else(|| "https://login.microsoftonline.com".into())
}

#[derive(Debug, Clone)]
pub struct MsConfig {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub tenant_id: String,
    pub redirect_uri: Option<String>,
}

/// `getMsConfig()`.
pub fn ms_config() -> MsConfig {
    MsConfig {
        client_id: env("MS_CLIENT_ID"),
        client_secret: env("MS_CLIENT_SECRET"),
        tenant_id: env("MS_TENANT_ID").unwrap_or_else(|| "common".into()),
        redirect_uri: env("MS_REDIRECT_URI"),
    }
}

/// A plain (non-guarded) HTTPS client: these endpoints are fixed Microsoft hosts.
static CLIENT: LazyLock<inboxora_net::reqwest::Client> = LazyLock::new(|| {
    inboxora_net::reqwest::Client::builder().timeout(Duration::from_secs(10)).build().expect("TLS backend available")
});

/// POST a form (`formBody`: an absent value serialises as the literal `undefined`).
pub async fn post_form(url: &str, form: &[(&str, Option<&str>)]) -> Result<(u16, Map<String, Value>), String> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form.iter().map(|(k, v)| (*k, v.unwrap_or("undefined"))))
        .finish();
    let res =
        CLIENT.post(url).header("Content-Type", "application/x-www-form-urlencoded").body(body).send().await.map_err(
            |e| {
                if e.is_timeout() {
                    "The operation was aborted due to timeout".to_owned()
                } else {
                    "fetch failed".to_owned()
                }
            },
        )?;
    let status = res.status().as_u16();
    let text = res.text().await.map_err(|_| "fetch failed".to_owned())?;
    let json: Map<String, Value> = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    Ok((status, json))
}

fn text<'a>(m: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    m.get(k).and_then(Value::as_str).filter(|v| !v.is_empty())
}

fn expiry_of(tokens: &Map<String, Value>) -> chrono::DateTime<chrono::Utc> {
    let secs = tokens.get("expires_in").and_then(Value::as_f64).filter(|s| s.is_finite() && *s > 0.0).unwrap_or(3600.0);
    chrono::Utc::now() + chrono::Duration::milliseconds((secs * 1000.0) as i64)
}

/// `processMicrosoftTokens(userId, tokens, ...)`: validate the identity and upsert the account.
/// Returns the account's email address.
pub async fn process_microsoft_tokens(
    db: &Db,
    user_id: &str,
    tokens: &Map<String, Value>,
    tenant_id: &str,
    client_id: Option<&str>,
    public_client: bool,
) -> Result<String, String> {
    let (Some(access), Some(refresh)) = (text(tokens, "access_token"), text(tokens, "refresh_token")) else {
        return Err("OAuth token response is missing access or refresh token — please reconnect your account".into());
    };
    let expiry = expiry_of(tokens);
    let mut email: Option<String> = None;
    let mut display: Option<String> = None;
    if let Some(id_token) = text(tokens, "id_token") {
        let base = authority();
        let fixed = matches!(tenant_id, "common" | "organizations" | "consumers");
        let issuer = (!fixed).then(|| format!("{base}/{tenant_id}/v2.0"));
        let jwks = format!("{base}/{tenant_id}/discovery/v2.0/keys");
        let verified = async {
            let claims = oidc::verify_id_token(
                id_token,
                &jwks,
                issuer.as_deref(),
                client_id.unwrap_or(""),
                base.starts_with("http://"),
            )
            .await?;
            if fixed && let (Some(tid), Some(iss)) = (text(&claims, "tid"), text(&claims, "iss")) {
                let expected = format!("{base}/{tid}/v2.0");
                if iss != expected {
                    return Err(format!("id_token issuer mismatch: expected {expected}, got {iss}"));
                }
            }
            Ok::<_, String>(claims)
        };
        match verified.await {
            Ok(claims) => {
                email = text(&claims, "email").or_else(|| text(&claims, "preferred_username")).map(str::to_owned);
                display = text(&claims, "name").map(str::to_owned);
            }
            Err(e) => {
                tracing::error!("Microsoft id_token validation failed: {e}");
                return Err("Could not validate Microsoft identity token — please try again".into());
            }
        }
    }
    let Some(email) = email else {
        return Err(
            "Could not retrieve email address from Microsoft profile — ensure the openid, email, and profile scopes are granted".into()
        );
    };
    let enc = |v: &str| crypto::encrypt(v).map_err(Error::internal);
    let (access_enc, refresh_enc) = (enc(access).map_err(|e| e.to_string())?, enc(refresh).map_err(|e| e.to_string())?);
    let colors = ["#0078d4", "#106ebe", "#005a9e", "#004578"];
    let color = colors[usize::from(crypto::random_bytes::<1>()[0]) % colors.len()];
    db.transaction(false, 0, |tx| {
        let (email, display, access_enc, refresh_enc) = (email.clone(), display.clone(), access_enc.clone(), refresh_enc.clone());
        let user_id = user_id.to_owned();
        Box::pin(async move {
            tx.exec("SELECT pg_advisory_xact_lock(hashtext($1))", params![format!("oauth-account:{}:{}", user_id, email.to_lowercase())]).await?;
            let existing = tx.opt("SELECT id FROM email_accounts WHERE user_id = $1 AND lower(email_address) = lower($2)", params![&user_id, &email]).await?;
            match existing {
                Some(row) => {
                    tx.exec(
                        "UPDATE email_accounts SET oauth_access_token = $1, oauth_refresh_token = $2, oauth_token_expiry = $3, name = $4, \
                         oauth_public_client = $5, oauth_provider = 'microsoft', sync_error = NULL WHERE id = $6",
                        params![access_enc, refresh_enc, expiry, display.clone().unwrap_or_else(|| email.clone()), public_client, row.get::<_, uuid::Uuid>("id")],
                    )
                    .await?;
                }
                None => {
                    tx.exec(
                        "INSERT INTO email_accounts (user_id, name, email_address, color, protocol, imap_host, imap_port, imap_tls, \
                         smtp_host, smtp_port, smtp_tls, auth_user, oauth_provider, oauth_access_token, oauth_refresh_token, oauth_token_expiry, oauth_public_client) \
                         VALUES ($1,$2,$3,$4,'imap','outlook.office365.com',993,true,'smtp.office365.com',587,'STARTTLS',$3,'microsoft',$5,$6,$7,$8)",
                        params![&user_id, display, &email, color, access_enc, refresh_enc, expiry, public_client],
                    )
                    .await?;
                }
            }
            Ok(())
        })
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(email)
}

type Refresh = Shared<BoxFuture<'static, Result<Map<String, Value>, String>>>;
static IN_FLIGHT: LazyLock<Mutex<HashMap<String, Refresh>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// `refreshMicrosoftToken(account)`: one refresh per account at a time; concurrent callers share
/// it. Returns the account row with the plaintext access token and new expiry.
pub async fn refresh_microsoft_token(db: &Db, account: Map<String, Value>) -> Result<Map<String, Value>, String> {
    let id = account.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let fut = {
        let mut m = IN_FLIGHT.lock();
        if let Some(f) = m.get(&id) {
            f.clone()
        } else {
            let (db, id2) = (db.clone(), id.clone());
            let f = async move {
                let r = do_refresh(&db, account).await;
                IN_FLIGHT.lock().remove(&id2);
                r
            }
            .boxed()
            .shared();
            m.insert(id.clone(), f.clone());
            f
        }
    };
    fut.await
}

async fn do_refresh(db: &Db, mut account: Map<String, Value>) -> Result<Map<String, Value>, String> {
    let cfg = ms_config();
    let stored =
        account.get("oauth_refresh_token").and_then(Value::as_str).and_then(crypto::decrypt).filter(|t| !t.is_empty());
    let Some(refresh) = stored else {
        return Err("OAuth refresh token is missing or corrupted — please reconnect your account".into());
    };
    let url = format!("{}/{}/oauth2/v2.0/token", authority(), cfg.tenant_id);
    let post = async |with_secret: bool| {
        let mut form = vec![
            ("client_id", cfg.client_id.as_deref()),
            ("refresh_token", Some(refresh.as_str())),
            ("grant_type", Some("refresh_token")),
            ("scope", Some(REFRESH_SCOPES)),
        ];
        if with_secret && let Some(sec) = cfg.client_secret.as_deref() {
            form.push(("client_secret", Some(sec)));
        }
        post_form(&url, &form).await
    };
    let was_public = account.get("oauth_public_client").and_then(Value::as_bool).unwrap_or(false);
    let send_secret = cfg.client_secret.is_some() && !was_public;
    let (mut status, mut tokens) = post(send_secret).await?;
    let mut became_public = false;
    let ok = |s: u16| (200..300).contains(&s);
    let description =
        |t: &Map<String, Value>| text(t, "error_description").or_else(|| text(t, "error")).unwrap_or("").to_owned();
    if !ok(status) && send_secret && description(&tokens).to_uppercase().contains("AADSTS90023") {
        (status, tokens) = post(false).await?;
        became_public = ok(status);
    }
    if !ok(status) {
        return Err(text(&tokens, "error_description").unwrap_or("Token refresh failed").to_owned());
    }
    let Some(access) = text(&tokens, "access_token").map(str::to_owned) else {
        return Err("Token refresh response is missing an access token — please reconnect your account".into());
    };
    let expiry = expiry_of(&tokens);
    let public = was_public || became_public;
    let enc = |v: &str| crypto::encrypt(v);
    db.exec(
        "UPDATE email_accounts SET oauth_access_token = $1, oauth_refresh_token = COALESCE($2, oauth_refresh_token), oauth_token_expiry = $3, \
         oauth_public_client = $4 WHERE id = $5",
        params![enc(&access)?, text(&tokens, "refresh_token").map(enc).transpose()?, expiry, public, account.get("id").cloned().unwrap_or(Value::Null)],
    )
    .await
    .map_err(|e| e.to_string())?;
    account.insert("oauth_access_token".into(), Value::String(access));
    account.insert("oauth_token_expiry".into(), Value::String(inboxora_core::json::js_iso(&expiry)));
    account.insert("oauth_public_client".into(), Value::Bool(public));
    Ok(account)
}
