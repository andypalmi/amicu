//! TLS for the VM's vLLM endpoint with a pinned self-signed certificate.
//!
//! The VM's external IP is unknown until it exists, so the certificate cannot
//! name it. amicu generates a certificate for the fixed name
//! `<instance>.amicu.invalid` before the VM is created and clients pin the
//! certificate itself: the SHA-256 fingerprint of its DER encoding, kept in the
//! [`super::VmRecord`]. [`PinnedTransport`] accepts exactly that certificate and
//! ignores the host name and the validity dates (the certificate is generated
//! for one VM and pinned, not chained to a CA, and its validity window is the
//! generator's default rather than a bound, so it is not "short-lived": what
//! limits its use is that the key exists only for that VM and its secrets are
//! deleted with it); it still checks the handshake signature, so the server
//! must hold the private key.
//!
//! The certificate is public and travels as instance metadata. The private key
//! is a secret: it goes to Secret Manager (`<instance>-tls-key`), or, with the
//! insecure metadata delivery, into instance metadata.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use amicu_core::secret::SecretValue;
use amicu_providers::{HttpMethod, HttpRequest, HttpResponse, HttpTransport, TransportError};
use async_trait::async_trait;
use rustls::DigitallySignedStruct;
use rustls::SignatureScheme;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, aws_lc_rs};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use sha2::{Digest, Sha256};

use crate::error::GcpError;

/// The certificate's DNS name suffix (`.invalid` never resolves).
pub const TLS_NAME_SUFFIX: &str = "amicu.invalid";

/// A certificate and its private key.
#[derive(Debug, Clone)]
pub struct TlsIdentity {
    /// The certificate, PEM. Public.
    pub cert_pem: String,
    /// The private key, PKCS#8 PEM.
    pub key_pem: SecretValue,
}

impl TlsIdentity {
    /// A fresh self-signed certificate and key for `instance`, with the SAN
    /// `<instance>.amicu.invalid`.
    ///
    /// # Errors
    /// [`GcpError::Invalid`] when the key or certificate cannot be generated.
    pub fn generate(instance: &str) -> Result<Self, GcpError> {
        let fail = |reason| GcpError::Invalid {
            kind: "tls certificate",
            value: String::new(),
            reason,
        };
        let name = format!("{instance}.{TLS_NAME_SUFFIX}");
        let ck = rcgen::generate_simple_self_signed(vec![name])
            .map_err(|_| fail("could not generate a self-signed certificate"))?;
        Ok(Self {
            cert_pem: ck.cert.pem(),
            key_pem: SecretValue::new(ck.signing_key.serialize_pem()),
        })
    }

    /// The pin: see [`fingerprint_of_pem`].
    ///
    /// # Errors
    /// [`GcpError::Invalid`] for a malformed certificate.
    pub fn fingerprint(&self) -> Result<String, GcpError> {
        fingerprint_of_pem(&self.cert_pem)
    }

    /// Checks the PEM texts: a parseable certificate and a one-key PEM.
    pub(crate) fn validate(&self) -> Result<(), GcpError> {
        self.fingerprint()?;
        let key_ok = self
            .key_pem
            .expose_secret_str()
            .is_ok_and(|k| k.contains("PRIVATE KEY-----") && !k.contains('\0'));
        if key_ok {
            Ok(())
        } else {
            Err(GcpError::Invalid {
                kind: "tls private key",
                value: String::new(),
                reason: "not a PEM private key",
            })
        }
    }
}

/// Lowercase hex SHA-256 of the certificate's DER encoding.
///
/// # Errors
/// [`GcpError::Invalid`] when `pem` holds no certificate.
pub fn fingerprint_of_pem(pem: &str) -> Result<String, GcpError> {
    let der = CertificateDer::from_pem_slice(pem.as_bytes()).map_err(|_| GcpError::Invalid {
        kind: "tls certificate",
        value: String::new(),
        reason: "not a PEM certificate",
    })?;
    Ok(hex(&Sha256::digest(der.as_ref())))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Where [`write_cert_pem`] puts the certificate of `instance` in `dir`.
#[must_use]
pub fn cert_path(dir: &Path, instance: &str) -> PathBuf {
    dir.join(format!("{instance}.tls.crt"))
}

/// Writes the certificate PEM to `<dir>/<instance>.tls.crt` (creating `dir`) and
/// returns the path, for client snippets that need the file (`curl --cacert`
/// works only against a name the certificate carries: pin the fingerprint
/// instead where the client allows it). The certificate is public.
///
/// # Errors
/// Any I/O error.
pub fn write_cert_pem(dir: &Path, instance: &str, cert_pem: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = cert_path(dir, instance);
    std::fs::write(&path, cert_pem)?;
    Ok(path)
}

#[derive(Debug)]
struct PinVerifier {
    expected: [u8; 32],
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if Sha256::digest(end_entity.as_ref()).as_slice() == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// An [`HttpTransport`] that accepts only the server certificate whose SHA-256
/// fingerprint it was built with.
pub struct PinnedTransport {
    client: reqwest::Client,
    timeout: Duration,
}

impl std::fmt::Debug for PinnedTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedTransport").finish_non_exhaustive()
    }
}

impl PinnedTransport {
    /// # Errors
    /// [`GcpError::Invalid`] for a fingerprint that is not 64 hex digits, or
    /// when the TLS client cannot be built.
    pub fn new(fingerprint: &str, timeout: Duration) -> Result<Self, GcpError> {
        let bad = |reason| GcpError::Invalid {
            kind: "tls fingerprint",
            value: fingerprint.to_owned(),
            reason,
        };
        let expected =
            parse_fingerprint(fingerprint).ok_or_else(|| bad("expected 64 hex digits"))?;
        let provider = Arc::new(aws_lc_rs::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|_| bad("TLS protocol versions unavailable"))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinVerifier { expected, provider }))
            .with_no_client_auth();
        let client = reqwest::Client::builder()
            .tls_backend_preconfigured(config)
            .build()
            .map_err(|_| bad("could not build the TLS client"))?;
        Ok(Self { client, timeout })
    }
}

fn parse_fingerprint(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

#[async_trait]
impl HttpTransport for PinnedTransport {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let method = match request.method {
            HttpMethod::Get => reqwest::Method::GET,
            HttpMethod::Post => reqwest::Method::POST,
            HttpMethod::Patch => reqwest::Method::PATCH,
            HttpMethod::Delete => reqwest::Method::DELETE,
        };
        let mut b = self
            .client
            .request(method, &request.url)
            .timeout(self.timeout);
        if let Some(token) = &request.bearer {
            b = b.bearer_auth(token);
        }
        for (name, value) in &request.headers {
            b = b.header(name, value);
        }
        if let Some(body) = &request.body {
            b = b.json(body);
        }
        let err = |e: &reqwest::Error| {
            if e.is_timeout() {
                TransportError::Timeout
            } else {
                TransportError::Connection(e.to_string())
            }
        };
        let resp = b.send().await.map_err(|e| err(&e))?;
        let status = resp.status().as_u16();
        let body = resp.bytes().await.map_err(|e| err(&e))?.to_vec();
        Ok(HttpResponse { status, body })
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    use super::*;

    /// Serves one `200` per connection over TLS with `id`, on 127.0.0.1.
    async fn serve(id: &TlsIdentity) -> u16 {
        let certs = vec![CertificateDer::from_pem_slice(id.cert_pem.as_bytes()).unwrap()];
        let key =
            rustls::pki_types::PrivateKeyDer::from_pem_slice(id.key_pem.expose_secret()).unwrap();
        let config =
            rustls::ServerConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(certs, key)
                .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let mut buf = [0u8; 1024];
                    let _ = tls.read(&mut buf).await;
                    let _ = tls
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                        )
                        .await;
                    let _ = tls.shutdown().await;
                });
            }
        });
        port
    }

    fn get(port: u16) -> HttpRequest {
        HttpRequest::new(HttpMethod::Get, format!("https://127.0.0.1:{port}/health"))
    }

    #[test]
    fn generated_identity_has_a_stable_fingerprint_and_a_fresh_key() {
        let a = TlsIdentity::generate("vm-1").unwrap();
        let b = TlsIdentity::generate("vm-1").unwrap();
        let f = a.fingerprint().unwrap();
        assert_eq!(f.len(), 64);
        assert!(f.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(f, fingerprint_of_pem(&a.cert_pem).unwrap());
        assert_ne!(f, b.fingerprint().unwrap());
        a.validate().unwrap();
        assert!(!format!("{a:?}").contains("PRIVATE"));
        assert!(fingerprint_of_pem("nope").is_err());
    }

    #[test]
    fn writes_the_certificate_next_to_state() {
        let dir = std::env::temp_dir().join(format!("amicu-tls-{}", std::process::id()));
        let id = TlsIdentity::generate("vm-1").unwrap();
        let p = write_cert_pem(&dir, "vm-1", &id.cert_pem).unwrap();
        assert!(p.ends_with("vm-1.tls.crt"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), id.cert_pem);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn accepts_the_pinned_certificate_and_rejects_another() {
        let server = TlsIdentity::generate("vm-1").unwrap();
        let other = TlsIdentity::generate("vm-1").unwrap();
        let port = serve(&server).await;

        let t =
            PinnedTransport::new(&server.fingerprint().unwrap(), Duration::from_secs(5)).unwrap();
        let r = t.execute(get(port)).await.unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"ok");

        let t =
            PinnedTransport::new(&other.fingerprint().unwrap(), Duration::from_secs(5)).unwrap();
        let e = t.execute(get(port)).await.unwrap_err();
        assert!(matches!(e, TransportError::Connection(_)), "{e:?}");
    }

    #[test]
    fn rejects_a_malformed_fingerprint() {
        assert!(PinnedTransport::new("abc", Duration::from_secs(1)).is_err());
        assert!(PinnedTransport::new(&"zz".repeat(32), Duration::from_secs(1)).is_err());
    }
}
