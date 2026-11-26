//! TLS for the PostgreSQL connection (rustls, `ring` provider, no OpenSSL).
//!
//! Certificates are **always verified** (chain and host name). Trust comes from the bundled
//! Mozilla roots, or from `ca_file` alone when one is given (typical for a private CA).
//! Whether TLS is *attempted* is controlled by `sslmode` in the connection string, or forced
//! with [`crate::PgOptions::require_tls`].

use crate::error::{Error, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{ring, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use std::path::PathBuf;
use std::sync::Arc;
use tokio_postgres_rustls::MakeRustlsConnect;

#[derive(Clone, Debug, Default)]
pub struct PgTls {
    /// PEM file with the CA certificate(s) that sign the server certificate. When set it
    /// *replaces* the bundled public roots.
    pub ca_file: Option<PathBuf>,
    /// DANGEROUS: accept any certificate (encryption without authentication; a man in the
    /// middle can read and change everything). Development only.
    pub danger_skip_cert_verification: bool,
}

#[derive(Debug)]
struct AcceptAny(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

impl PgTls {
    #[doc(hidden)]
    pub fn connector_for_tests(&self) -> MakeRustlsConnect {
        connector(self).expect("tls connector")
    }
}

pub(crate) fn connector(t: &PgTls) -> Result<MakeRustlsConnect> {
    let provider = Arc::new(ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Backend(format!("TLS setup failed: {e}")))?;
    let cfg = if t.danger_skip_cert_verification {
        builder.dangerous().with_custom_certificate_verifier(Arc::new(AcceptAny(provider))).with_no_client_auth()
    } else {
        let mut roots = RootCertStore::empty();
        match &t.ca_file {
            Some(p) => {
                let f = std::fs::File::open(p).map_err(|e| Error::Invalid(format!("cannot read CA file {}: {e}", p.display())))?;
                let mut n = 0;
                for c in rustls_pemfile::certs(&mut std::io::BufReader::new(f)) {
                    let c = c.map_err(|e| Error::Invalid(format!("bad PEM in {}: {e}", p.display())))?;
                    roots.add(c).map_err(|e| Error::Invalid(format!("unusable CA certificate in {}: {e}", p.display())))?;
                    n += 1;
                }
                if n == 0 {
                    return Err(Error::Invalid(format!("no certificates found in {}", p.display())));
                }
            }
            None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    };
    Ok(MakeRustlsConnect::new(cfg))
}
