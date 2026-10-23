//! OpenID Connect provider discovery (port of `oidcDiscovery.ts`) and RP-initiated logout
//! (`buildEndSessionUrl`).

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use inboxora_core::crypto;
use inboxora_core::params;
use inboxora_core::pg::{Db, Q};
use inboxora_net::reqwest;
use parking_lot::Mutex;
use serde_json::{Map, Value};

use crate::services::oauth::RefreshError;

const DISCOVERY_TTL: Duration = Duration::from_secs(5 * 60);

static CACHE: LazyLock<Mutex<HashMap<String, (Value, Instant)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// For `allow_insecure` providers: no certificate checks and no redirects (Node used a bare
/// `http(s).request` there).
static INSECURE: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .expect("TLS backend available")
});

fn host_of(url: &url::Url) -> String {
    crate::services::dav_client::hostname(url)
}

async fn fetch_json(url: &str, insecure: bool) -> Result<(u16, String), String> {
    let req = if insecure {
        INSECURE.get(url)
    } else {
        inboxora_net::safe_get(url, inboxora_net::Guard { allow_private: false, require_https: Some(true) })
            .map_err(|e| e.to_string())?
            .timeout(Duration::from_secs(10))
    };
    let res = req.send().await.map_err(|e| {
        if e.is_timeout() { "The operation was aborted due to timeout".to_owned() } else { "fetch failed".to_owned() }
    })?;
    let status = res.status().as_u16();
    let text = res.text().await.map_err(|_| "fetch failed".to_owned())?;
    Ok((status, text))
}

/// `getDiscovery(issuerUrl, allowInsecure)`: the validated discovery document (cached 5 min).
pub async fn discovery(issuer: &str, allow_insecure: bool) -> Result<Value, String> {
    let parsed = url::Url::parse(issuer).map_err(|_| "Invalid URL".to_owned())?;
    if !allow_insecure && parsed.scheme() != "https" {
        return Err("OIDC issuer URL must use HTTPS".into());
    }
    if !allow_insecure && let Some(e) = inboxora_net::validate_host(&host_of(&parsed), false).await {
        return Err(format!("OIDC issuer host rejected: {e}"));
    }
    let key = format!("{issuer}:{allow_insecure}");
    if let Some((doc, at)) = CACHE.lock().get(&key)
        && at.elapsed() < DISCOVERY_TTL
    {
        return Ok(doc.clone());
    }
    let well_known = format!("{}/.well-known/openid-configuration", issuer.strip_suffix('/').unwrap_or(issuer));
    let (status, body) = fetch_json(&well_known, allow_insecure).await?;
    if !(200..300).contains(&status) {
        if status == 401 || status == 403 {
            return Err(format!(
                "OIDC discovery blocked ({status}): the server cannot reach {well_known} — ensure the well-known endpoint is publicly accessible from the Inboxora server"
            ));
        }
        return Err(format!("OIDC discovery failed for {issuer}: {status}"));
    }
    let doc: Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let configured = issuer.strip_suffix('/').unwrap_or(issuer);
    let discovered = doc.get("issuer").and_then(Value::as_str).unwrap_or("");
    let discovered = discovered.strip_suffix('/').unwrap_or(discovered);
    if discovered != configured {
        return Err(format!("OIDC issuer mismatch: configured \"{configured}\", got \"{discovered}\""));
    }
    for field in ["authorization_endpoint", "token_endpoint", "jwks_uri"] {
        let Some(u) = doc.get(field).filter(|v| inboxora_core::json::truthy(Some(v))) else {
            return Err(format!("OIDC discovery missing required field: {field}"));
        };
        let text = crate::services::dav_xml::js_string(u);
        let Ok(endpoint) = url::Url::parse(&text) else {
            return Err(format!("OIDC discovery returned invalid URL for {field}: {text}"));
        };
        if !allow_insecure {
            if endpoint.scheme() != "https" {
                return Err(format!("OIDC discovery returned non-HTTPS {field}: {text}"));
            }
            if let Some(e) = inboxora_net::validate_host(&host_of(&endpoint), false).await {
                return Err(format!("OIDC discovery {field} points to a disallowed host: {e}"));
            }
        }
    }
    CACHE.lock().insert(key, (doc.clone(), Instant::now()));
    Ok(doc)
}

/// `buildEndSessionUrl({ providerId, idToken })`.
pub async fn end_session_url(db: &Db, provider_id: Option<&str>, id_token: Option<&str>) -> Option<String> {
    let provider_id = provider_id.filter(|p| !p.is_empty())?;
    let run = async {
        let row = db
            .opt(
                "SELECT issuer_url, client_id, allow_insecure, rp_initiated_logout FROM oidc_providers WHERE id = $1",
                params![provider_id],
            )
            .await
            .map_err(|e| e.to_string())?;
        let Some(row) = row else { return Ok(None) };
        if !row.get::<_, Option<bool>>("rp_initiated_logout").unwrap_or(false) {
            return Ok(None);
        }
        let issuer: String = row.get("issuer_url");
        let doc = discovery(&issuer, row.get::<_, Option<bool>>("allow_insecure").unwrap_or(false)).await?;
        let Some(endpoint) = doc.get("end_session_endpoint").filter(|v| inboxora_core::json::truthy(Some(v))) else {
            return Ok(None);
        };
        let mut params = url::form_urlencoded::Serializer::new(String::new());
        params.append_pair("client_id", &row.get::<_, String>("client_id"));
        if let Some(t) = id_token.filter(|t| !t.is_empty()) {
            params.append_pair("id_token_hint", t);
        }
        if let Some(app) = inboxora_core::config::env("APP_URL").filter(|a| !a.is_empty()) {
            params.append_pair("post_logout_redirect_uri", &format!("{app}/login"));
        }
        Ok::<_, String>(Some(format!("{}?{}", crate::services::dav_xml::js_string(endpoint), params.finish())))
    };
    match run.await {
        Ok(u) => u,
        Err(e) => {
            tracing::error!("buildEndSessionUrl failed: {e}");
            None
        }
    }
}

// ── tokens ───────────────────────────────────────────────────────────────────────────

/// POST a form to a token endpoint: (status, body text).
pub async fn token_request(endpoint: &str, form: &[(&str, &str)], insecure: bool) -> Result<(u16, String), String> {
    let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(form.iter()).finish();
    let req = if insecure {
        INSECURE.post(endpoint)
    } else {
        inboxora_net::safe_request(
            reqwest::Method::POST,
            endpoint,
            inboxora_net::Guard { allow_private: false, require_https: Some(true) },
        )
        .map_err(|e| e.to_string())?
        .timeout(Duration::from_secs(10))
    };
    let res = req
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                "The operation was aborted due to timeout".to_owned()
            } else {
                "fetch failed".to_owned()
            }
        })?;
    let status = res.status().as_u16();
    Ok((status, res.text().await.map_err(|_| "fetch failed".to_owned())?))
}

/// Remote JWKS per URI, refetched (at most every 30 s) when a token names an unknown key.
static JWKS: LazyLock<Mutex<HashMap<String, (jsonwebtoken::jwk::JwkSet, Instant)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

async fn fetch_jwks(uri: &str, insecure: bool) -> Result<jsonwebtoken::jwk::JwkSet, String> {
    let (status, body) = fetch_json(uri, insecure).await?;
    if !(200..300).contains(&status) {
        return Err("Expected 200 OK from the JSON Web Key Set HTTP response".into());
    }
    serde_json::from_str(&body).map_err(|_| "JSON Web Key Set malformed".to_owned())
}

/// `jwtVerify(idToken, createRemoteJWKSet(jwks_uri), { issuer, audience })`.
pub async fn verify_id_token(
    token: &str,
    jwks_uri: &str,
    issuer: Option<&str>,
    audience: &str,
    insecure: bool,
) -> Result<serde_json::Map<String, Value>, String> {
    let header = jsonwebtoken::decode_header(token).map_err(|e| e.to_string())?;
    let find = |set: &jsonwebtoken::jwk::JwkSet| -> Option<jsonwebtoken::jwk::Jwk> {
        set.keys
            .iter()
            .filter(|k| header.kid.is_none() || k.common.key_id == header.kid)
            .find(|k| jsonwebtoken::DecodingKey::from_jwk(k).is_ok())
            .cloned()
    };
    let cached = JWKS.lock().get(jwks_uri).cloned();
    let mut key = cached.as_ref().and_then(|(set, _)| find(set));
    if key.is_none() && cached.as_ref().is_none_or(|(_, at)| at.elapsed() >= Duration::from_secs(30)) {
        let set = fetch_jwks(jwks_uri, insecure).await?;
        key = find(&set);
        JWKS.lock().insert(jwks_uri.to_owned(), (set, Instant::now()));
    }
    let Some(jwk) = key else { return Err("no applicable key found in the JSON Web Key Set".into()) };
    let decoding = jsonwebtoken::DecodingKey::from_jwk(&jwk).map_err(|e| e.to_string())?;
    if matches!(
        header.alg,
        jsonwebtoken::Algorithm::HS256 | jsonwebtoken::Algorithm::HS384 | jsonwebtoken::Algorithm::HS512
    ) {
        return Err("unsupported \"alg\" (Algorithm) Header value".into());
    }
    let mut v = jsonwebtoken::Validation::new(header.alg);
    v.leeway = 0;
    v.validate_nbf = true;
    v.required_spec_claims.clear();
    if let Some(iss) = issuer {
        v.set_issuer(&[iss]);
    }
    v.set_audience(&[audience]);
    let data =
        jsonwebtoken::decode::<serde_json::Map<String, Value>>(token, &decoding, &v).map_err(|e| e.to_string())?;
    Ok(data.claims)
}

// ── OIDC_<N>_* environment providers (oidcEnvConfig.ts / oidcEnvSeed.ts) ──────────────

const DANGEROUS_CLAIMS: [&str; 3] = ["__proto__", "constructor", "prototype"];

/// `normalizeClaimName(value)`.
pub fn normalize_claim_name(v: &str) -> Option<String> {
    static RE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^[a-zA-Z0-9_.:-]{1,64}$").unwrap());
    let c = inboxora_mail::text::js_trim(v);
    (RE.is_match(c) && !DANGEROUS_CLAIMS.contains(&c)).then(|| c.to_owned())
}

/// `resolveClaimValue(claim, payload, email)`: a lower-cased, trimmed string claim.
pub fn resolve_claim_value(
    claim: Option<&str>,
    payload: &serde_json::Map<String, Value>,
    email: Option<&str>,
) -> Option<String> {
    let c = claim.map(inboxora_mail::text::js_trim).filter(|c| !c.is_empty()).unwrap_or("email");
    if c != "email" && DANGEROUS_CLAIMS.contains(&c) {
        return None;
    }
    let raw =
        if c == "email" { email.map(str::to_owned) } else { payload.get(c).and_then(Value::as_str).map(str::to_owned) };
    raw.map(|r| inboxora_mail::text::js_trim(&r).to_lowercase()).filter(|r| !r.is_empty())
}

#[derive(Debug, Clone)]
pub struct MailConfig {
    pub imap_host: String,
    pub imap_port: i64,
    pub imap_tls: bool,
    pub smtp_host: String,
    pub smtp_port: i64,
    pub smtp_tls: String,
    pub username_claim: String,
    pub skip_tls_verify: bool,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EnvProvider {
    pub index: usize,
    pub name: String,
    pub slug: String,
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub scopes: String,
    pub provisioning_mode: String,
    pub allowed_domains: Option<String>,
    pub enabled: bool,
    pub require_email_verified: bool,
    pub allow_insecure: bool,
    pub admin_group_claim: Option<String>,
    pub admin_group_value: Option<String>,
    pub rp_initiated_logout: bool,
    pub login_match_claim: String,
    pub mail: Option<MailConfig>,
}

fn env_bool(v: Option<String>, fallback: bool) -> bool {
    match v.as_deref() {
        None | Some("") => fallback,
        Some(v) => v == "true" || v == "1",
    }
}

fn env_int(v: Option<String>, fallback: i64) -> i64 {
    v.filter(|v| !v.is_empty())
        .map(|v| crate::services::dav_xml::js_number(inboxora_mail::text::js_trim(&v)))
        .filter(|n| n.fract() == 0.0 && n.is_finite())
        .map_or(fallback, |n| n as i64)
}

fn parse_env_provider(n: usize) -> Option<std::result::Result<EnvProvider, Vec<String>>> {
    let p = |k: &str| inboxora_core::config::env(&format!("OIDC_{n}_{k}"));
    let trimmed = |k: &str| p(k).map(|v| inboxora_mail::text::js_trim(&v).to_owned());
    if !["ISSUER_URL", "CLIENT_ID", "CLIENT_SECRET", "NAME", "SLUG"].iter().any(|k| p(k).is_some()) {
        return None;
    }
    let mut errors = Vec::new();
    let issuer = trimmed("ISSUER_URL").filter(|v| !v.is_empty());
    let client_id = trimmed("CLIENT_ID").filter(|v| !v.is_empty());
    let client_secret = p("CLIENT_SECRET").filter(|v| !v.is_empty());
    let allow_insecure = env_bool(p("ALLOW_INSECURE"), false);
    match &issuer {
        None => errors.push(format!("OIDC_{n}_ISSUER_URL is required")),
        Some(u) => match url::Url::parse(u) {
            Ok(parsed) if !allow_insecure && parsed.scheme() != "https" => {
                errors.push(format!("OIDC_{n}_ISSUER_URL must use HTTPS (or set OIDC_{n}_ALLOW_INSECURE=true)"))
            }
            Ok(_) => {}
            Err(_) => errors.push(format!("OIDC_{n}_ISSUER_URL is not a valid URL")),
        },
    }
    if client_id.is_none() {
        errors.push(format!("OIDC_{n}_CLIENT_ID is required"));
    }
    if client_secret.is_none() {
        errors.push(format!("OIDC_{n}_CLIENT_SECRET is required"));
    }
    let slug = trimmed("SLUG").filter(|s| !s.is_empty()).unwrap_or_else(|| format!("sso-{n}")).to_lowercase();
    if slug.is_empty() || !slug.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        errors.push(format!("OIDC_{n}_SLUG must contain only lowercase letters, numbers and hyphens"));
    }
    let mode = trimmed("PROVISIONING_MODE").filter(|m| !m.is_empty()).unwrap_or_else(|| "login_existing_only".into());
    if !matches!(mode.as_str(), "disabled" | "login_existing_only" | "open") {
        errors.push(format!("OIDC_{n}_PROVISIONING_MODE must be one of: disabled, login_existing_only, open"));
    }
    let claim = |key: &str, default: Option<&str>| -> (Option<String>, bool) {
        match trimmed(key).filter(|v| !v.is_empty()) {
            Some(raw) => {
                let c = normalize_claim_name(&raw);
                let bad = c.is_none();
                (c, bad)
            }
            None => (default.map(str::to_owned), false),
        }
    };
    let (login_claim, bad_login) = claim("LOGIN_MATCH_CLAIM", Some("email"));
    if bad_login {
        errors.push(format!("OIDC_{n}_LOGIN_MATCH_CLAIM is not a valid claim name"));
    }
    let (admin_claim, bad_admin) = claim("ADMIN_GROUP_CLAIM", None);
    if bad_admin {
        errors.push(format!("OIDC_{n}_ADMIN_GROUP_CLAIM is not a valid claim name"));
    }
    let mut mail = None;
    let mut mail_errors = Vec::new();
    let (imap, smtp) =
        (trimmed("MAIL_IMAP_HOST").filter(|v| !v.is_empty()), trimmed("MAIL_SMTP_HOST").filter(|v| !v.is_empty()));
    if imap.is_some() || smtp.is_some() {
        match (imap, smtp) {
            (Some(imap_host), Some(smtp_host)) => {
                let smtp_tls = trimmed("MAIL_SMTP_TLS").filter(|v| !v.is_empty()).unwrap_or_else(|| "STARTTLS".into());
                if !matches!(smtp_tls.as_str(), "STARTTLS" | "SSL" | "none") {
                    mail_errors.push(format!("OIDC_{n}_MAIL_SMTP_TLS must be one of: STARTTLS, SSL, none"));
                }
                let (user_claim, bad_user) = claim("MAIL_USERNAME_CLAIM", Some("email"));
                if bad_user {
                    mail_errors.push(format!("OIDC_{n}_MAIL_USERNAME_CLAIM is not a valid claim name"));
                }
                if mail_errors.is_empty() {
                    mail = Some(MailConfig {
                        imap_host,
                        imap_port: env_int(p("MAIL_IMAP_PORT"), 993),
                        imap_tls: env_bool(p("MAIL_IMAP_TLS"), true),
                        smtp_host,
                        smtp_port: env_int(p("MAIL_SMTP_PORT"), 587),
                        smtp_tls,
                        username_claim: user_claim.unwrap_or_else(|| "email".into()),
                        skip_tls_verify: env_bool(p("MAIL_SKIP_TLS_VERIFY"), false),
                        display_name: trimmed("MAIL_NAME").filter(|v| !v.is_empty()),
                    });
                }
            }
            _ => mail_errors.push(format!(
                "OIDC_{n}_MAIL_IMAP_HOST and OIDC_{n}_MAIL_SMTP_HOST must both be set to enable mailbox auto-provisioning for provider {n}"
            )),
        }
    }
    if !mail_errors.is_empty() {
        tracing::error!(
            "OIDC env provider OIDC_{n}_* mailbox auto-provisioning is misconfigured and will be disabled (SSO login is unaffected):\n  - {}",
            mail_errors.join("\n  - ")
        );
    }
    if !errors.is_empty() {
        return Some(Err(errors));
    }
    Some(Ok(EnvProvider {
        index: n,
        name: trimmed("NAME").filter(|v| !v.is_empty()).unwrap_or_else(|| format!("SSO {n}")),
        slug,
        issuer_url: issuer.unwrap_or_default(),
        client_id: client_id.unwrap_or_default(),
        client_secret: client_secret.unwrap_or_default(),
        scopes: trimmed("SCOPES").filter(|v| !v.is_empty()).unwrap_or_else(|| "openid email profile".into()),
        provisioning_mode: mode,
        allowed_domains: trimmed("ALLOWED_DOMAINS").filter(|v| !v.is_empty()),
        enabled: env_bool(p("ENABLED"), true),
        require_email_verified: env_bool(p("REQUIRE_EMAIL_VERIFIED"), true),
        allow_insecure,
        admin_group_value: if admin_claim.is_some() {
            trimmed("ADMIN_GROUP_VALUE").filter(|v| !v.is_empty())
        } else {
            None
        },
        admin_group_claim: admin_claim,
        rp_initiated_logout: env_bool(p("RP_INITIATED_LOGOUT"), false),
        login_match_claim: login_claim.unwrap_or_else(|| "email".into()),
        mail,
    }))
}

static ENV_PROVIDERS: LazyLock<Mutex<Option<Vec<EnvProvider>>>> = LazyLock::new(|| Mutex::new(None));

/// `getOidcEnvProviders()`.
pub fn env_providers() -> Vec<EnvProvider> {
    if let Some(p) = ENV_PROVIDERS.lock().clone() {
        return p;
    }
    let mut providers: Vec<EnvProvider> = Vec::new();
    for n in 1..=50 {
        match parse_env_provider(n) {
            None => {}
            Some(Err(errors)) => tracing::error!(
                "OIDC env provider OIDC_{n}_* is misconfigured and will be skipped:\n  - {}",
                errors.join("\n  - ")
            ),
            Some(Ok(p)) => providers.push(p),
        }
    }
    let mut seen: HashMap<String, usize> = HashMap::new();
    for p in &providers {
        if let Some(prev) = seen.get(&p.slug) {
            tracing::error!(
                "OIDC env providers OIDC_{prev}_* and OIDC_{}_* both resolve to slug \"{}\" — set distinct OIDC_<N>_SLUG values",
                p.index,
                p.slug
            );
        }
        seen.insert(p.slug.clone(), p.index);
    }
    *ENV_PROVIDERS.lock() = Some(providers.clone());
    providers
}

/// `getMailConfigForSlug(slug)`.
pub fn mail_config_for_slug(slug: &str) -> Option<MailConfig> {
    env_providers().into_iter().find(|p| p.slug == slug).and_then(|p| p.mail)
}

/// `seedOidcProvidersFromEnv()`: create env-configured providers that do not exist yet.
pub async fn seed_env_providers(db: &Db) {
    for p in env_providers() {
        let run = async {
            if !p.allow_insecure {
                let host = url::Url::parse(&p.issuer_url).map(|u| host_of(&u)).unwrap_or_default();
                if let Some(e) = inboxora_net::validate_host(&host, false).await {
                    tracing::error!("OIDC env provider OIDC_{}_* skipped: issuer host rejected: {e}", p.index);
                    return Ok(());
                }
            }
            if db
                .opt("SELECT id FROM oidc_providers WHERE slug = $1", params![&p.slug])
                .await
                .map_err(|e| e.to_string())?
                .is_some()
            {
                return Ok(());
            }
            db.exec(
                "INSERT INTO oidc_providers (name, slug, issuer_url, client_id, client_secret, scopes, provisioning_mode, allowed_domains, enabled, require_email_verified, allow_insecure, admin_group_claim, admin_group_value, rp_initiated_logout, login_match_claim) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
                params![
                    &p.name,
                    &p.slug,
                    &p.issuer_url,
                    &p.client_id,
                    inboxora_core::crypto::encrypt(&p.client_secret)?,
                    &p.scopes,
                    &p.provisioning_mode,
                    p.allowed_domains.clone(),
                    p.enabled,
                    p.require_email_verified,
                    p.allow_insecure,
                    p.admin_group_claim.clone(),
                    p.admin_group_value.clone(),
                    p.rp_initiated_logout,
                    &p.login_match_claim,
                ],
            )
            .await
            .map_err(|e| e.to_string())?;
            tracing::info!("Seeded OIDC provider \"{}\" (slug: {}) from OIDC_{}_* env vars", p.name, p.slug, p.index);
            Ok::<_, String>(())
        };
        if let Err(e) = run.await {
            tracing::error!("Failed to seed OIDC provider OIDC_{}_* (slug: {}): {e}", p.index, p.slug);
        }
    }
}

// ── mailbox auto-provisioning (oidcMailAuth.ts) ──────────────────────────────────────

fn token_expiry(expires_in: Option<&Value>) -> chrono::DateTime<chrono::Utc> {
    let secs = expires_in.and_then(Value::as_f64).filter(|s| s.is_finite() && *s > 0.0).unwrap_or(3600.0);
    chrono::Utc::now() + chrono::Duration::milliseconds((secs * 1000.0) as i64)
}

/// Who signed in through which provider, with what claims and tokens.
pub struct MailProvision<'a> {
    pub user_id: &'a str,
    pub provider_id: &'a str,
    pub provider_name: &'a str,
    pub slug: &'a str,
    pub payload: &'a serde_json::Map<String, Value>,
    pub email: Option<&'a str>,
    pub tokens: &'a serde_json::Map<String, Value>,
}

/// `provisionOidcMailAccount(...)`: create or refresh the user's OIDC-backed mailbox.
pub async fn provision_mail_account(db: &Db, m: MailProvision<'_>) -> Result<(), String> {
    let MailProvision { user_id, provider_id, provider_name, slug, payload, email, tokens } = m;
    let Some(mail) = mail_config_for_slug(slug) else { return Ok(()) };
    let Some(access) = tokens
        .get("access_token")
        .filter(|v| inboxora_core::json::truthy(Some(v)))
        .map(crate::services::dav_xml::js_string)
    else {
        return Ok(());
    };
    let Some(username) = resolve_claim_value(Some(&mail.username_claim), payload, email) else {
        tracing::warn!(
            "OIDC mail auto-provision: claim \"{}\" is missing from the id_token for user {user_id} — skipping mailbox provisioning",
            mail.username_claim
        );
        return Ok(());
    };
    let Some(email) = email.filter(|e| !e.is_empty()) else {
        tracing::warn!(
            "OIDC mail auto-provision: no verified email claim for user {user_id} — skipping mailbox provisioning"
        );
        return Ok(());
    };
    let refresh = tokens
        .get("refresh_token")
        .filter(|v| inboxora_core::json::truthy(Some(v)))
        .map(crate::services::dav_xml::js_string);
    let enc = inboxora_core::crypto::encrypt;
    db.exec(
        "INSERT INTO email_accounts (user_id, name, email_address, color, protocol, imap_host, imap_port, imap_tls, imap_skip_tls_verify, \
           smtp_host, smtp_port, smtp_tls, auth_user, oauth_provider, oauth_access_token, oauth_refresh_token, oauth_token_expiry, oidc_provider_id) \
         VALUES ($1,$2,$3,$4,'imap',$5,$6,$7,$8,$9,$10,$11,$12,'oidc',$13,$14,$15,$16) \
         ON CONFLICT (user_id, oidc_provider_id) WHERE oidc_provider_id IS NOT NULL DO UPDATE SET \
           auth_user = excluded.auth_user, email_address = excluded.email_address, \
           imap_host = excluded.imap_host, imap_port = excluded.imap_port, imap_tls = excluded.imap_tls, \
           imap_skip_tls_verify = excluded.imap_skip_tls_verify, \
           smtp_host = excluded.smtp_host, smtp_port = excluded.smtp_port, smtp_tls = excluded.smtp_tls, \
           oauth_access_token = excluded.oauth_access_token, \
           oauth_refresh_token = COALESCE(excluded.oauth_refresh_token, email_accounts.oauth_refresh_token), \
           oauth_token_expiry = excluded.oauth_token_expiry, sync_error = NULL, reauth_required = false",
        params![
            user_id,
            mail.display_name.clone().unwrap_or_else(|| provider_name.to_owned()),
            email,
            "#0ea5e9",
            &mail.imap_host,
            mail.imap_port,
            mail.imap_tls,
            mail.skip_tls_verify,
            &mail.smtp_host,
            mail.smtp_port,
            &mail.smtp_tls,
            username,
            enc(&access)?,
            refresh.map(|r| enc(&r)).transpose()?,
            token_expiry(tokens.get("expires_in")),
            provider_id,
        ],
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// `refreshOidcAccountToken`: refresh an OIDC-linked mailbox's access token. Returns the
/// account row with the plaintext token and new expiry.
pub async fn refresh_mail_account_token(
    db: &Db,
    mut account: Map<String, Value>,
) -> Result<Map<String, Value>, RefreshError> {
    let s = |k: &str| account.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_owned);
    let Some(provider_id) = s("oidc_provider_id") else {
        return Err(RefreshError::reauth(
            "This mailbox is not linked to an OIDC provider — please log in again to reconnect it",
        ));
    };
    let row = db
        .opt(
            "SELECT id, issuer_url, client_id, client_secret, allow_insecure FROM oidc_providers WHERE id = $1",
            params![&provider_id],
        )
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| RefreshError::reauth("The OIDC provider for this mailbox no longer exists"))?;
    let refresh =
        s("oauth_refresh_token").as_deref().and_then(crypto::decrypt).filter(|t| !t.is_empty()).ok_or_else(|| {
            RefreshError::reauth(
                "OIDC refresh token is missing or corrupted — please log in again to reconnect this mailbox",
            )
        })?;
    let secret = row
        .get::<_, Option<String>>("client_secret")
        .as_deref()
        .and_then(crypto::decrypt)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| "OIDC client secret decryption failed".to_owned())?;
    let insecure = row.get::<_, Option<bool>>("allow_insecure").unwrap_or(false);
    let issuer: String = row.get("issuer_url");
    let client_id: String = row.get("client_id");
    let doc = discovery(&issuer, insecure).await?;
    let endpoint = doc
        .get("token_endpoint")
        .and_then(Value::as_str)
        .ok_or_else(|| "OIDC discovery missing required field: token_endpoint".to_owned())?
        .to_owned();
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh.as_str()),
        ("client_id", client_id.as_str()),
        ("client_secret", secret.as_str()),
    ];
    let (status, body) = token_request(&endpoint, &form, insecure).await?;
    let tokens: Map<String, Value> = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let text = |k: &str| tokens.get(k).and_then(Value::as_str).filter(|v| !v.is_empty());
    if !(200..300).contains(&status) {
        return Err(RefreshError::from_token_error(
            text("error"),
            text("error_description").or_else(|| text("error")).unwrap_or("OIDC token refresh failed"),
        ));
    }
    let secs = tokens.get("expires_in").and_then(Value::as_f64).filter(|v| v.is_finite() && *v > 0.0).unwrap_or(3600.0);
    let expiry = chrono::Utc::now() + chrono::Duration::milliseconds((secs * 1000.0) as i64);
    let access = text("access_token").map(str::to_owned);
    let enc = |v: &str| crypto::encrypt(v).map_err(|e| e.to_string());
    db.exec(
        "UPDATE email_accounts SET oauth_access_token = $1, oauth_refresh_token = COALESCE($2, oauth_refresh_token), oauth_token_expiry = $3 WHERE id = $4",
        params![
            access.as_deref().map(enc).transpose()?,
            text("refresh_token").map(enc).transpose()?,
            expiry,
            account.get("id").cloned().unwrap_or(Value::Null)
        ],
    )
    .await
    .map_err(|e| e.to_string())?;
    account.insert("oauth_access_token".into(), access.map_or(Value::Null, Value::String));
    account.insert("oauth_token_expiry".into(), Value::String(inboxora_core::json::js_iso(&expiry)));
    Ok(account)
}
