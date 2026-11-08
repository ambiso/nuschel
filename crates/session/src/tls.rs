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
) -> Result<(TlsStream<TcpStream>, String), Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
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
    let socket = timeout(
        Duration::from_secs(10),
        TcpStream::connect((server.host.as_str(), server.port)),
    )
    .await
    .map_err(|_| Error::Timeout("connecting"))??;
    socket.set_nodelay(true)?;
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
