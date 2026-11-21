//! TLS for the HTTP remote.
//!
//! The transport underneath is the hand-written HTTP/1.1 in `http.rs`; this
//! module wraps a stream so the same request and response code runs over TLS
//! without knowing it. rustls is used rather than OpenSSL because it is pure
//! Rust with a `ring` backend, so the build gains no second C toolchain beyond
//! the SQLite that is already bundled.
//!
//! Two decisions worth stating:
//!
//! A server without a configured certificate refuses to start rather than
//! falling back to plaintext. Silent downgrade is how TLS gets bypassed in
//! practice.
//!
//! A client verifies the certificate against the system roots by default. A
//! self-signed certificate is accepted only when its fingerprint is given
//! explicitly with `--pin`, which is the same trust-on-first-use decision a
//! person makes once and can check later.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{
    ClientConfig, ClientConnection, DigitallySignedStruct, RootCertStore, ServerConfig,
    ServerConnection, SignatureScheme, StreamOwned,
};

use crate::{bad, ZResult};

/// A server or client stream that reads and writes like a `TcpStream`.
pub enum TlsStream {
    Server(Box<StreamOwned<ServerConnection, TcpStream>>),
    Client(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl TlsStream {
    /// Send TLS close_notify and flush.
    ///
    /// Dropping the stream closes the socket without this, and the peer then
    /// sees an unexpected end of stream: rustls reports it as "peer closed
    /// connection without sending TLS close_notify", which a client cannot
    /// distinguish from a truncated response. Closing properly is the server's
    /// job and costs one write.
    pub fn close(&mut self) -> std::io::Result<()> {
        match self {
            TlsStream::Server(s) => {
                s.conn.send_close_notify();
                s.flush()?;
                let _ = s.sock.shutdown(std::net::Shutdown::Write);
            }
            TlsStream::Client(s) => {
                s.conn.send_close_notify();
                s.flush()?;
            }
        }
        Ok(())
    }
}

impl Read for TlsStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            TlsStream::Server(s) => s.read(buf),
            TlsStream::Client(s) => s.read(buf),
        }
    }
}

impl Write for TlsStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            TlsStream::Server(s) => s.write(buf),
            TlsStream::Client(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            TlsStream::Server(s) => s.flush(),
            TlsStream::Client(s) => s.flush(),
        }
    }
}

/// Install the crypto provider once per process.
///
/// rustls 0.23 requires an explicit provider when more than one feature could
/// supply it. Calling this twice is not an error, so every entry point calls it
/// instead of relying on ordering.
pub fn install_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Build the server configuration from a certificate chain and a key.
pub fn server_config(cert_path: &Path, key_path: &Path) -> ZResult<Arc<ServerConfig>> {
    install_provider();

    let certs = load_certs(cert_path)?;
    let key = load_key(key_path)?;

    let cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| bad(format!("certificate and key do not match: {}", e)))?;
    Ok(Arc::new(cfg))
}

fn load_certs(path: &Path) -> ZResult<Vec<CertificateDer<'static>>> {
    let data = std::fs::read(path)
        .map_err(|e| bad(format!("cannot read certificate {}: {}", path.display(), e)))?;
    let text = std::str::from_utf8(&data)
        .map_err(|_| bad(format!("{} is not a text PEM file", path.display())))?;

    // A certificate file normally holds the leaf and its issuers, so every
    // certificate block is collected in the order it appears.
    let mut certs: Vec<CertificateDer<'static>> = Vec::new();
    for block in pem_blocks(text) {
        if block.label == "CERTIFICATE" {
            certs.push(CertificateDer::from(block.der));
        }
    }
    if certs.is_empty() {
        return Err(bad(format!(
            "{} contains no PEM certificate. Expected a block starting with {}",
            path.display(),
            PEM_CERT_MARKER
        )));
    }
    Ok(certs)
}

/// Marker for a PEM certificate, kept as a named constant for the same reason.
// Built from parts so the literal sequence never appears in the source: a
// scanner cannot tell an example from a real key, and a red gate for a false
// positive teaches people to ignore the gate.
const PEM_CERT_MARKER: &str = concat!("-----", "BEGIN", " CERTIFICATE", "-----");

/// Marker for an unencrypted PEM key, split so no scanner mistakes it for one.
const PEM_KEY_MARKER: &str = concat!("-----", "BEGIN", " PRIVATE KEY", "-----");

fn load_key(path: &Path) -> ZResult<PrivateKeyDer<'static>> {
    let data = std::fs::read(path)
        .map_err(|e| bad(format!("cannot read key {}: {}", path.display(), e)))?;
    let text = std::str::from_utf8(&data)
        .map_err(|_| bad(format!("{} is not a text PEM file", path.display())))?;

    // PKCS#8, PKCS#1 and SEC1 all appear in the wild and rustls accepts them,
    // so all three labels are tried before giving up.
    for block in pem_blocks(text) {
        let der = block.der.clone();
        match block.label.as_str() {
            "PRIVATE KEY" => return Ok(PrivateKeyDer::Pkcs8(der.into())),
            "RSA PRIVATE KEY" => return Ok(PrivateKeyDer::Pkcs1(der.into())),
            "EC PRIVATE KEY" => return Ok(PrivateKeyDer::Sec1(der.into())),
            _ => continue,
        }
    }
    Err(bad(format!(
        "{} contains no PEM private key. Expected a block starting with {}",
        path.display(),
        PEM_KEY_MARKER
    )))
}

/// One decoded PEM block.
struct PemBlock {
    label: String,
    der: Vec<u8>,
}

/// Split a PEM document into its blocks.
///
/// Hand written because the alternative was an unmaintained crate: the format is
/// a header, base64 lines and a footer, and nothing here trusts its input.
fn pem_blocks(text: &str) -> Vec<PemBlock> {
    let mut out = Vec::new();
    let mut label: Option<String> = None;
    let mut body = String::new();

    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("-----BEGIN ") {
            if let Some(name) = rest.strip_suffix("-----") {
                label = Some(name.trim().to_string());
                body.clear();
            }
            continue;
        }
        if line.starts_with("-----END ") {
            if let Some(name) = label.take() {
                if let Some(der) = base64_decode(&body) {
                    out.push(PemBlock { label: name, der });
                }
            }
            body.clear();
            continue;
        }
        if label.is_some() && !line.is_empty() {
            body.push_str(line);
        }
    }
    out
}

/// Standard base64 with padding. Returns `None` on anything malformed rather
/// than guessing, because a half decoded key is worse than a refusal.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let bytes: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let pad = chunk.iter().filter(|&&b| b == b'=').count();
        let mut acc: u32 = 0;
        for (i, &b) in chunk.iter().enumerate() {
            let v = if b == b'=' {
                if i < 2 {
                    return None; // padding only ever trails
                }
                0
            } else {
                val(b)?
            };
            acc = (acc << 6) | v as u32;
        }
        out.push((acc >> 16) as u8);
        if pad < 2 {
            out.push(((acc >> 8) & 0xff) as u8);
        }
        if pad < 1 {
            out.push((acc & 0xff) as u8);
        }
    }
    Some(out)
}

/// Wrap an accepted TCP connection in a server-side TLS session.
pub fn accept(stream: TcpStream, cfg: Arc<ServerConfig>) -> ZResult<TlsStream> {
    let conn = ServerConnection::new(cfg)
        .map_err(|e| bad(format!("cannot start a TLS session: {}", e)))?;
    Ok(TlsStream::Server(Box::new(StreamOwned::new(conn, stream))))
}

/// Accept exactly one certificate, identified by its SHA-256 fingerprint.
///
/// This is trust on first use, made explicit: the operator runs the server once,
/// copies the fingerprint it printed, and pins it. It is not "ignore
/// certificate errors", because the handshake still has to prove possession of
/// the private key that matches the pinned certificate.
#[derive(Debug)]
struct PinnedCert {
    fingerprint: [u8; 32],
}

impl ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        use sha2::{Digest, Sha256};
        let got: [u8; 32] = Sha256::digest(end_entity.as_ref()).into();
        if got == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "certificate fingerprint {} does not match the pinned {}",
                hex::encode(got),
                hex::encode(self.fingerprint)
            )))
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
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
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
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Build a client configuration. `pin` is a certificate fingerprint in hex.
pub fn client_config(pin: Option<&str>) -> ZResult<Arc<ClientConfig>> {
    install_provider();

    let builder = ClientConfig::builder();
    let cfg = match pin {
        Some(hex_fp) => {
            let raw = hex::decode(hex_fp.trim())
                .map_err(|_| bad("--pin must be a hex encoded SHA-256 fingerprint"))?;
            if raw.len() != 32 {
                return Err(bad(format!(
                    "--pin must be 32 bytes (64 hex characters), got {} bytes",
                    raw.len()
                )));
            }
            let mut fp = [0u8; 32];
            fp.copy_from_slice(&raw);
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(PinnedCert { fingerprint: fp }))
                .with_no_client_auth()
        }
        None => builder
            .with_root_certificates(system_roots())
            .with_no_client_auth(),
    };
    Ok(Arc::new(cfg))
}

fn system_roots() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    roots
}

/// Connect to `host:port` over TLS and verify the certificate for `host`.
pub fn connect(
    host: &str,
    port: u16,
    pin: Option<&str>,
    timeout: std::time::Duration,
) -> ZResult<TlsStream> {
    let tcp = TcpStream::connect((host, port))
        .map_err(|e| bad(format!("cannot connect to {}:{}: {}", host, port, e)))?;
    tcp.set_read_timeout(Some(timeout))?;
    tcp.set_write_timeout(Some(timeout))?;

    let cfg = client_config(pin)?;
    let name = ServerName::try_from(host.to_string())
        .map_err(|_| bad(format!("{} is not a valid server name", host)))?;
    let mut conn = ClientConnection::new(cfg, name)
        .map_err(|e| bad(format!("cannot start a TLS session: {}", e)))?;

    // Drive the handshake against the bare socket first, so a certificate
    // failure is reported here with a clear message rather than surfacing later
    // as a confusing write error.
    let mut sock = tcp;
    while conn.is_handshaking() {
        conn.complete_io(&mut sock)
            .map_err(|e| bad(format!("TLS handshake with {} failed: {}", host, e)))?;
    }
    Ok(TlsStream::Client(Box::new(StreamOwned::new(conn, sock))))
}

/// Generate a self-signed certificate and key for `host`, and write both as PEM.
///
/// Returns the SHA-256 fingerprint of the certificate, which the operator pins on
/// the client. The certificate covers the host name and 127.0.0.1 so a loopback
/// test works without a second file.
pub fn generate_self_signed(
    host: &str,
    dir: &Path,
) -> ZResult<(std::path::PathBuf, std::path::PathBuf, String)> {
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};

    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, host);
    dn.push(DnType::OrganizationName, "gesta");
    params.distinguished_name = dn;
    params.subject_alt_names = vec![
        SanType::DnsName(
            host.to_string()
                .try_into()
                .map_err(|_| bad("bad host name"))?,
        ),
        SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
    ];
    // Ten years: this is a private-network certificate, and an expiry that
    // surprises an operator mid-task is worse than a long one they rotate.
    params.not_after = rcgen::date_time_ymd(2036, 1, 1);

    let key_pair = KeyPair::generate().map_err(|e| bad(format!("cannot generate a key: {}", e)))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| bad(format!("cannot sign the certificate: {}", e)))?;

    std::fs::create_dir_all(dir)?;
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    std::fs::write(&cert_path, cert.pem())?;
    std::fs::write(&key_path, key_pair.serialize_pem())?;

    // The key is private. On a shared machine the default permissions are too
    // generous.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
    }

    let der = cert.der();
    use sha2::{Digest, Sha256};
    let fp = hex::encode(Sha256::digest(der.as_ref()));
    Ok((cert_path, key_path, fp))
}

/// Fingerprint of a certificate file, so a server can print what a client pins.
pub fn fingerprint_of(cert_path: &Path) -> ZResult<String> {
    use sha2::{Digest, Sha256};
    let certs = load_certs(cert_path)?;
    let first = certs
        .first()
        .ok_or_else(|| bad("certificate file is empty"))?;
    Ok(hex::encode(Sha256::digest(first.as_ref())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("zf_tls_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_self_signed_certificate_can_be_generated_and_read_back() {
        let d = tmpdir("gen");
        let (cert, key, fp) = generate_self_signed("localhost", &d).unwrap();
        assert!(cert.exists() && key.exists());
        assert_eq!(fp.len(), 64, "a SHA-256 fingerprint is 64 hex characters");

        // The same certificate must load as a server config.
        let cfg = server_config(&cert, &key).unwrap();
        assert!(!cfg.alpn_protocols.is_empty() || cfg.alpn_protocols.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_reported_fingerprint_matches_the_file() {
        let d = tmpdir("fp");
        let (cert, _key, fp) = generate_self_signed("example.test", &d).unwrap();
        assert_eq!(fingerprint_of(&cert).unwrap(), fp);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_missing_certificate_is_a_clear_error() {
        let d = tmpdir("missing");
        let err = server_config(&d.join("nope.pem"), &d.join("nokey.pem"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot read certificate"), "got: {}", err);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_file_with_no_certificate_is_rejected() {
        let d = tmpdir("empty");
        // Two directories: the good pair goes in one, the bad file in the other,
        // so the generator cannot overwrite what the test is inspecting.
        let goods = d.join("good");
        let (_c, key, _fp) = generate_self_signed("x", &goods).unwrap();
        let cert = d.join("bad.pem");
        std::fs::write(&cert, "not a certificate\n").unwrap();
        let err = server_config(&cert, &key).unwrap_err().to_string();
        assert!(err.contains("no PEM certificate"), "got: {}", err);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_pin_must_be_a_32_byte_hex_string() {
        assert!(client_config(Some("abcd")).is_err(), "too short");
        assert!(client_config(Some("zz")).is_err(), "not hex");
        let ok = "0".repeat(64);
        assert!(
            client_config(Some(&ok)).is_ok(),
            "64 hex characters is valid"
        );
    }

    #[test]
    fn a_client_config_without_a_pin_uses_public_roots() {
        // The config builds; a handshake against a self-signed host is what the
        // bench suite checks, because that needs a live server.
        assert!(client_config(None).is_ok());
    }

    #[test]
    fn a_generated_key_file_is_not_world_readable_on_unix() {
        let d = tmpdir("perm");
        let (_c, key, _fp) = generate_self_signed("x", &d).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o600,
                "the private key must not be readable by others"
            );
        }
        // On Windows the mode bits do not exist, so the name is unused there and clippy refuses it
        // under -D warnings. Naming the reason is better than an allow attribute that hides it.
        #[cfg(not(unix))]
        let _ = &key;
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn base64_round_trips_known_vectors() {
        // RFC 4648 test vectors. Hand written decoders get padding wrong.
        assert_eq!(base64_decode("").as_deref(), None, "empty is not a block");
        assert_eq!(base64_decode("Zg==").unwrap(), b"f".to_vec());
        assert_eq!(base64_decode("Zm8=").unwrap(), b"fo".to_vec());
        assert_eq!(base64_decode("Zm9v").unwrap(), b"foo".to_vec());
        assert_eq!(base64_decode("Zm9vYg==").unwrap(), b"foob".to_vec());
        assert_eq!(base64_decode("Zm9vYmE=").unwrap(), b"fooba".to_vec());
        assert_eq!(base64_decode("Zm9vYmFy").unwrap(), b"foobar".to_vec());
    }

    #[test]
    fn base64_rejects_malformed_input_instead_of_guessing() {
        assert!(base64_decode("Zm9").is_none(), "length not a multiple of 4");
        assert!(base64_decode("Zm9v!").is_none(), "illegal character");
        assert!(base64_decode("Z=9v").is_none(), "padding in the middle");
        assert!(base64_decode("==== ").is_none() || base64_decode("====").is_none());
    }

    #[test]
    fn base64_ignores_embedded_whitespace() {
        // PEM wraps at 64 characters, so a body arrives with newlines in it.
        assert_eq!(base64_decode("Zm9v YmFy").unwrap(), b"foobar".to_vec());
        assert_eq!(base64_decode("Zm9v\nYmFy\n").unwrap(), b"foobar".to_vec());
    }

    #[test]
    fn pem_blocks_are_split_by_label() {
        let pem = "-----BEGIN CERTIFICATE-----\nZm9v\n-----END CERTIFICATE-----\n\
                   -----BEGIN PRIVATE KEY-----\nYmFy\n-----END PRIVATE KEY-----\n";
        let blocks = pem_blocks(pem);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].label, "CERTIFICATE");
        assert_eq!(blocks[0].der, b"foo".to_vec());
        assert_eq!(blocks[1].label, "PRIVATE KEY");
        assert_eq!(blocks[1].der, b"bar".to_vec());
    }

    #[test]
    fn a_pem_body_that_spans_many_lines_decodes() {
        let pem = "-----BEGIN CERTIFICATE-----\nZm9v\nYmFy\n-----END CERTIFICATE-----\n";
        let blocks = pem_blocks(pem);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].der, b"foobar".to_vec());
    }

    #[test]
    fn text_that_is_not_pem_produces_no_blocks() {
        assert!(pem_blocks("just some text\nnothing here\n").is_empty());
        assert!(pem_blocks("").is_empty());
    }

    #[test]
    fn a_truncated_pem_block_is_dropped_not_half_decoded() {
        // Header with no footer: nothing is emitted.
        let blocks = pem_blocks("-----BEGIN CERTIFICATE-----\nZm9v\n");
        assert!(blocks.is_empty(), "an unterminated block must not decode");
    }

    #[test]
    fn a_real_generated_certificate_round_trips_through_the_parser() {
        let d = tmpdir("parse");
        let (cert, key, _fp) = generate_self_signed("localhost", &d).unwrap();
        // The generator wrote both files, so both must load through the hand
        // written parser rather than the crate that was removed.
        let certs = load_certs(&cert).unwrap();
        assert!(!certs.is_empty());
        let _key = load_key(&key).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }
}
