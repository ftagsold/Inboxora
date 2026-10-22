//! `encryptExistingCredentials()`: encrypt credentials stored before at-rest encryption existed.

use inboxora_core::pg::{Db, Q, Val};
use inboxora_core::{Result, crypto, params};

fn needs(v: &Option<String>) -> bool {
    v.as_deref().is_some_and(|v| !v.is_empty() && !v.starts_with("enc:v1:"))
}

pub async fn encrypt_existing(db: &Db) -> Result<()> {
    if inboxora_core::config::env("ENCRYPTION_KEY").is_none() {
        tracing::warn!(
            "ENCRYPTION_KEY not set — stored credentials are NOT encrypted. Set ENCRYPTION_KEY in .env to enable at-rest encryption."
        );
        return Ok(());
    }
    let enc = |v: &str| crypto::encrypt(v).map_err(inboxora_core::Error::internal);
    let rows = db
        .rows(
            "SELECT id, auth_pass, oauth_access_token, oauth_refresh_token FROM email_accounts \
             WHERE (auth_pass IS NOT NULL AND auth_pass NOT LIKE 'enc:v1:%') \
                OR (oauth_access_token IS NOT NULL AND oauth_access_token NOT LIKE 'enc:v1:%') \
                OR (oauth_refresh_token IS NOT NULL AND oauth_refresh_token NOT LIKE 'enc:v1:%')",
            params![],
        )
        .await?;
    let mut count = 0;
    for row in &rows {
        let mut sets = Vec::new();
        let mut values: Vec<Val> = Vec::new();
        for col in ["auth_pass", "oauth_access_token", "oauth_refresh_token"] {
            let v: Option<String> = row.get(col);
            if needs(&v) {
                values.push(Val::from(enc(&v.unwrap_or_default())?));
                sets.push(format!("{col} = ${}", values.len()));
            }
        }
        if !sets.is_empty() {
            values.push(Val::from(row.get::<_, uuid::Uuid>("id")));
            db.exec(&format!("UPDATE email_accounts SET {} WHERE id = ${}", sets.join(", "), values.len()), &values)
                .await?;
            count += 1;
        }
    }
    if count > 0 {
        tracing::info!("Encrypted credentials for {count} account(s)");
    }
    let rows = db.rows("SELECT id, client_secret FROM oidc_providers WHERE client_secret IS NOT NULL AND client_secret NOT LIKE 'enc:v1:%'", params![]).await?;
    let mut oidc = 0;
    for row in &rows {
        let secret: Option<String> = row.get("client_secret");
        if needs(&secret) {
            db.exec(
                "UPDATE oidc_providers SET client_secret = $1 WHERE id = $2",
                params![enc(&secret.unwrap_or_default())?, row.get::<_, uuid::Uuid>("id")],
            )
            .await?;
            oidc += 1;
        }
    }
    if oidc > 0 {
        tracing::info!("Encrypted client secrets for {oidc} OIDC provider(s)");
    }
    let rows = db
        .rows(
            "SELECT id, url, last_error FROM calendar_import_sources WHERE url IS NOT NULL AND url NOT LIKE 'enc:v1:%'",
            params![],
        )
        .await?;
    let mut sources = 0;
    for row in &rows {
        let url: Option<String> = row.get("url");
        let Some(url) = url.filter(|u| !u.is_empty() && !u.starts_with("enc:v1:")) else { continue };
        let error: Option<String> = row.get::<_, Option<String>>("last_error").map(|e| e.replace(&url, "[redacted]"));
        let n = db
            .exec(
                "UPDATE calendar_import_sources SET url = $1, last_error = $2, updated_at = NOW() WHERE id = $3 AND url = $4 AND url_fingerprint IS NOT NULL",
                params![enc(&url)?, error, row.get::<_, uuid::Uuid>("id"), &url],
            )
            .await?;
        if n > 0 {
            sources += 1;
        }
    }
    if sources > 0 {
        tracing::info!("Encrypted URLs for {sources} calendar source(s)");
    }
    Ok(())
}
