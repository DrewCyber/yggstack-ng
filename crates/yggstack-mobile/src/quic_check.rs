//! Measure RTT to a QUIC peer via handshake timing.
//!
//! Mirrors the Go yggstack `Mobile.checkQUICPeer`: dial the peer with TLS 1.3
//! only, no ALPN, certificate verification disabled (identity binding happens
//! after the handshake in yggdrasil; the handshake itself is the measurement),
//! and report the wall-clock duration in milliseconds, or -1 on failure.
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use quinn::crypto::rustls::QuicClientConfig;
use quinn::Connection;
use rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use rustls::crypto::ring as ring_provider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::DigitallySignedStruct;

/// Overall budget for DNS + handshake, matching the Go implementation.
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Accept-any-certificate verifier (RTT probe only — no data is exchanged).
#[derive(Debug)]
struct AcceptAllVerifier(Arc<rustls::crypto::CryptoProvider>);

impl AcceptAllVerifier {
    fn new(provider: Arc<rustls::crypto::CryptoProvider>) -> Self {
        Self(provider)
    }
}

impl ServerCertVerifier for AcceptAllVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Ping a `quic://host:port` peer; returns RTT in milliseconds or -1.
pub fn check_quic_rtt(uri: &str) -> i64 {
    let Some(target) = parse_target(uri) else {
        return -1;
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(_) => return -1,
    };
    rt.block_on(async {
        let start = Instant::now();
        let connected =
            tokio::time::timeout(CHECK_TIMEOUT, connect(&target)).await;
        match connected {
            Ok(Ok(conn)) => {
                let ms = start.elapsed().as_millis() as i64;
                conn.close(0u32.into(), b"rtt-probe");
                ms
            }
            _ => -1,
        }
    })
}

/// Extract "host:port" from a `quic://` URI (query string ignored,
/// default port 443 like the Go version).
fn parse_target(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("quic://").unwrap_or(uri);
    let rest = rest.split('?').next()?.trim();
    if rest.is_empty() {
        return None;
    }
    if let Some(inner) = rest
        .strip_prefix('[')
        .and_then(|h| h.split_once(']'))
    {
        // [ipv6]:port
        let port = inner.1.strip_prefix(':').filter(|p| !p.is_empty());
        return Some(match port {
            Some(p) => format!("[{}]:{}", inner.0, p),
            None => format!("[{}]:443", inner.0),
        });
    }
    match rest.rsplit_once(':') {
        Some((_host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            Some(rest.to_string())
        }
        _ => Some(format!("{rest}:443")),
    }
}

/// Resolve, then complete the QUIC/TLS handshake against the first
/// reachable address (IPv6 first, mirroring the yggdrasil dialer).
async fn connect(target: &str) -> Result<Connection, String> {
    let mut addrs: Vec<SocketAddr> = tokio::net::lookup_host(target)
        .await
        .map_err(|e| format!("resolve {target}: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err("no address resolved".into());
    }
    addrs.sort_unstable();
    addrs.dedup();

    let provider = Arc::new(ring_provider::default_provider());
    let client_config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAllVerifier::new(provider)))
        .with_no_client_auth();
    // no ALPN — yggdrasil does not use protocol negotiation

    let quic_config = Arc::new(
        QuicClientConfig::try_from(Arc::new(client_config))
            .map_err(|e| format!("quic client config: {e}"))?,
    );

    let server_name = target.rsplit_once(':').map(|(h, _)| h).unwrap_or(target);
    let server_name = server_name
        .trim_start_matches('[')
        .trim_end_matches(']');

    let mut last_err = String::from("no address resolved");
    for addr in addrs {
        let bind: SocketAddr = if addr.is_ipv6() {
            "[::]:0".parse().unwrap()
        } else {
            "0.0.0.0:0".parse().unwrap()
        };
        let mut endpoint = quinn::Endpoint::client(bind)
            .map_err(|e| format!("bind: {e}"))?;
        let mut cfg = quinn::ClientConfig::new(quic_config.clone());
        let mut transport = quinn::TransportConfig::default();
        transport.max_idle_timeout(Some(
            CHECK_TIMEOUT.try_into().expect("valid idle timeout"),
        ));
        cfg.transport_config(Arc::new(transport));
        endpoint.set_default_client_config(cfg);
        match endpoint.connect(addr, server_name) {
            Ok(connecting) => match connecting.await {
                Ok(conn) => return Ok(conn),
                Err(e) => last_err = format!("handshake: {e}"),
            },
            Err(e) => last_err = format!("connect: {e}"),
        }
    }
    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_targets() {
        assert_eq!(
            parse_target("quic://example.org:1234?key=abc").as_deref(),
            Some("example.org:1234")
        );
        assert_eq!(parse_target("quic://example.org").as_deref(), Some("example.org:443"));
        assert_eq!(
            parse_target("quic://[200:1:2:3:4:5:6:7]:999").as_deref(),
            Some("[200:1:2:3:4:5:6:7]:999")
        );
        assert_eq!(parse_target("quic://").as_deref(), None);
        // Scheme is optional; the app always passes quic:// URIs.
        assert_eq!(parse_target("host:5").as_deref(), Some("host:5"));
    }

    #[test]
    fn unreachable_returns_minus_one() {
        // RFC 5737 documentation address — guaranteed unroutable; the probe
        // must fail fast-ish and report -1 rather than hang or panic.
        let rtt = check_quic_rtt("quic://192.0.2.1:1");
        assert_eq!(rtt, -1);
    }
}
