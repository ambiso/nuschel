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
use rustls::{
    DigitallySignedStruct, DistinguishedName, SignatureScheme,
    client::danger::HandshakeSignatureValid,
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, PrivateKeyDer, UnixTime, pem::PemObject},
    server::{
        ClientHello, ResolvesServerCert,
        danger::{ClientCertVerified, ClientCertVerifier},
    },
    sign::CertifiedKey,
};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

/// ALPN for the Nuschel extension (docs/extension.md); `mumble` is accepted too,
/// because rustls refuses a client whose offer shares nothing with ours. Stock
/// clients offer no ALPN at all.
use nuschel_protocol::nuschel::{ALPN as ALPN_NUSCHEL, ALPN_MUMBLE};

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
    hex(ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, der).as_ref())
}

pub fn sha256_hex(der: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, der).as_ref())
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
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
        let generated = rcgen::generate_simple_self_signed(vec!["Nuschel server".into()])
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
    config.alpn_protocols = vec![ALPN_NUSCHEL.to_vec(), ALPN_MUMBLE.to_vec()];
    // Session resumption would let a client skip presenting its certificate.
    config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    config.send_tls13_tickets = 0;
    Ok((Arc::new(config), certificates))
}
