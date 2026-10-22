//! Building outgoing MIME messages (nodemailer's `sendMail` options → RFC 5322 bytes) and
//! per-account SMTP transports (`createAccountSmtpTransport`).

use inboxora_core::crypto;
use inboxora_core::pg::Db;
use lettre::Message;
use lettre::message::header::{ContentType, HeaderName, HeaderValue};
use lettre::message::{Attachment, Mailbox, Mailboxes, MultiPart, SinglePart};
use serde_json::{Map, Value};

use super::smtp::{Auth, Transport, resolve};

#[derive(Debug, Clone, Default)]
pub struct Part {
    pub filename: Option<String>,
    pub content: Vec<u8>,
    pub content_type: String,
    /// Content-ID for inline (cid:) parts.
    pub cid: Option<String>,
}

/// A calendar part sent as `text/calendar; method=…` alongside the body (nodemailer `icalEvent`).
#[derive(Debug, Clone, Default)]
pub struct IcalEvent {
    pub method: String,
    pub content: String,
    pub filename: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Compose {
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub reply_to: Vec<String>,
    pub subject: String,
    pub text: Option<String>,
    pub html: Option<String>,
    pub attachments: Vec<Part>,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
    pub date: Option<chrono::DateTime<chrono::Utc>>,
    pub headers: Vec<(String, String)>,
    pub ical: Option<IcalEvent>,
}

#[derive(Debug, Clone)]
pub struct Built {
    pub raw: Vec<u8>,
    pub message_id: String,
    pub envelope_from: String,
    pub recipients: Vec<String>,
}

fn mailboxes(list: &[String]) -> Result<Vec<Mailbox>, String> {
    list.iter()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().parse::<Mailbox>().map_err(|e| format!("Invalid address \"{s}\": {e}")))
        .collect()
}

/// A fresh `<uuid@domain>` Message-ID like nodemailer's.
pub fn new_message_id(from_email: &str) -> String {
    let domain = from_email.rsplit_once('@').map_or("localhost", |(_, d)| d.trim_end_matches('>'));
    format!("<{}@{domain}>", uuid::Uuid::new_v4())
}

fn single(ct: &str, body: Vec<u8>) -> Result<SinglePart, String> {
    let ct = ContentType::parse(ct).map_err(|e| e.to_string())?;
    Ok(SinglePart::builder().header(ct).body(body))
}

impl Compose {
    pub fn build(&self) -> Result<Built, String> {
        let from: Mailbox = self.from.trim().parse().map_err(|e| format!("Invalid from address: {e}"))?;
        let mut b = Message::builder().from(from.clone()).subject(self.subject.clone());
        let (to, cc, bcc, reply) =
            (mailboxes(&self.to)?, mailboxes(&self.cc)?, mailboxes(&self.bcc)?, mailboxes(&self.reply_to)?);
        for m in &to {
            b = b.to(m.clone());
        }
        for m in &cc {
            b = b.cc(m.clone());
        }
        for m in &bcc {
            b = b.bcc(m.clone());
        }
        for m in &reply {
            b = b.reply_to(m.clone());
        }
        let message_id = self.message_id.clone().unwrap_or_else(|| new_message_id(&from.email.to_string()));
        b = b.message_id(Some(message_id.clone()));
        if let Some(v) = self.in_reply_to.clone().filter(|s| !s.is_empty()) {
            b = b.in_reply_to(v);
        }
        if let Some(v) = self.references.clone().filter(|s| !s.is_empty()) {
            b = b.references(v);
        }
        if let Some(d) = self.date {
            b = b.date(std::time::SystemTime::from(d));
        }
        for (k, v) in &self.headers {
            let name = HeaderName::new_from_ascii(k.clone()).map_err(|e| e.to_string())?;
            b = b.raw_header(HeaderValue::new(name, v.clone()));
        }
        // Body: text and/or HTML alternative (plus the calendar alternative), wrapped in
        // related (inline images) and mixed (attachments) as needed.
        let mut alternatives: Vec<SinglePart> = Vec::new();
        if let Some(t) = &self.text {
            alternatives.push(single("text/plain; charset=utf-8", t.clone().into_bytes())?);
        }
        if let Some(h) = &self.html {
            alternatives.push(single("text/html; charset=utf-8", h.clone().into_bytes())?);
        }
        if let Some(ical) = &self.ical {
            alternatives.push(single(
                &format!("text/calendar; charset=utf-8; method={}", ical.method),
                ical.content.clone().into_bytes(),
            )?);
        }
        if alternatives.is_empty() {
            alternatives.push(single("text/plain; charset=utf-8", Vec::new())?);
        }
        let (inline, attached): (Vec<&Part>, Vec<&Part>) = self.attachments.iter().partition(|p| p.cid.is_some());
        let mut ical_attachment: Option<SinglePart> = None;
        if let Some(ical) = &self.ical {
            let name = ical.filename.clone().unwrap_or_else(|| "invite.ics".into());
            let ct = ContentType::parse(&format!("application/ics; name=\"{name}\"")).map_err(|e| e.to_string())?;
            ical_attachment = Some(Attachment::new(name).body(ical.content.clone().into_bytes(), ct));
        }
        let simple = alternatives.len() == 1 && inline.is_empty() && attached.is_empty() && ical_attachment.is_none();
        let message = if simple {
            b.singlepart(alternatives.remove(0)).map_err(|e| e.to_string())?
        } else {
            let alt = if alternatives.len() == 1 {
                MultiPart::mixed().singlepart(alternatives.remove(0))
            } else {
                alternatives.into_iter().fold(MultiPart::alternative().build(), |m, p| m.singlepart(p))
            };
            let body = if inline.is_empty() {
                alt
            } else {
                let mut related = MultiPart::related().multipart(alt);
                for p in inline {
                    let ct = ContentType::parse(&p.content_type)
                        .unwrap_or(ContentType::parse("application/octet-stream").expect("ct"));
                    let cid = p.cid.clone().unwrap_or_default();
                    related = related.singlepart(Attachment::new_inline(cid).body(p.content.clone(), ct));
                }
                related
            };
            if attached.is_empty() && ical_attachment.is_none() {
                b.multipart(body).map_err(|e| e.to_string())?
            } else {
                let mut mixed = MultiPart::mixed().multipart(body);
                for p in attached {
                    let ct = ContentType::parse(&p.content_type)
                        .unwrap_or(ContentType::parse("application/octet-stream").expect("ct"));
                    mixed = mixed.singlepart(
                        Attachment::new(p.filename.clone().unwrap_or_else(|| "attachment".into()))
                            .body(p.content.clone(), ct),
                    );
                }
                if let Some(a) = ical_attachment {
                    mixed = mixed.singlepart(a);
                }
                b.multipart(mixed).map_err(|e| e.to_string())?
            }
        };
        let recipients: Vec<String> = to.iter().chain(&cc).chain(&bcc).map(|m| m.email.to_string()).collect();
        Ok(Built { raw: message.formatted(), message_id, envelope_from: from.email.to_string(), recipients })
    }
}

/// Parse a comma/semicolon separated address list (`a@x, "B" <b@y>`).
pub fn split_addresses(v: &str) -> Vec<String> {
    v.parse::<Mailboxes>()
        .map(|m| m.into_iter().map(|b| b.to_string()).collect())
        .unwrap_or_else(|_| v.split([',', ';']).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect())
}

/// `createAccountSmtpTransport`: the account's SMTP transport (refreshing OAuth first), or
/// `(status, error)`.
pub async fn account_transport(
    db: &Db,
    account: Map<String, Value>,
) -> Result<(Transport, Map<String, Value>), (u16, String)> {
    let provider = account.get("oauth_provider").and_then(Value::as_str).unwrap_or("").to_owned();
    let mut account = account;
    if provider == "microsoft" || provider == "oidc" {
        let expiry = account
            .get("oauth_token_expiry")
            .and_then(Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map_or(0, |d| d.timestamp_millis());
        if account.get("id").and_then(Value::as_str).is_some()
            && expiry - chrono::Utc::now().timestamp_millis() < 5 * 60 * 1000
        {
            let refreshed = if provider == "microsoft" {
                crate::services::oauth::refresh_microsoft_token(db, account.clone()).await
            } else {
                crate::services::oidc::refresh_mail_account_token(db, account.clone()).await
            };
            account = refreshed.map_err(|e| (500, e))?;
        }
    }
    let s = |k: &str| account.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_owned);
    let auth = if matches!(provider.as_str(), "microsoft" | "google" | "oidc") && s("oauth_access_token").is_some() {
        let Some(token) = s("oauth_access_token").as_deref().and_then(crypto::decrypt).filter(|t| !t.is_empty()) else {
            return Err((502, "OAuth access token is corrupted — please reconnect your account.".into()));
        };
        Auth::OAuth2 { user: s("auth_user").or_else(|| s("email_address")).unwrap_or_default(), token }
    } else {
        let Some(pass) = s("smtp_auth_pass")
            .or_else(|| s("auth_pass"))
            .as_deref()
            .and_then(crypto::decrypt)
            .filter(|p| !p.is_empty())
        else {
            return Err((
                502,
                "SMTP password is corrupted or missing — please re-enter your account password in Settings.".into(),
            ));
        };
        Auth::Password { user: s("smtp_auth_user").or_else(|| s("auth_user")).unwrap_or_default(), pass }
    };
    let policy = crate::services::dav_client::connection_policy(db).await.map_err(|e| (500, e.to_string()))?;
    let resolved =
        resolve(&s("smtp_host").unwrap_or_default(), policy.allow_private_hosts).await.map_err(|e| (400, e))?;
    let tls = s("smtp_tls");
    let plain = !matches!(tls.as_deref(), Some("STARTTLS" | "SSL"));
    if !policy.allow_insecure_tls && plain {
        return Err((403, "Plain-text SMTP is not allowed: admin must enable \"Allow insecure TLS\"".into()));
    }
    let port = account.get("smtp_port").and_then(Value::as_u64).unwrap_or(587) as u16;
    let skip = account.get("imap_skip_tls_verify").and_then(Value::as_bool).unwrap_or(false);
    let transport = Transport {
        resolved,
        port,
        secure: tls.as_deref() == Some("SSL") || (tls.as_deref() != Some("none") && port == 465),
        require_tls: tls.as_deref() == Some("STARTTLS"),
        ignore_tls: tls.as_deref() == Some("none"),
        reject_unauthorized: !(policy.allow_insecure_tls && skip),
        auth,
    };
    Ok((transport, account))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_alternative_with_attachment() {
        let c = Compose {
            from: "Alice <alice@example.test>".into(),
            to: vec!["bob@example.test".into()],
            subject: "Grüße".into(),
            text: Some("hi".into()),
            html: Some("<p>hi</p>".into()),
            attachments: vec![Part {
                filename: Some("a.txt".into()),
                content: b"x".to_vec(),
                content_type: "text/plain".into(),
                cid: None,
            }],
            in_reply_to: Some("<p@x>".into()),
            ..Default::default()
        };
        let built = c.build().unwrap();
        let raw = String::from_utf8_lossy(&built.raw);
        assert!(raw.contains("multipart/mixed"));
        assert!(raw.contains("multipart/alternative"));
        assert!(raw.contains("In-Reply-To: <p@x>"));
        assert!(built.message_id.ends_with("@example.test>"));
        assert_eq!(built.recipients, vec!["bob@example.test"]);
    }
}
