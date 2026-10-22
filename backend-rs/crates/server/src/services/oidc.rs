//! OpenID Connect provider discovery (port of `oidcDiscovery.ts`) and RP-initiated logout
//! (`buildEndSessionUrl`).

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use inboxora_core::params;
use inboxora_core::pg::{Db, Q};
use inboxora_net::reqwest;
use parking_lot::Mutex;
use serde_json::Value;

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
