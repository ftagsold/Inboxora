//! Sockets: pinned-address TCP connect, TLS (Mozilla roots like Node, plus
//! `NODE_EXTRA_CA_CERTS`), and the COMPRESS=DEFLATE layer (RFC 4978).

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send + Sync {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + Sync> Io for T {}

pub type BoxIo = Box<dyn Io>;

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

static ROOTS: LazyLock<Arc<RootCertStore>> = LazyLock::new(|| {
    let mut roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    if let Ok(path) = std::env::var("NODE_EXTRA_CA_CERTS") {
        use rustls_pki_types::pem::PemObject;
        match CertificateDer::pem_file_iter(&path) {
            Ok(certs) => {
                for cert in certs.flatten() {
                    let _ = roots.add(cert);
                }
            }
            Err(e) => tracing::warn!("NODE_EXTRA_CA_CERTS {path}: {e}"),
        }
    }
    Arc::new(roots)
});

static VERIFIED: LazyLock<Arc<ClientConfig>> = LazyLock::new(|| {
    Arc::new(
        ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .expect("TLS versions")
            .with_root_certificates(ROOTS.clone())
            .with_no_client_auth(),
    )
});

static UNVERIFIED: LazyLock<Arc<ClientConfig>> = LazyLock::new(|| {
    Arc::new(
        ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .expect("TLS versions")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAny(provider())))
            .with_no_client_auth(),
    )
});

/// `rejectUnauthorized: false`.
#[derive(Debug)]
struct AcceptAny(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Connect to the first reachable address. Like Node's `autoSelectFamily`, each address but the
/// last gets a short attempt window and families are interleaved (RFC 8305).
pub async fn connect_tcp(addresses: &[IpAddr], port: u16, deadline: Duration) -> io::Result<TcpStream> {
    let (v6, v4): (Vec<IpAddr>, Vec<IpAddr>) = addresses.iter().partition(|a| a.is_ipv6());
    let mut ordered = Vec::with_capacity(addresses.len());
    let first_v6 = addresses.first().is_some_and(IpAddr::is_ipv6);
    let (a, b) = if first_v6 { (v6, v4) } else { (v4, v6) };
    for i in 0..a.len().max(b.len()) {
        ordered.extend(a.get(i).copied());
        ordered.extend(b.get(i).copied());
    }
    if ordered.is_empty() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no addresses to connect to"));
    }
    let started = tokio::time::Instant::now();
    let mut last_err = None;
    for (i, ip) in ordered.iter().enumerate() {
        let remaining = deadline.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        let window = if i + 1 == ordered.len() { remaining } else { remaining.min(Duration::from_millis(1000)) };
        match tokio::time::timeout(window, TcpStream::connect(SocketAddr::new(*ip, port))).await {
            Ok(Ok(s)) => {
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Ok(Err(e)) => last_err = Some(e),
            Err(_) => last_err = Some(io::Error::new(io::ErrorKind::TimedOut, "connect timeout")),
        }
    }
    Err(last_err.unwrap_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "connect timeout")))
}

pub async fn tls<S: Io + 'static>(io: S, servername: &str, verify: bool) -> io::Result<BoxIo> {
    let cfg = if verify { VERIFIED.clone() } else { UNVERIFIED.clone() };
    let name = ServerName::try_from(servername.to_owned())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
    let stream = tokio_rustls::TlsConnector::from(cfg).connect(name, io).await?;
    Ok(Box::new(stream))
}

/// Raw-deflate both directions; every write is sync-flushed so a command is never held back.
pub struct Deflate {
    inner: BoxIo,
    inflate: Decompress,
    deflate: Compress,
    read_buf: Vec<u8>,
    read_pos: usize,
    read_eof: bool,
    write_buf: Vec<u8>,
}

impl Deflate {
    pub fn new(inner: BoxIo) -> Self {
        Self {
            inner,
            inflate: Decompress::new(false),
            deflate: Compress::new(Compression::default(), false),
            read_buf: Vec::with_capacity(16 * 1024),
            read_pos: 0,
            read_eof: false,
            write_buf: Vec::with_capacity(16 * 1024),
        }
    }

    fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while !self.write_buf.is_empty() {
            let n = ready!(Pin::new(&mut self.inner).poll_write(cx, &self.write_buf))?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.write_buf.drain(..n);
        }
        Poll::Ready(Ok(()))
    }
}

fn zerr(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("deflate: {e}"))
}

impl AsyncRead for Deflate {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, out: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        loop {
            if this.read_pos < this.read_buf.len() || this.read_eof {
                let input = &this.read_buf[this.read_pos..];
                let dst = out.initialize_unfilled();
                let (before_in, before_out) = (this.inflate.total_in(), this.inflate.total_out());
                let status = this.inflate.decompress(input, dst, FlushDecompress::Sync).map_err(zerr)?;
                let consumed = (this.inflate.total_in() - before_in) as usize;
                let produced = (this.inflate.total_out() - before_out) as usize;
                this.read_pos += consumed;
                if this.read_pos == this.read_buf.len() {
                    this.read_buf.clear();
                    this.read_pos = 0;
                }
                if produced > 0 {
                    out.advance(produced);
                    return Poll::Ready(Ok(()));
                }
                if this.read_eof || matches!(status, Status::StreamEnd) {
                    return Poll::Ready(Ok(()));
                }
                if consumed > 0 {
                    continue;
                }
            }
            // Need more compressed input.
            let mut chunk = [0u8; 16 * 1024];
            let mut rb = ReadBuf::new(&mut chunk);
            ready!(Pin::new(&mut this.inner).poll_read(cx, &mut rb))?;
            if rb.filled().is_empty() {
                this.read_eof = true;
                if this.read_pos >= this.read_buf.len() {
                    return Poll::Ready(Ok(()));
                }
            } else {
                this.read_buf.extend_from_slice(rb.filled());
            }
        }
    }
}

impl AsyncWrite for Deflate {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        // Bound buffered output so a large APPEND streams instead of growing without limit.
        if this.write_buf.len() > 64 * 1024 {
            ready!(this.poll_drain(cx))?;
        }
        let mut consumed = 0;
        while consumed < data.len() {
            this.write_buf.reserve(data.len() - consumed + 1024);
            let before = this.deflate.total_in();
            this.deflate.compress_vec(&data[consumed..], &mut this.write_buf, FlushCompress::None).map_err(zerr)?;
            consumed += (this.deflate.total_in() - before) as usize;
        }
        Poll::Ready(Ok(data.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        loop {
            this.write_buf.reserve(1024);
            let before = this.write_buf.len();
            this.deflate.compress_vec(&[], &mut this.write_buf, FlushCompress::Sync).map_err(zerr)?;
            if this.write_buf.len() - before < this.write_buf.capacity() - before {
                break;
            }
        }
        ready!(this.poll_drain(cx))?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.as_mut().poll_flush(cx))?;
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    #[tokio::test]
    async fn deflate_round_trips_across_flushes() {
        let (a, b) = duplex(1 << 16);
        let mut left = Deflate::new(Box::new(a));
        let mut right = Deflate::new(Box::new(b));
        for msg in [&b"A1 NOOP\r\n"[..], b"A2 UID FETCH 1:* (FLAGS)\r\n", &vec![b'x'; 100_000]] {
            left.write_all(msg).await.unwrap();
            left.flush().await.unwrap();
            let mut got = vec![0u8; msg.len()];
            right.read_exact(&mut got).await.unwrap();
            assert_eq!(got, msg);
        }
    }
}
