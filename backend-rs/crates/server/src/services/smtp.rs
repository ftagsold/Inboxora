//! SMTP delivery (port of `smtpTransport.ts` and `mailer.ts`).
//!
//! Connections go only to addresses validated by [`inboxora_net::resolve_for_connection`]; TLS
//! still authenticates the original hostname. When a connection attempt fails before anything
//! was transmitted (TCP connect, TLS, greeting, EHLO), the next validated address is tried
//! within a 45 s budget; any later failure is final, because the message may already have
//! been accepted and a retry could deliver it twice.

use std::net::IpAddr;
use std::time::{Duration, Instant};

use inboxora_core::pg::{Db, Q};
use inboxora_core::{crypto, params};
use lettre::message::{Mailbox, MultiPart, SinglePart, header::ContentType};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::{AsyncSmtpConnection, TlsParameters};
use lettre::transport::smtp::extension::ClientId;
use lettre::{Address, Message};

const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);
const FAILOVER_BUDGET: Duration = Duration::from_secs(45);
/// nodemailer's default socket timeout for the rest of the session.
const SESSION_TIMEOUT: Duration = Duration::from_secs(600);

/// Where to connect (`ResolvedConnectionInfo`).
#[derive(Debug, Clone)]
pub struct Resolved {
    pub host: String,
    pub servername: Option<String>,
    pub addresses: Vec<IpAddr>,
}

/// `resolveForConnection(host, { allowPrivate })`.
pub async fn resolve(host: &str, allow_private: bool) -> Result<Resolved, String> {
    let addrs = inboxora_net::resolve_for_connection(host, allow_private).await?;
    let h = host.trim().to_owned();
    Ok(match addrs {
        Some(list) => Resolved { host: list[0].to_string(), servername: Some(h), addresses: list },
        None => Resolved { host: h, servername: None, addresses: Vec::new() },
    })
}

#[derive(Debug, Clone)]
pub enum Auth {
    None,
    Password { user: String, pass: String },
    OAuth2 { user: String, token: String },
}

#[derive(Debug, Clone)]
pub struct Transport {
    pub resolved: Resolved,
    pub port: u16,
    /// Implicit TLS (`secure: true`).
    pub secure: bool,
    pub require_tls: bool,
    pub ignore_tls: bool,
    pub reject_unauthorized: bool,
    pub auth: Auth,
}

/// A delivery failure, with nodemailer's distinction of pre-delivery connection errors.
#[derive(Debug, Clone)]
pub struct SmtpError {
    pub message: String,
    pub connection: bool,
}

impl std::fmt::Display for SmtpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

fn err(message: impl Into<String>, connection: bool) -> SmtpError {
    SmtpError { message: message.into(), connection }
}

impl Transport {
    fn tls(&self) -> Result<TlsParameters, SmtpError> {
        let domain = self.resolved.servername.clone().unwrap_or_else(|| self.resolved.host.clone());
        TlsParameters::builder(domain)
            .dangerous_accept_invalid_certs(!self.reject_unauthorized)
            .build_rustls()
            .map_err(|e| err(e.to_string(), true))
    }

    /// Connect, negotiate TLS and authenticate against one address.
    async fn session(&self, host: &str, budget: Duration) -> Result<AsyncSmtpConnection, SmtpError> {
        let hello = ClientId::default();
        let implicit = if self.secure { Some(self.tls()?) } else { None };
        let connect = AsyncSmtpConnection::connect_tokio1((host, self.port), Some(budget), &hello, implicit, None);
        let mut conn = match tokio::time::timeout(budget, connect).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => return Err(err(e.to_string(), true)),
            Err(_) => return Err(err("Connection timeout", true)),
        };
        let setup = async {
            if !self.secure && !self.ignore_tls {
                if conn.can_starttls() {
                    conn.starttls(self.tls()?, &hello).await.map_err(|e| err(e.to_string(), false))?;
                } else if self.require_tls {
                    return Err(err("Server does not support STARTTLS", false));
                }
            }
            match &self.auth {
                Auth::None => {}
                Auth::Password { user, pass } => {
                    conn.auth(&[Mechanism::Plain, Mechanism::Login], &Credentials::new(user.clone(), pass.clone()))
                        .await
                        .map_err(|e| err(format!("Invalid login: {e}"), false))?;
                }
                Auth::OAuth2 { user, token } => {
                    conn.auth(&[Mechanism::Xoauth2], &Credentials::new(user.clone(), token.clone()))
                        .await
                        .map_err(|e| err(format!("Invalid login: {e}"), false))?;
                }
            }
            Ok(())
        };
        match tokio::time::timeout(SESSION_TIMEOUT, setup).await {
            Ok(Ok(())) => Ok(conn),
            Ok(Err(e)) => {
                conn.abort().await;
                Err(e)
            }
            Err(_) => Err(err("Timeout", false)),
        }
    }

    /// `runWithAddressFallback`: run `op` on a fresh session, failing over between addresses
    /// only while no data has been sent.
    async fn with_session(&self, op: Op<'_>) -> Result<String, SmtpError> {
        let mut candidates: Vec<String> = self.resolved.addresses.iter().map(IpAddr::to_string).collect();
        candidates.dedup();
        if candidates.is_empty() {
            candidates.push(self.resolved.host.clone());
        }
        let started = Instant::now();
        let mut last: Option<SmtpError> = None;
        for (i, host) in candidates.iter().enumerate() {
            let remaining = FAILOVER_BUDGET.saturating_sub(started.elapsed());
            if remaining < Duration::from_secs(2)
                && let Some(e) = last
            {
                return Err(e);
            }
            let attempt = ATTEMPT_TIMEOUT.min(remaining / 2).max(Duration::from_secs(1));
            match self.session(host, attempt).await {
                Ok(mut conn) => {
                    let run = async {
                        match op {
                            Op::Verify => Ok(String::new()),
                            Op::Send(envelope, message) => {
                                let r = conn.send(envelope, message).await.map_err(|e| err(e.to_string(), false))?;
                                Ok(r.message().collect::<Vec<_>>().join(" "))
                            }
                        }
                    };
                    let result =
                        tokio::time::timeout(SESSION_TIMEOUT, run).await.unwrap_or_else(|_| Err(err("Timeout", false)));
                    let _ = conn.quit().await;
                    return result;
                }
                Err(e) if e.connection && i + 1 < candidates.len() => {
                    tracing::warn!("SMTP connection failed; retrying another validated address: {}", e.message);
                    last = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| err("No SMTP address available", true)))
    }

    /// `transport.sendMail(...)` for a fully formatted message; returns the server's reply.
    pub async fn send_raw(&self, from: &str, to: &[String], message: &[u8]) -> Result<String, SmtpError> {
        let parse = |a: &str| a.parse::<Address>().map_err(|e| err(format!("Invalid address {a}: {e}"), false));
        let from = parse(from)?;
        let to: Vec<Address> = to.iter().map(|a| parse(a)).collect::<Result<_, _>>()?;
        let envelope = lettre::address::Envelope::new(Some(from), to).map_err(|e| err(e.to_string(), false))?;
        self.with_session(Op::Send(&envelope, message)).await
    }

    /// `transport.verify()`: connect and authenticate.
    pub async fn verify(&self) -> Result<(), SmtpError> {
        self.with_session(Op::Verify).await.map(|_| ())
    }
}

enum Op<'a> {
    Verify,
    Send(&'a lettre::address::Envelope, &'a [u8]),
}

/// A simple text/HTML message (`sendMail({ from, to, subject, text, html })`).
pub fn simple_message(
    from_name: &str,
    from: &str,
    to: &str,
    subject: &str,
    text: &str,
    html: Option<&str>,
) -> Result<Vec<u8>, SmtpError> {
    let from_addr: Address = from.parse().map_err(|e| err(format!("Invalid from address: {e}"), false))?;
    let to_addr: Address = to.parse().map_err(|e| err(format!("Invalid recipient: {e}"), false))?;
    let builder = Message::builder()
        .from(Mailbox::new(Some(from_name.to_owned()), from_addr))
        .to(Mailbox::new(None, to_addr))
        .subject(subject);
    let message = match html {
        Some(h) => builder.multipart(
            MultiPart::alternative()
                .singlepart(SinglePart::builder().header(ContentType::TEXT_PLAIN).body(text.to_owned()))
                .singlepart(SinglePart::builder().header(ContentType::TEXT_HTML).body(h.to_owned())),
        ),
        None => builder.header(ContentType::TEXT_PLAIN).body(text.to_owned()),
    }
    .map_err(|e| err(e.to_string(), false))?;
    Ok(message.formatted())
}

/// The admin-configured system mailer (`system_email_config`): transport and sender.
pub async fn system_transport(db: &Db) -> Result<(Transport, String, String), String> {
    let row = db
        .opt("SELECT value FROM system_settings WHERE key = 'system_email_config'", params![])
        .await
        .map_err(|e| e.to_string())?;
    let Some(row) = row else { return Err("System email is not configured".into()) };
    let cfg: serde_json::Value = serde_json::from_str(&row.get::<_, String>("value")).map_err(|e| e.to_string())?;
    let text =
        |k: &str| cfg.get(k).filter(|v| inboxora_core::json::truthy(Some(v))).map(crate::services::dav_xml::js_string);
    let pass = text("pass").and_then(|p| crypto::decrypt(&p)).filter(|p| !p.is_empty());
    let (Some(host), Some(user), Some(pass)) = (text("host"), text("user"), pass) else {
        return Err("System email is not configured".into());
    };
    let policy = crate::services::dav_client::connection_policy(db).await.map_err(|e| e.to_string())?;
    let resolved = resolve(&host, policy.allow_private_hosts).await?;
    let port = cfg.get("port").and_then(serde_json::Value::as_u64).filter(|p| *p > 0).map_or(587, |p| p as u16);
    let transport = Transport {
        resolved,
        port,
        secure: port == 465,
        require_tls: false,
        ignore_tls: false,
        reject_unauthorized: true,
        auth: Auth::Password { user: user.clone(), pass },
    };
    let from_name = text("fromName").unwrap_or_else(|| "Inboxora".into());
    let from_email = text("fromEmail").unwrap_or(user);
    Ok((transport, from_name, from_email))
}

/// `sendSystemEmail({ to, subject, text, html })`.
pub async fn send_system_email(db: &Db, to: &str, subject: &str, text: &str, html: Option<&str>) -> Result<(), String> {
    let (transport, from_name, from_email) = system_transport(db).await?;
    let message = simple_message(&from_name, &from_email, to, subject, text, html).map_err(|e| e.message)?;
    transport.send_raw(&from_email, &[to.to_owned()], &message).await.map_err(|e| e.message)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn literal_hosts_are_not_resolved() {
        let r = resolve("203.0.113.7", false).await.unwrap();
        assert_eq!((r.host.as_str(), r.servername.as_deref(), r.addresses.len()), ("203.0.113.7", None, 0));
        assert!(resolve("10.0.0.1", false).await.is_err());
    }

    #[tokio::test]
    async fn failover_tries_every_validated_address_on_connection_errors() {
        // Two closed local ports: both attempts fail at connect, and the error is a connection error.
        let t = Transport {
            resolved: Resolved {
                host: "x".into(),
                servername: Some("x".into()),
                addresses: vec!["127.0.0.1".parse().unwrap(), "::1".parse().unwrap()],
            },
            port: 9,
            secure: false,
            require_tls: false,
            ignore_tls: true,
            reject_unauthorized: true,
            auth: Auth::None,
        };
        let e = t.verify().await.unwrap_err();
        assert!(e.connection, "{e}");
    }

    #[test]
    fn simple_messages_are_multipart_alternative() {
        let m = String::from_utf8(
            simple_message("Inboxora", "a@x.test", "b@y.test", "Hi", "text", Some("<b>html</b>")).unwrap(),
        )
        .unwrap();
        assert!(m.contains("multipart/alternative"), "{m}");
        assert!(m.contains("From: Inboxora <a@x.test>"), "{m}");
    }
}
