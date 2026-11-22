//! Remote transport: TLS 1.3 with pinned self-signed certificates and one-time pairing codes
//! (ADR-003, threats T8–T12).
//!
//! * **Identity.** Daemons and controllers each own an ECDSA P-256 key and a self-signed cert.
//!   Peers are recognised by the SHA-256 fingerprint of the certificate (no CA, no hostnames).
//! * **Proof of possession.** The custom verifiers below still verify every TLS handshake
//!   signature, so a peer must hold the private key of the certificate it presents.
//! * **Pairing.** The daemon owner issues a one-time code with 100 random bits. Both sides
//!   prove knowledge of it with HMAC-SHA256 over a transcript that binds *both* certificate
//!   fingerprints. A machine-in-the-middle presents its own certificate, so the fingerprints
//!   differ and both proofs fail. With 100-bit codes, offline guessing is infeasible, so no PAKE
//!   is needed (documented change to ADR-003).

use std::sync::{Arc, Mutex};

use hmac::{Hmac, Mac};
use rustls::DigitallySignedStruct;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DistinguishedName, SignatureScheme};
use sha2::{Digest, Sha256};

/// A certificate and its private key (PKCS#8 DER).
#[derive(Clone)]
pub struct Identity {
    /// Certificate DER.
    pub cert_der: Vec<u8>,
    /// Private key PKCS#8 DER (secret).
    pub key_der: Vec<u8>,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("fingerprint", &fingerprint(&self.cert_der))
            .finish_non_exhaustive()
    }
}

impl Identity {
    /// Generates a new self-signed identity.
    ///
    /// # Errors
    /// Key generation failure.
    pub fn generate(name: &str) -> Result<Self, String> {
        let ck = rcgen::generate_simple_self_signed(vec![sanitize_name(name)])
            .map_err(|e| e.to_string())?;
        Ok(Self {
            cert_der: ck.cert.der().to_vec(),
            key_der: ck.key_pair.serialize_der(),
        })
    }

    /// SHA-256 fingerprint of the certificate.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.cert_der)
    }

    /// Serializes to JSON (contains the private key: store it as a secret).
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::json!({ "cert": hex::encode(&self.cert_der), "key": hex::encode(&self.key_der) }).to_string()
    }

    /// Parses [`Identity::to_json`] output.
    #[must_use]
    pub fn from_json(s: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(s).ok()?;
        Some(Self {
            cert_der: hex::decode(v["cert"].as_str()?).ok()?,
            key_der: hex::decode(v["key"].as_str()?).ok()?,
        })
    }

    fn chain(&self) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        (
            vec![CertificateDer::from(self.cert_der.clone())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_der.clone())),
        )
    }
}

fn sanitize_name(name: &str) -> String {
    let n: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(40)
        .collect();
    if n.is_empty() {
        "msm".into()
    } else {
        n.to_lowercase()
    }
}

/// Lowercase hex SHA-256 of a certificate (DER).
#[must_use]
pub fn fingerprint(cert_der: &[u8]) -> String {
    hex::encode(Sha256::digest(cert_der))
}

/// Groups a fingerprint for humans: `AB12 CD34 …` (first 16 bytes).
#[must_use]
pub fn display_fingerprint(fp: &str) -> String {
    fp.as_bytes()
        .chunks(4)
        .take(8)
        .map(|c| String::from_utf8_lossy(c).to_uppercase())
        .collect::<Vec<_>>()
        .join(" ")
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Accepts any client certificate whose handshake signature is valid. Authorization happens
/// afterwards by fingerprint (paired devices) — unknown clients only get pairing.
#[derive(Debug)]
struct AnyClientCert(Arc<rustls::crypto::CryptoProvider>);

impl ClientCertVerifier for AnyClientCert {
    fn offer_client_auth(&self) -> bool {
        true
    }
    fn client_auth_mandatory(&self) -> bool {
        true
    }
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
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

/// Pins the server certificate fingerprint (or records it, when pairing).
#[derive(Debug)]
struct PinnedServer {
    provider: Arc<rustls::crypto::CryptoProvider>,
    expected: Option<String>,
    seen: Arc<Mutex<Option<String>>>,
}

impl ServerCertVerifier for PinnedServer {
    fn verify_server_cert(
        &self,
        end: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fp = fingerprint(end.as_ref());
        if let Ok(mut s) = self.seen.lock() {
            *s = Some(fp.clone());
        }
        match &self.expected {
            Some(want) if *want != fp => Err(rustls::Error::General(
                "server certificate does not match the paired fingerprint".into(),
            )),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            m,
            c,
            d,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            m,
            c,
            d,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// TLS acceptor for the daemon (TLS 1.3 only, client certificate required).
///
/// # Errors
/// Invalid identity.
pub fn acceptor(identity: &Identity) -> Result<tokio_rustls::TlsAcceptor, String> {
    let p = provider();
    let (chain, key) = identity.chain();
    let config = rustls::ServerConfig::builder_with_provider(p.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .with_client_cert_verifier(Arc::new(AnyClientCert(p)))
        .with_single_cert(chain, key)
        .map_err(|e| e.to_string())?;
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

/// Server-side stream after the handshake, with the client's fingerprint.
pub type ServerStream = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
/// Client-side stream.
pub type ClientStream = tokio_rustls::client::TlsStream<tokio::net::TcpStream>;

/// Completes a TLS handshake on an accepted TCP connection (with a timeout) and returns the
/// client's certificate fingerprint.
///
/// # Errors
/// Handshake failures or timeout.
pub async fn accept(
    acceptor: &tokio_rustls::TlsAcceptor,
    tcp: tokio::net::TcpStream,
) -> Result<(ServerStream, String), String> {
    let tls = tokio::time::timeout(std::time::Duration::from_secs(10), acceptor.accept(tcp))
        .await
        .map_err(|_| "TLS handshake timed out".to_owned())?
        .map_err(|e| e.to_string())?;
    let fp = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|c| c.first())
        .map(|c| fingerprint(c.as_ref()))
        .ok_or_else(|| "client presented no certificate".to_owned())?;
    Ok((tls, fp))
}

/// Connects to a daemon. With `expected_fp`, the server must present exactly that certificate;
/// without it (pairing only), any certificate is accepted and its fingerprint returned for the
/// pairing proof.
///
/// # Errors
/// Connection, handshake or pin-mismatch failures.
pub async fn connect(
    addr: &str,
    identity: &Identity,
    expected_fp: Option<&str>,
) -> Result<(ClientStream, String), String> {
    let p = provider();
    let seen = Arc::new(Mutex::new(None));
    let (chain, key) = identity.chain();
    let config = rustls::ClientConfig::builder_with_provider(p.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedServer {
            provider: p,
            expected: expected_fp.map(str::to_owned),
            seen: seen.clone(),
        }))
        .with_client_auth_cert(chain, key)
        .map_err(|e| e.to_string())?;
    let tcp = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::net::TcpStream::connect(addr),
    )
    .await
    .map_err(|_| format!("timed out connecting to {addr}"))?
    .map_err(|e| format!("cannot reach {addr}: {e}"))?;
    let name = ServerName::try_from("msm").map_err(|e| e.to_string())?;
    let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .map_err(|e| e.to_string())?;
    let fp = seen
        .lock()
        .ok()
        .and_then(|s| s.clone())
        .ok_or_else(|| "no server certificate".to_owned())?;
    Ok((tls, fp))
}

/// Generates a one-time pairing code: 20 Crockford base32 characters (100 bits), shown in
/// groups of five.
#[must_use]
pub fn new_pairing_code() -> String {
    use rand::RngCore;
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut bytes = [0u8; 20];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let chars: String = bytes
        .iter()
        .map(|b| ALPHABET[usize::from(b & 31)] as char)
        .collect();
    chars
        .as_bytes()
        .chunks(5)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join("-")
}

/// Normalises user input of a code (case, separators, look-alike letters).
#[must_use]
pub fn normalize_code(input: &str) -> String {
    input
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            other => other,
        })
        .collect()
}

fn proof(code: &str, label: &[u8], a: &str, b: &str) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(normalize_code(code).as_bytes())
        .unwrap_or_else(|_| unreachable!("HMAC accepts any key length"));
    mac.update(label);
    mac.update(a.as_bytes());
    mac.update(b"|");
    mac.update(b.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// Client's proof: binds the code to the server's and the client's certificates.
#[must_use]
pub fn client_proof(code: &str, server_fp: &str, client_fp: &str) -> String {
    hex::encode(proof(code, b"msm-pair-v1/client", server_fp, client_fp))
}

/// Server's proof back to the client.
#[must_use]
pub fn server_proof(code: &str, server_fp: &str, client_fp: &str) -> String {
    hex::encode(proof(code, b"msm-pair-v1/server", server_fp, client_fp))
}

/// Constant-time check of a hex proof.
#[must_use]
pub fn verify_proof(expected_hex: &str, given_hex: &str) -> bool {
    let (Ok(a), Ok(b)) = (hex::decode(expected_hex), hex::decode(given_hex)) else {
        return false;
    };
    a.len() == b.len() && a.iter().zip(&b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[test]
    fn codes_are_long_and_normalized() {
        let c = new_pairing_code();
        assert_eq!(c.len(), 23);
        assert_eq!(normalize_code(&c).len(), 20);
        assert_eq!(normalize_code("abcde-oil11"), "ABCDE01111");
        assert_ne!(new_pairing_code(), new_pairing_code());
    }

    #[test]
    fn proofs_bind_both_fingerprints() {
        let p = client_proof("AAAAA-BBBBB-CCCCC-DDDDD", "srv", "cli");
        assert!(verify_proof(
            &p,
            &client_proof("aaaaabbbbbcccccddddd", "srv", "cli")
        ));
        assert!(!verify_proof(
            &p,
            &client_proof("AAAAA-BBBBB-CCCCC-DDDDE", "srv", "cli")
        ));
        assert!(!verify_proof(
            &p,
            &client_proof("AAAAA-BBBBB-CCCCC-DDDDD", "mitm", "cli")
        ));
        assert!(!verify_proof(
            &p,
            &server_proof("AAAAA-BBBBB-CCCCC-DDDDD", "srv", "cli")
        ));
        assert!(!verify_proof(&p, "zz"));
    }

    #[test]
    fn identity_round_trips() {
        let id = Identity::generate("my-host").unwrap();
        let back = Identity::from_json(&id.to_json()).unwrap();
        assert_eq!(back.fingerprint(), id.fingerprint());
        assert_eq!(display_fingerprint(&id.fingerprint()).split(' ').count(), 8);
    }

    #[tokio::test]
    async fn mutual_tls_with_pinning() {
        let server = Identity::generate("server").unwrap();
        let client = Identity::generate("client").unwrap();
        let other = Identity::generate("impostor").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let acc = acceptor(&server).unwrap();
        let client_fp = client.fingerprint();
        let srv = tokio::spawn(async move {
            // First connection: pinned, succeeds and echoes.
            let (tcp, _) = listener.accept().await.unwrap();
            let (mut tls, fp) = accept(&acc, tcp).await.unwrap();
            assert_eq!(fp, client_fp);
            let mut buf = [0u8; 4];
            tls.read_exact(&mut buf).await.unwrap();
            tls.write_all(&buf).await.unwrap();
            tls.flush().await.unwrap();
            // Second connection: client expects a different fingerprint and must abort.
            let (tcp, _) = listener.accept().await.unwrap();
            let _ = accept(&acc, tcp).await;
        });
        let (mut tls, seen) = connect(&addr, &client, Some(&server.fingerprint()))
            .await
            .unwrap();
        assert_eq!(seen, server.fingerprint());
        tls.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        tls.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        let err = connect(&addr, &client, Some(&other.fingerprint()))
            .await
            .unwrap_err();
        assert!(err.contains("does not match"), "{err}");
        srv.await.unwrap();
    }
}
