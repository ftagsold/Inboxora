//! Optional HTTPS listener (`TLS_PORT`) next to the plain HTTP one, for deployments without a
//! TLS-terminating proxy in front.
//!
//! The certificate is `TLS_CERT_FILE` / `TLS_KEY_FILE` (default `certs/cert.pem` and
//! `certs/key.pem`). When neither exists a self-signed pair is generated and written there (kept
//! in memory if the directory is not writable), so a fresh install answers HTTPS at once.
//!
//! This listener is the edge: client-supplied X-Forwarded-For / X-Real-IP are dropped (the socket
//! peer is the client) and X-Forwarded-Proto is set to `https`, so sessions issue Secure cookies.
//! Every response carries HSTS.
//!
//! HTTP/2 and HTTP/1.1 are offered through ALPN. WebSockets stay on HTTP/1.1: extended CONNECT is
//! not enabled, so browsers open a separate HTTP/1.1 connection for `/ws` and the ntfy sockets.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderName, HeaderValue, Request, header};
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use rustls::ServerConfig;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tower::Service;

pub struct Tls {
    port: u16,
    config: Arc<ServerConfig>,
}

impl Tls {
    /// `None` when `TLS_PORT` is unset.
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        let env = inboxora_core::config::env;
        let Some(port) = env("TLS_PORT") else { return Ok(None) };
        let port = port.parse().map_err(|_| anyhow::anyhow!("TLS_PORT must be a port number, got {port}"))?;
        let cert_path = PathBuf::from(env("TLS_CERT_FILE").unwrap_or_else(|| "certs/cert.pem".into()));
        let key_path = PathBuf::from(env("TLS_KEY_FILE").unwrap_or_else(|| "certs/key.pem".into()));
        let (cert_pem, key_pem) = if cert_path.exists() || key_path.exists() {
            (std::fs::read(&cert_path)?, std::fs::read(&key_path)?)
        } else {
            self_signed(&cert_path, &key_path)?
        };
        let certs = CertificateDer::pem_slice_iter(&cert_pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("{}: {e}", cert_path.display()))?;
        let key =
            PrivateKeyDer::from_pem_slice(&key_pem).map_err(|e| anyhow::anyhow!("{}: {e}", key_path.display()))?;
        let mut config = ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(certs, key)?;
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(Some(Self { port, config: Arc::new(config) }))
    }

    pub async fn serve(self, app: Router) -> anyhow::Result<()> {
        let listener = TcpListener::bind(SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 0], self.port))).await?;
        tracing::info!("Inboxora backend serving HTTPS on port {}", self.port);
        let acceptor = TlsAcceptor::from(self.config);
        loop {
            let (tcp, peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    // e.g. EMFILE: back off instead of spinning.
                    tracing::warn!("HTTPS accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let acceptor = acceptor.clone();
            let app = app.clone();
            tokio::spawn(async move {
                let Ok(Ok(stream)) = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(tcp)).await else {
                    return;
                };
                let service = hyper::service::service_fn(move |req: Request<Incoming>| {
                    let mut app = app.clone();
                    async move {
                        let Ok(mut res) = app.call(edge_request(req, peer)).await;
                        res.headers_mut().insert(
                            header::STRICT_TRANSPORT_SECURITY,
                            HeaderValue::from_static("max-age=63072000; includeSubDomains; preload"),
                        );
                        Ok::<_, std::convert::Infallible>(res)
                    }
                });
                let mut builder = auto::Builder::new(TokioExecutor::new());
                builder.http1().timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(30));
                builder.http2().timer(TokioTimer::new()).keep_alive_interval(Duration::from_secs(30));
                if let Err(e) = builder.serve_connection_with_upgrades(TokioIo::new(stream), service).await {
                    tracing::debug!("HTTPS connection from {peer}: {e}");
                }
            });
        }
    }
}

fn edge_request(req: Request<Incoming>, peer: SocketAddr) -> Request<Body> {
    let mut req = req.map(Body::new);
    req.extensions_mut().insert(ConnectInfo(peer));
    // HTTP/2 carries the host in :authority; handlers (and the ntfy hop) read Host.
    if !req.headers().contains_key(header::HOST)
        && let Some(authority) = req.uri().authority().and_then(|a| HeaderValue::from_str(a.as_str()).ok())
    {
        req.headers_mut().insert(header::HOST, authority);
    }
    let h = req.headers_mut();
    h.remove("x-forwarded-for");
    h.remove("x-real-ip");
    h.insert(HeaderName::from_static("x-forwarded-proto"), HeaderValue::from_static("https"));
    req
}

/// Generate a self-signed certificate for localhost and the APP_URL host, and try to persist it.
fn self_signed(cert_path: &Path, key_path: &Path) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let mut names = vec!["localhost".to_owned(), "127.0.0.1".to_owned(), "::1".to_owned()];
    if let Some(host) = inboxora_core::config::env("APP_URL")
        .and_then(|u| url::Url::parse(&u).ok())
        .and_then(|u| u.host_str().map(|h| h.trim_matches(['[', ']']).to_owned()))
        && !names.contains(&host)
    {
        names.push(host);
    }
    let generated = rcgen::generate_simple_self_signed(names)?;
    let (cert, key) = (generated.cert.pem(), generated.signing_key.serialize_pem());
    let persist = || -> std::io::Result<()> {
        for path in [cert_path, key_path] {
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir)?;
            }
        }
        std::fs::write(cert_path, &cert)?;
        let mut key_file = std::fs::OpenOptions::new();
        key_file.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut key_file, 0o600);
        std::io::Write::write_all(&mut key_file.open(key_path)?, key.as_bytes())
    };
    match persist() {
        Ok(()) => tracing::info!("No TLS certificate found — generated a self-signed one at {}", cert_path.display()),
        Err(e) => tracing::warn!(
            "No TLS certificate found — using a self-signed one for this run only ({}: {e})",
            cert_path.display()
        ),
    }
    Ok((cert.into_bytes(), key.into_bytes()))
}
