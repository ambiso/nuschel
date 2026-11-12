// SPDX-License-Identifier: EUPL-1.2
use crate::{
    Error,
    config::{ClientIdentity, ServerBookmark, fingerprint},
};
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::{
        WebPkiServerVerifier,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    },
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
};
use std::sync::{Arc, Mutex};
use tokio::{
    net::TcpStream,
    time::{Duration, timeout},
};
use tokio_rustls::{TlsConnector, client::TlsStream};

#[derive(Clone, Debug)]
struct Observed {
    fingerprint: String,
    rejected: bool,
}

#[derive(Debug)]
struct Verifier {
    pin: Option<String>,
    observed: Arc<Mutex<Option<Observed>>>,
    webpki: Arc<WebPkiServerVerifier>,
    provider: Arc<CryptoProvider>,
}
impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let actual = fingerprint(end_entity.as_ref());
        let result = if let Some(pin) = &self.pin {
            if pin.eq_ignore_ascii_case(&actual) {
                Ok(ServerCertVerified::assertion())
            } else {
                Err(rustls::Error::General(
                    "the pinned server certificate has changed".into(),
                ))
            }
        } else {
            self.webpki
                .verify_server_cert(end_entity, intermediates, name, ocsp, now)
        };
        *self
            .observed
            .lock()
            .map_err(|_| rustls::Error::General("certificate state unavailable".into()))? =
            Some(Observed {
                fingerprint: actual,
                rejected: result.is_err(),
            });
        result
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
            &self.provider.signature_verification_algorithms,
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
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

pub async fn connect(
    server: &ServerBookmark,
    identity: ClientIdentity,
) -> Result<(TlsStream<TcpStream>, String, std::net::SocketAddr), Error> {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    connect_with_roots(server, identity, roots).await
}

// Private injection point for local PKI fixtures; production always uses the
// compiled WebPKI roots above, and never an unverified per-server root.
async fn connect_with_roots(
    server: &ServerBookmark,
    identity: ClientIdentity,
    roots: rustls::RootCertStore,
) -> Result<(TlsStream<TcpStream>, String, std::net::SocketAddr), Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let webpki = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .map_err(|e| Error::Tls(e.to_string()))?;
    let observed = Arc::new(Mutex::new(None));
    let verifier = Verifier {
        pin: server.certificate_sha256.clone(),
        observed: observed.clone(),
        webpki,
        provider: provider.clone(),
    };
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Tls(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_client_auth_cert(
            vec![CertificateDer::from(identity.certificate)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.private_key)),
        )
        .map_err(|e| Error::Tls(e.to_string()))?;
    // Offer the Nuschl extension and plain Mumble in one handshake (stock
    // servers ignore ALPN). The offer is covered by the handshake transcript,
    // so it cannot be stripped without failing the connection.
    let mut config = config;
    config.alpn_protocols = vec![
        nuschl_protocol::nuschl::ALPN.to_vec(),
        nuschl_protocol::nuschl::ALPN_MUMBLE.to_vec(),
    ];
    config.resumption = rustls::client::Resumption::disabled();
    let socket = timeout(
        Duration::from_secs(10),
        TcpStream::connect((server.host.as_str(), server.port)),
    )
    .await
    .map_err(|_| Error::Timeout("connecting"))?
    .map_err(|error| match error.kind() {
        std::io::ErrorKind::ConnectionRefused => Error::Refused {
            host: server.host.clone(),
            port: server.port,
        },
        _ => error.into(),
    })?;
    socket.set_nodelay(true)?;
    let peer_addr = socket.peer_addr()?;
    let name = ServerName::try_from(server.host.clone())
        .map_err(|_| Error::Tls("invalid TLS server name".into()))?;
    let result = timeout(
        Duration::from_secs(10),
        TlsConnector::from(Arc::new(config)).connect(name, socket),
    )
    .await
    .map_err(|_| Error::Timeout("TLS handshake"))?;
    let actual = observed
        .lock()
        .map_err(|_| Error::Tls("certificate state unavailable".into()))?
        .clone();
    match result {
        Ok(stream) => Ok((
            stream,
            actual
                .ok_or_else(|| Error::Tls("server did not present a certificate".into()))?
                .fingerprint,
            peer_addr,
        )),
        Err(error) => {
            if let Some(observed) = actual.filter(|certificate| certificate.rejected) {
                // No Authenticate frame or password is sent until TLS succeeds.
                Err(Error::Certificate {
                    sha256: observed.fingerprint,
                    changed: server.certificate_sha256.is_some(),
                    reason: error.to_string(),
                })
            } else {
                Err(Error::Tls(error.to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa,
        KeyPair, KeyUsagePurpose,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn ca(name: &str) -> CertifiedIssuer<'static, KeyPair> {
        let mut p = CertificateParams::new(vec![]).unwrap();
        p.distinguished_name.push(rcgen::DnType::CommonName, name);
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        CertifiedIssuer::self_signed(p, KeyPair::generate().unwrap()).unwrap()
    }
    fn leaf(
        ca: &rcgen::Issuer<'_, KeyPair>,
        host: &str,
        expired: bool,
    ) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
        let mut p = CertificateParams::new(vec![host.into()]).unwrap();
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        if expired {
            p.not_before = rcgen::date_time_ymd(2000, 1, 1);
            p.not_after = rcgen::date_time_ymd(2001, 1, 1);
        }
        let key = KeyPair::generate().unwrap();
        (
            p.signed_by(&key, ca).unwrap().der().clone(),
            PrivateKeyDer::Pkcs8(key.serialize_der().into()),
        )
    }
    fn roots(ca: &CertifiedIssuer<'static, KeyPair>) -> rustls::RootCertStore {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca.der().clone()).unwrap();
        roots
    }
    fn server(
        chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
    ) -> Arc<rustls::ServerConfig> {
        Arc::new(
            rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .unwrap(),
        )
    }
    fn identity() -> ClientIdentity {
        let c = rcgen::generate_simple_self_signed(vec!["private-client.test".into()]).unwrap();
        ClientIdentity {
            certificate: c.cert.der().to_vec(),
            private_key: c.signing_key.serialize_der(),
        }
    }

    #[tokio::test]
    async fn private_ca_fullchain_renews_without_pins_and_old_streams_keep_working() {
        let ca = ca("private root");
        let mut p = CertificateParams::new(vec![]).unwrap();
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let intermediate =
            CertifiedIssuer::signed_by(p, KeyPair::generate().unwrap(), &ca).unwrap();
        let (first, key) = leaf(&intermediate, "localhost", false);
        let current = Arc::new(std::sync::RwLock::new(server(
            vec![first.clone(), intermediate.der().clone()],
            key,
        )));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let selected = current.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let config = selected.read().unwrap().clone();
                tokio::spawn(async move {
                    let Ok(mut stream) =
                        tokio_rustls::TlsAcceptor::from(config).accept(socket).await
                    else {
                        return;
                    };
                    let mut buffer = [0; 4];
                    while stream.read_exact(&mut buffer).await.is_ok() {
                        if stream.write_all(&buffer).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        let mut bookmark = ServerBookmark::new("private", "localhost", port, "fixture").unwrap();
        let (mut old, pin, _) = connect_with_roots(&bookmark, identity(), roots(&ca))
            .await
            .unwrap();
        assert_eq!(pin, fingerprint(first.as_ref()));
        assert!(bookmark.certificate_sha256.is_none());
        let (renewed, key) = leaf(&intermediate, "localhost", false);
        *current.write().unwrap() = server(vec![renewed.clone(), intermediate.der().clone()], key);
        let (_, new_pin, _) = connect_with_roots(&bookmark, identity(), roots(&ca))
            .await
            .unwrap();
        assert_ne!(pin, new_pin);
        assert!(bookmark.certificate_sha256.is_none());
        old.write_all(b"keep").await.unwrap();
        let mut reply = [0; 4];
        old.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"keep");
        bookmark.certificate_sha256 = Some(pin.clone());
        assert!(matches!(
            connect_with_roots(&bookmark, identity(), roots(&ca)).await,
            Err(Error::Certificate { changed: true, .. })
        ));
        bookmark.certificate_sha256 = None;
        for (host, expired, with_intermediate) in [
            ("wrong.test", false, true),
            ("localhost", true, true),
            ("localhost", false, false),
        ] {
            let (cert, key) = leaf(&intermediate, host, expired);
            let mut chain = vec![cert];
            if with_intermediate {
                chain.push(intermediate.der().clone());
            }
            *current.write().unwrap() = server(chain, key);
            assert!(matches!(
                connect_with_roots(&bookmark, identity(), roots(&ca)).await,
                Err(Error::Certificate { .. })
            ));
            old.write_all(b"safe").await.unwrap();
            old.read_exact(&mut reply).await.unwrap();
            assert_eq!(&reply, b"safe");
        }
        let other = self::ca("untrusted root");
        let (cert, key) = leaf(&other, "localhost", false);
        *current.write().unwrap() = server(vec![cert], key);
        assert!(matches!(
            connect_with_roots(&bookmark, identity(), roots(&ca)).await,
            Err(Error::Certificate { .. })
        ));
        task.abort();
    }
}
