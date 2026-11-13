// SPDX-License-Identifier: EUPL-1.2
//! TLS for the control channel. Clients identify themselves with (usually
//! self-signed) certificates; Mumble's user identity is the SHA-1 of the leaf
//! certificate's DER, in lowercase hex. Any certificate is accepted, but the
//! client must prove possession of its key in the handshake. Without a
//! configured certificate, a self-signed one is generated once and kept, since
//! clients pin the server certificate.
//!
//! The certificate can be replaced while the server runs ([`Certificates`],
//! SIGHUP): new handshakes use the new one, established connections keep
//! their session, and a certificate that does not load keeps the old one.
//!
//! A client certificate that chains to a configured authority is "strong"
//! ([`Authorities`], Murmur's `bVerified`), which only adds to what a
//! certificate gives: nothing is refused for not being strong.
use rustls::{
    DigitallySignedStruct, DistinguishedName, SignatureScheme,
    client::danger::HandshakeSignatureValid,
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{
        CertificateDer, PrivateKeyDer, SignatureVerificationAlgorithm, TrustAnchor, UnixTime,
        pem::PemObject,
    },
    server::{
        ClientHello, ResolvesServerCert,
        danger::{ClientCertVerified, ClientCertVerifier},
    },
    sign::CertifiedKey,
};
use std::{
    fmt,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

/// ALPN for the Nuschl extension (docs/extension.md); `mumble` is accepted too,
/// because rustls refuses a client whose offer shares nothing with ours. Stock
/// clients offer no ALPN at all.
use nuschl_protocol::nuschl::{ALPN as ALPN_NUSCHL, ALPN_MUMBLE};

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("cannot read {path}: {message}")]
    Read { path: PathBuf, message: String },
    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("certificate generation failed: {0}")]
    Generate(String),
    #[error("invalid TLS configuration: {0}")]
    Config(#[from] rustls::Error),
}

pub fn sha1_hex(der: &[u8]) -> String {
    hex::encode(ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, der).as_ref())
}

pub fn sha256_hex(der: &[u8]) -> String {
    hex::encode(ring::digest::digest(&ring::digest::SHA256, der).as_ref())
}

pub struct Identity {
    pub chain: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
}

impl Identity {
    /// A PEM certificate chain and private key held in memory.
    pub fn from_pem(certificate: &[u8], key: &[u8]) -> Result<Self, TlsError> {
        let read_error = |what: &str| {
            let what = PathBuf::from(what);
            move |error: rustls::pki_types::pem::Error| TlsError::Read {
                path: what.clone(),
                message: error.to_string(),
            }
        };
        let chain = CertificateDer::pem_slice_iter(certificate)
            .collect::<Result<Vec<_>, _>>()
            .map_err(read_error("certificate"))?;
        if chain.is_empty() {
            return Err(TlsError::Read {
                path: PathBuf::from("certificate"),
                message: "no certificate found".into(),
            });
        }
        let key = PrivateKeyDer::from_pem_slice(key).map_err(read_error("private key"))?;
        Ok(Self { chain, key })
    }

    /// The chain with its signing key; fails if the key does not belong to
    /// the leaf certificate or cannot sign.
    pub fn certified(self) -> Result<Arc<CertifiedKey>, TlsError> {
        let provider = rustls::crypto::ring::default_provider();
        Ok(Arc::new(CertifiedKey::from_der(
            self.chain, self.key, &provider,
        )?))
    }

    pub fn load(certificate: &Path, key: &Path) -> Result<Self, TlsError> {
        let read_error = |path: &Path| {
            let path = path.to_owned();
            move |error: rustls::pki_types::pem::Error| TlsError::Read {
                path,
                message: error.to_string(),
            }
        };
        let chain = CertificateDer::pem_file_iter(certificate)
            .map_err(read_error(certificate))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(read_error(certificate))?;
        if chain.is_empty() {
            return Err(TlsError::Read {
                path: certificate.to_owned(),
                message: "no certificate found".into(),
            });
        }
        let key = PrivateKeyDer::from_pem_file(key).map_err(read_error(key))?;
        Ok(Self { chain, key })
    }

    /// The configured identity, or the generated one in `directory` (created on
    /// first start with an owner-only private key).
    pub fn load_or_generate(
        configured: Option<(&Path, &Path)>,
        directory: &Path,
    ) -> Result<Self, TlsError> {
        if let Some((certificate, key)) = configured {
            return Self::load(certificate, key);
        }
        let certificate = directory.join("certificate.pem");
        let key = directory.join("private-key.pem");
        if certificate.exists() && key.exists() {
            return Self::load(&certificate, &key);
        }
        let generated = rcgen::generate_simple_self_signed(vec!["Nuschl server".into()])
            .map_err(|error| TlsError::Generate(error.to_string()))?;
        write_atomically(
            &key,
            generated.signing_key.serialize_pem().as_bytes(),
            0o600,
        )?;
        write_atomically(&certificate, generated.cert.pem().as_bytes(), 0o644)?;
        tracing::info!(path = %certificate.display(), "generated a self-signed server certificate");
        Self::load(&certificate, &key)
    }

    /// What a SIGHUP reloads: the configured files, or the generated ones,
    /// which are never generated again here (a missing file is an error).
    pub fn reload(configured: Option<(&Path, &Path)>, directory: &Path) -> Result<Self, TlsError> {
        match configured {
            Some((certificate, key)) => Self::load(certificate, key),
            None => Self::load(
                &directory.join("certificate.pem"),
                &directory.join("private-key.pem"),
            ),
        }
    }

    pub fn fingerprint(&self) -> String {
        sha256_hex(self.chain[0].as_ref())
    }
}

/// Whether a certificate and key can serve TLS.
pub fn check_identity(identity: Identity) -> Result<(), TlsError> {
    identity.certified().map(|_| ())
}

/// The server certificate, replaceable while running.
#[derive(Debug)]
pub struct Certificates {
    current: RwLock<Arc<CertifiedKey>>,
}

impl Certificates {
    pub fn new(identity: Identity) -> Result<Self, TlsError> {
        Ok(Self {
            current: RwLock::new(identity.certified()?),
        })
    }

    fn current(&self) -> Arc<CertifiedKey> {
        self.current
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Use `identity` for new handshakes; on error the current one stays.
    pub fn replace(&self, identity: Identity) -> Result<String, TlsError> {
        let certified = identity.certified()?;
        let fingerprint = certified
            .end_entity_cert()
            .map(|leaf| sha256_hex(leaf.as_ref()))?;
        *self
            .current
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = certified;
        Ok(fingerprint)
    }

    /// SHA-256 of the current leaf certificate, lowercase hex.
    pub fn fingerprint(&self) -> String {
        self.current()
            .end_entity_cert()
            .map(|leaf| sha256_hex(leaf.as_ref()))
            .unwrap_or_default()
    }
}

impl ResolvesServerCert for Certificates {
    fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.current())
    }
}

fn write_atomically(path: &Path, contents: &[u8], mode: u32) -> Result<(), TlsError> {
    let error = |source| TlsError::Write {
        path: path.to_owned(),
        source,
    };
    let temporary = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, mode);
    #[cfg(not(unix))]
    let _ = mode;
    let mut file = options.open(&temporary).map_err(error)?;
    file.write_all(contents).map_err(error)?;
    file.sync_all().map_err(error)?;
    std::fs::rename(&temporary, path).map_err(error)
}

/// Accept any client certificate (Mumble identities are self-signed); the
/// handshake signature still proves the client holds its private key.
#[derive(Debug)]
struct AnyClientCertificate(Arc<CryptoProvider>);

impl ClientCertVerifier for AnyClientCertificate {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        false
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            signature,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            signature,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Which certificate authorities make a client certificate "strong" (Murmur's
/// `bVerified`): the ACL group `strong` matches it and `UserStats` says so.
/// Murmur verifies the chain against the system's authorities and murmur.ini's
/// `sslCA`, and counts a certificate as strong unless the chain is self-signed,
/// leads to no known authority, or is expired or not yet valid; any purpose
/// is fine (an e-mail certificate, say). Here the authorities are configured
/// (`strong_certificate_authorities`) and none by default, so nobody is strong
/// unless the operator says whom to trust. The chain is the one the client
/// presented in a handshake that proved it holds the leaf's key; strength is
/// decided once, at login.
#[derive(Clone)]
pub struct Authorities {
    roots: Arc<Vec<TrustAnchor<'static>>>,
    algorithms: &'static [&'static dyn SignatureVerificationAlgorithm],
}

impl Default for Authorities {
    fn default() -> Self {
        Self {
            roots: Arc::default(),
            algorithms: rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .all,
        }
    }
}

impl fmt::Debug for Authorities {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Authorities({} roots)", self.roots.len())
    }
}

/// Where systems keep their bundle of trusted authorities, after
/// `SSL_CERT_FILE` (NixOS and Debian, Fedora, openSUSE, Alpine).
const SYSTEM_BUNDLES: [&str; 5] = [
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/ca-bundle.pem",
    "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
    "/etc/ssl/cert.pem",
];

impl Authorities {
    /// `"webpki"` (Mozilla's roots, built in), `"system"` (`SSL_CERT_FILE` or
    /// the system's bundle) or the path of a PEM file of authorities. A file
    /// named explicitly must hold only usable certificates; entries of the
    /// system bundle that cannot serve as an authority are skipped.
    pub fn load(sources: &[String]) -> Result<Self, String> {
        let mut roots = Vec::new();
        for source in sources {
            match source.as_str() {
                "webpki" => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
                "system" => {
                    let bundle = std::env::var_os("SSL_CERT_FILE")
                        .map(PathBuf::from)
                        .into_iter()
                        .chain(SYSTEM_BUNDLES.into_iter().map(PathBuf::from))
                        .find(|path| path.is_file())
                        .ok_or("no system certificate bundle (set SSL_CERT_FILE)")?;
                    roots.extend(read_anchors(&bundle, false)?);
                }
                path => roots.extend(read_anchors(Path::new(path), true)?),
            }
        }
        Ok(Self {
            roots: Arc::new(roots),
            ..Self::default()
        })
    }

    /// Whether `chain` (leaf first, DER, as presented) leads from a valid leaf
    /// to one of these authorities at `now`.
    pub fn strong(&self, chain: &[Vec<u8>], now: UnixTime) -> bool {
        let Some((leaf, intermediates)) = chain.split_first() else {
            return false;
        };
        if self.roots.is_empty() {
            return false;
        }
        let leaf = CertificateDer::from(leaf.as_slice());
        let Ok(leaf) = webpki::EndEntityCert::try_from(&leaf) else {
            return false;
        };
        let intermediates: Vec<CertificateDer<'_>> = intermediates
            .iter()
            .map(|der| CertificateDer::from(der.as_slice()))
            .collect();
        leaf.verify_for_usage(
            self.algorithms,
            &self.roots,
            &intermediates,
            now,
            AnyPurpose,
            None,
            None,
        )
        .is_ok()
    }
}

/// Murmur accepts a certificate issued for any purpose (Qt's
/// `InvalidPurpose` does not clear `bVerified`: "allow email certificates").
struct AnyPurpose;

impl webpki::ExtendedKeyUsageValidator for AnyPurpose {
    fn validate(&self, purposes: webpki::KeyPurposeIdIter<'_, '_>) -> Result<(), webpki::Error> {
        for purpose in purposes {
            purpose?;
        }
        Ok(())
    }
}

/// The authorities in a PEM file; `strict`: every certificate must be one.
fn read_anchors(path: &Path, strict: bool) -> Result<Vec<TrustAnchor<'static>>, String> {
    let fail = |error: &dyn fmt::Display| format!("{}: {error}", path.display());
    let mut anchors = Vec::new();
    for certificate in CertificateDer::pem_file_iter(path).map_err(|error| fail(&error))? {
        let certificate = certificate.map_err(|error| fail(&error))?;
        match webpki::anchor_from_trusted_cert(&certificate) {
            Ok(anchor) => anchors.push(anchor.to_owned()),
            Err(error) if strict => return Err(fail(&error)),
            Err(_) => {}
        }
    }
    if anchors.is_empty() {
        return Err(fail(&"no certificate authority in it"));
    }
    Ok(anchors)
}

/// The TLS configuration and its replaceable certificate. With
/// `tls13_only`, TLS 1.2 handshakes are refused (in 1.2 the client
/// certificate, a stable identity, crosses the network in the clear; stock
/// clients from 1.2.x on speak 1.3 when built against OpenSSL 1.1.1 or later).
pub fn server_config(
    identity: Identity,
    tls13_only: bool,
) -> Result<(Arc<rustls::ServerConfig>, Arc<Certificates>), TlsError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let certificates = Arc::new(Certificates::new(identity)?);
    let builder = rustls::ServerConfig::builder_with_provider(provider.clone());
    let builder = if tls13_only {
        builder.with_protocol_versions(&[&rustls::version::TLS13])?
    } else {
        builder.with_safe_default_protocol_versions()?
    };
    let mut config = builder
        .with_client_cert_verifier(Arc::new(AnyClientCertificate(provider)))
        .with_cert_resolver(certificates.clone());
    config.alpn_protocols = vec![ALPN_NUSCHL.to_vec(), ALPN_MUMBLE.to_vec()];
    // Session resumption would let a client skip presenting its certificate.
    config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    config.send_tls13_tickets = 0;
    Ok((Arc::new(config), certificates))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose,
        IsCa, Issuer, KeyPair, KeyUsagePurpose,
    };

    fn authority_params(name: &str) -> CertificateParams {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name.push(DnType::CommonName, name);
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params
    }

    fn root(name: &str) -> CertifiedIssuer<'static, KeyPair> {
        CertifiedIssuer::self_signed(authority_params(name), KeyPair::generate().unwrap()).unwrap()
    }

    fn leaf(issuer: &Issuer<'_, KeyPair>, edit: impl FnOnce(&mut CertificateParams)) -> Vec<u8> {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name.push(DnType::CommonName, "Alice");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        edit(&mut params);
        let key = KeyPair::generate().unwrap();
        params.signed_by(&key, issuer).unwrap().der().to_vec()
    }

    #[test]
    fn strong_certificates_chain_to_a_configured_authority() {
        let ca = root("Test CA");
        let other = root("Other CA");
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("ca.pem");
        std::fs::write(&file, ca.pem()).unwrap();
        let authorities = Authorities::load(&[file.display().to_string()]).unwrap();
        let now = UnixTime::now();
        let client = leaf(&ca, |_| {});
        assert!(authorities.strong(std::slice::from_ref(&client), now));
        // Any purpose, as in Murmur (an e-mail certificate, a server's).
        for purpose in [
            ExtendedKeyUsagePurpose::EmailProtection,
            ExtendedKeyUsagePurpose::ServerAuth,
        ] {
            let certificate = leaf(&ca, |params| params.extended_key_usages = vec![purpose]);
            assert!(authorities.strong(&[certificate], now));
        }
        // Self-signed (every Mumble identity), another authority's, expired.
        let own = rcgen::generate_simple_self_signed(vec!["Mumble User".into()]).unwrap();
        assert!(!authorities.strong(&[own.cert.der().to_vec()], now));
        assert!(!authorities.strong(&[leaf(&other, |_| {})], now));
        let expired = leaf(&ca, |params| {
            params.not_before = rcgen::date_time_ymd(2000, 1, 1);
            params.not_after = rcgen::date_time_ymd(2001, 1, 1);
        });
        assert!(!authorities.strong(&[expired], now));
        // Through an intermediate the client presents, not without it.
        let intermediate = CertifiedIssuer::signed_by(
            authority_params("Intermediate"),
            KeyPair::generate().unwrap(),
            &ca,
        )
        .unwrap();
        let through = leaf(&intermediate, |_| {});
        assert!(authorities.strong(&[through.clone(), intermediate.der().to_vec()], now));
        assert!(!authorities.strong(&[through], now));
        // An intermediate as the leaf is not a client certificate.
        assert!(!authorities.strong(&[intermediate.der().to_vec()], now));
        // Nothing configured, nothing presented: nobody is strong.
        assert!(!Authorities::default().strong(std::slice::from_ref(&client), now));
        assert!(!Authorities::load(&[]).unwrap().strong(&[client], now));
        assert!(!authorities.strong(&[], now));
        assert!(!authorities.strong(&[b"not a certificate".to_vec()], now));
    }

    #[test]
    fn authority_sources_are_checked() {
        assert!(Authorities::load(&["webpki".into()]).unwrap().roots.len() > 50);
        assert!(Authorities::load(&["/nonexistent/authorities.pem".into()]).is_err());
        let directory = tempfile::tempdir().unwrap();
        let empty = directory.path().join("empty.pem");
        std::fs::write(&empty, "no certificate here\n").unwrap();
        assert!(Authorities::load(&[empty.display().to_string()]).is_err());
        // An explicit file must hold only authorities; a broken entry is an error.
        let broken = directory.path().join("broken.pem");
        std::fs::write(
            &broken,
            format!(
                "{}-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
                root("Test CA").pem()
            ),
        )
        .unwrap();
        assert!(Authorities::load(&[broken.display().to_string()]).is_err());
    }
}
