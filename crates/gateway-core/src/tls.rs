use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;

use rustls::ServerConfig;
use rustls_pemfile::{certs, pkcs8_private_keys};
use tokio_rustls::TlsAcceptor;

use gateway_config::TlsConfig;

use crate::error::CoreError;

/// Build a `TlsAcceptor` from the gateway TLS configuration.
///
/// When `client_ca_path` is set, the acceptor requests a client certificate
/// and validates it against the provided CA. The validated certificate is
/// made available to the auth middleware via the `PeerCertificate` request
/// extension.
///
/// When `client_ca_path` is absent, TLS is server-only — standard HTTPS.
pub fn build_acceptor(cfg: &TlsConfig) -> Result<TlsAcceptor, CoreError> {
    let certs = load_certs(&cfg.cert_path)?;
    let key   = load_private_key(&cfg.key_path)?;

    let mut server_cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| CoreError::Tls(e.to_string()))?;

    // Enable mTLS if a client CA is configured
    if let Some(ca_path) = &cfg.client_ca_path {
        let client_auth = build_client_auth(ca_path)?;

        server_cfg = ServerConfig::builder()
            .with_client_cert_verifier(client_auth)
            .with_single_cert(
                load_certs(&cfg.cert_path)?,
                load_private_key(&cfg.key_path)?,
            )
            .map_err(|e| CoreError::Tls(e.to_string()))?;
    }

    // Enable HTTP/2 via ALPN negotiation
    server_cfg.alpn_protocols = vec![
        b"h2".to_vec(),
        b"http/1.1".to_vec(),
    ];

    Ok(TlsAcceptor::from(Arc::new(server_cfg)))
}

/// rustls-pemfile 1.x returns `Result<Vec<Vec<u8>>>` — convert each DER
/// byte vector to `CertificateDer<'static>` for rustls 0.23.
fn load_certs(path: &str) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, CoreError> {
    let f = File::open(path)
        .map_err(|e| CoreError::Tls(format!("cannot open cert file `{path}`: {e}")))?;
    let mut reader = BufReader::new(f);

    let raw = certs(&mut reader)
        .map_err(|e| CoreError::Tls(format!("cert parse error in `{path}`: {e}")))?;

    Ok(raw
        .into_iter()
        .map(rustls::pki_types::CertificateDer::from)
        .collect())
}

/// Load a PKCS#8 private key and convert to `PrivateKeyDer<'static>`.
fn load_private_key(path: &str) -> Result<rustls::pki_types::PrivateKeyDer<'static>, CoreError> {
    let f = File::open(path)
        .map_err(|e| CoreError::Tls(format!("cannot open key file `{path}`: {e}")))?;
    let mut reader = BufReader::new(f);

    let raw = pkcs8_private_keys(&mut reader)
        .map_err(|e| CoreError::Tls(format!("key parse error in `{path}`: {e}")))?;

    let der = raw
        .into_iter()
        .next()
        .ok_or_else(|| CoreError::Tls(format!("no PKCS#8 private key found in `{path}`")))?;

    Ok(rustls::pki_types::PrivateKeyDer::Pkcs8(
        rustls::pki_types::PrivatePkcs8KeyDer::from(der),
    ))
}

fn build_client_auth(
    ca_path: &str,
) -> Result<Arc<dyn rustls::server::danger::ClientCertVerifier>, CoreError> {
    let f = File::open(ca_path)
        .map_err(|e| CoreError::Tls(format!("cannot open CA file `{ca_path}`: {e}")))?;
    let mut reader = BufReader::new(f);

    let raw = certs(&mut reader)
        .map_err(|e| CoreError::Tls(format!("CA cert parse error: {e}")))?;

    let mut roots = rustls::RootCertStore::empty();
    for der in raw {
        roots
            .add(rustls::pki_types::CertificateDer::from(der))
            .map_err(|e| CoreError::Tls(format!("invalid CA cert: {e}")))?;
    }

    Ok(rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .map_err(|e| CoreError::Tls(e.to_string()))?)
}
