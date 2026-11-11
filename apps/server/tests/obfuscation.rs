// SPDX-License-Identifier: EUPL-1.2
//! `obfuscate = true` keeps client addresses out of the log. Its own test
//! binary: the log is captured with a thread-local subscriber, which tests on
//! other threads (with callsites cached as disabled) would interfere with.
use bytes::BytesMut;
use nuschel_protocol::{control as proto, decode_control, encode_control, kind};
use nuscheld::{
    config::Config,
    server::start,
    storage::{Backend, MemoryStore},
};
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};
use tokio_rustls::{TlsConnector, client::TlsStream};

#[derive(Debug)]
struct AnyServer(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for AnyServer {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
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
        rustls::crypto::verify_tls13_signature(
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

async fn tls(address: SocketAddr) -> std::io::Result<TlsStream<TcpStream>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyServer(provider)))
        .with_no_client_auth();
    let stream = TcpStream::connect(address).await?;
    TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("localhost").unwrap(), stream)
        .await
}

/// Log in; the kind of the answer that ends the login (`ServerSync` or `Reject`).
async fn login(address: SocketAddr, name: &str, password: &str) -> Option<u16> {
    let mut stream = tls(address).await.ok()?;
    let version = proto::Version {
        version_v2: Some((1 << 48) | (5 << 32)),
        ..Default::default()
    };
    let authenticate = proto::Authenticate {
        username: Some(name.into()),
        password: Some(password.into()),
        opus: Some(true),
        ..Default::default()
    };
    let mut frames = encode_control(kind::VERSION, &version)?.to_vec();
    frames.extend_from_slice(&encode_control(kind::AUTHENTICATE, &authenticate)?);
    stream.write_all(&frames).await.ok()?;
    let mut buffer = BytesMut::new();
    loop {
        while let Ok(Some(frame)) = decode_control(&mut buffer) {
            if frame.kind == kind::SERVER_SYNC || frame.kind == kind::REJECT {
                return Some(frame.kind);
            }
        }
        match timeout(Duration::from_secs(10), stream.read_buf(&mut buffer)).await {
            Ok(Ok(n)) if n > 0 => {}
            _ => return None,
        }
    }
}

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Logins, rejections and the autoban log tokens instead of addresses with
/// `obfuscate = true`, and addresses without it.
#[tokio::test]
async fn obfuscated_logs_contain_no_client_addresses() {
    for obfuscate in [false, true] {
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let directory = tempfile::tempdir().unwrap();
        let password = directory.path().join("superuser");
        std::fs::write(&password, "secret").unwrap();
        let config = Config::from_toml(&format!(
            "bind = [\"127.0.0.1\"]\nport = 0\nstate_directory = {:?}\n\
             superuser_password_file = {password:?}\nobfuscate = {obfuscate}\n\
             [autoban]\nattempts = 3\ntimeframe = 60\nduration = 60\n\
             [password_hashing]\nmemory_kib = 64\npasses = 1\n",
            directory.path()
        ))
        .unwrap();
        let running = start(
            config.settings().unwrap(),
            Backend::Memory(MemoryStore::shared()),
        )
        .await
        .unwrap();
        let address = running.addresses[0];
        assert_eq!(
            login(address, "SuperUser", "secret").await,
            Some(kind::SERVER_SYNC)
        );
        assert_eq!(
            login(address, "SuperUser", "wrong").await,
            Some(kind::REJECT)
        );
        // Past the autoban threshold.
        for _ in 0..3 {
            let _ = tls(address).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        running.shutdown().await;
        let text = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        for event in ["authenticated", "rejected", "autoban"] {
            assert!(text.contains(event), "{event} in {text}");
        }
        let leaked: Vec<&str> = text
            .lines()
            .filter(|line| !line.contains("listening on") && line.contains("127.0.0.1"))
            .collect();
        if obfuscate {
            assert!(leaked.is_empty(), "{leaked:?}");
            assert!(text.contains("address=<<"), "{text}");
        } else {
            assert!(!leaked.is_empty(), "{text}");
        }
    }
}
