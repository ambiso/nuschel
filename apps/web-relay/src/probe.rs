// SPDX-License-Identifier: EUPL-1.2
//! A client without a browser: through the relay's tunnel, TLS to a Mumble
//! server and a login up to `ServerSync`, as the web client will do with rustls
//! in WebAssembly. For the loopback test and the vm-web check; the TLS session
//! runs here, so the relay only sees ciphertext.

use futures_util::{SinkExt, StreamExt};
use nuschl_protocol::{
    control as proto, decode_control, encode_control, kind,
    tunnel::{self, Frame},
};
use prost::Message as _;
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use sha2::{Digest, Sha256};
use std::{
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};

pub struct Probe {
    /// `ws://` or `wss://` URL of the relay.
    pub relay: String,
    /// Sent as the `Origin` header, as a browser would.
    pub origin: Option<String>,
    pub host: String,
    pub port: u16,
    pub username: String,
    /// The server certificate's SHA-256 (hex); without one any certificate is
    /// accepted and reported.
    pub pin: Option<String>,
}

#[derive(Debug)]
pub struct Outcome {
    /// Datagrams reach the server over UDP.
    pub udp: bool,
    /// The server answered a ping datagram through the tunnel.
    pub udp_answered: bool,
    pub fingerprint: String,
    pub release: Option<String>,
    pub session: u32,
    pub welcome: Option<String>,
}

#[derive(Debug)]
struct Pin {
    expected: Option<String>,
    seen: Mutex<Option<String>>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pin {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let actual = hex::encode(Sha256::digest(end_entity.as_ref()));
        *self.seen.lock().unwrap_or_else(PoisonError::into_inner) = Some(actual.clone());
        match &self.expected {
            Some(pin) if !pin.eq_ignore_ascii_case(&actual) => Err(rustls::Error::General(
                "the server certificate does not match the pin".into(),
            )),
            _ => Ok(ServerCertVerified::assertion()),
        }
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

pub async fn run(probe: Probe) -> Result<Outcome, String> {
    tokio::time::timeout(Duration::from_secs(30), run_inner(probe))
        .await
        .map_err(|_| "timed out".to_string())?
}

async fn run_inner(probe: Probe) -> Result<Outcome, String> {
    let mut request = probe
        .relay
        .as_str()
        .into_client_request()
        .map_err(|error| format!("relay URL: {error}"))?;
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static(tunnel::SUBPROTOCOL),
    );
    if let Some(origin) = &probe.origin {
        request.headers_mut().insert(
            "Origin",
            HeaderValue::from_str(origin).map_err(|_| "invalid origin")?,
        );
    }
    let (ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|error| format!("relay: {error}"))?;
    let (mut sink, mut source) = ws.split();
    let open = tunnel::encode(&Frame::Open {
        host: &probe.host,
        port: probe.port,
    })
    .ok_or("invalid host")?;
    sink.send(Message::binary(open))
        .await
        .map_err(|error| format!("relay: {error}"))?;
    let udp = loop {
        match source.next().await {
            Some(Ok(Message::Binary(bytes))) => match tunnel::decode(&bytes) {
                Ok(Frame::Opened { udp }) => break udp,
                Ok(Frame::Closed(reason)) => return Err(format!("refused: {reason:?}")),
                _ => return Err("unexpected frame before Opened".into()),
            },
            Some(Ok(_)) => continue,
            _ => return Err("the relay closed the connection".into()),
        }
    };

    // The TLS connection runs over a pipe whose other end is the tunnel.
    let (local, remote) = tokio::io::duplex(256 * 1024);
    let (mut remote_read, mut remote_write) = tokio::io::split(remote);
    let (datagrams_tx, mut datagrams_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    // Why the relay closed the tunnel, if it said.
    let closed = Arc::new(Mutex::new(None));
    let closed_by = closed.clone();
    let why = move |error: String| match *closed.lock().unwrap_or_else(PoisonError::into_inner) {
        Some(reason) => format!("{error} (relay: {reason:?})"),
        None => error,
    };
    let (outgoing_tx, mut outgoing_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let uplink = tokio::spawn(async move {
        let mut buffer = vec![0_u8; 16 * 1024];
        loop {
            tokio::select! {
                read = remote_read.read(&mut buffer) => {
                    let Ok(size @ 1..) = read else { break };
                    let frame = tunnel::encode(&Frame::Stream(&buffer[..size])).expect("bounded");
                    if sink.send(Message::binary(frame)).await.is_err() {
                        break;
                    }
                }
                Some(datagram) = outgoing_rx.recv() => {
                    let Some(frame) = tunnel::encode(&Frame::Datagram(&datagram)) else { continue };
                    if sink.send(Message::binary(frame)).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    let downlink = tokio::spawn(async move {
        while let Some(Ok(message)) = source.next().await {
            let Message::Binary(bytes) = message else {
                continue;
            };
            match tunnel::decode(&bytes) {
                Ok(Frame::Stream(data)) => {
                    if remote_write.write_all(data).await.is_err() {
                        break;
                    }
                }
                Ok(Frame::Datagram(data)) => {
                    let _ = datagrams_tx.try_send(data.to_vec());
                }
                Ok(Frame::Closed(reason)) => {
                    *closed_by.lock().unwrap_or_else(PoisonError::into_inner) = Some(reason);
                    break;
                }
                _ => break,
            }
        }
        // The TLS side reads the end of the tunnel as the end of its stream.
        let _ = remote_write.shutdown().await;
    });

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(Pin {
        expected: probe.pin.clone(),
        seen: Mutex::new(None),
        provider: provider.clone(),
    });
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| error.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_no_client_auth();
    config.resumption = rustls::client::Resumption::disabled();
    let name = ServerName::try_from(probe.host.clone()).map_err(|_| "invalid server name")?;
    let mut tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, local)
        .await
        .map_err(|error| why(format!("TLS: {error}")))?;
    let fingerprint = verifier
        .seen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .ok_or("no certificate")?;

    let version = proto::Version {
        version_v1: Some(0x0001_0500),
        version_v2: Some(nuschl_protocol::voice::PROTOBUF_VERSION),
        release: Some(format!(
            "Nuschl web relay probe {}",
            env!("CARGO_PKG_VERSION")
        )),
        os: Some("probe".into()),
        ..Default::default()
    };
    let authenticate = proto::Authenticate {
        username: Some(probe.username.clone()),
        opus: Some(true),
        ..Default::default()
    };
    for frame in [
        encode_control(kind::VERSION, &version),
        encode_control(kind::AUTHENTICATE, &authenticate),
    ] {
        tls.write_all(&frame.ok_or("frame too large")?)
            .await
            .map_err(|error| format!("TLS write: {error}"))?;
    }
    let mut buffer = bytes::BytesMut::with_capacity(64 * 1024);
    let mut release = None;
    let (session, welcome) = 'login: loop {
        if tls
            .read_buf(&mut buffer)
            .await
            .map_err(|error| why(format!("TLS read: {error}")))?
            == 0
        {
            return Err(why("the server closed the connection".into()));
        }
        if buffer.len() > 8 * 1024 * 1024 {
            return Err("too much data before ServerSync".into());
        }
        while let Some(frame) = decode_control(&mut buffer).map_err(|error| error.to_string())? {
            match frame.kind {
                kind::VERSION => {
                    release = proto::Version::decode(frame.payload)
                        .ok()
                        .and_then(|version| version.release);
                }
                kind::REJECT => {
                    let reject = proto::Reject::decode(frame.payload).unwrap_or_default();
                    return Err(format!("rejected: {}", reject.reason.unwrap_or_default()));
                }
                kind::SERVER_SYNC => {
                    let sync = proto::ServerSync::decode(frame.payload)
                        .map_err(|error| error.to_string())?;
                    break 'login (sync.session.unwrap_or_default(), sync.welcome_text);
                }
                _ => {}
            }
        }
    };

    // A datagram through the tunnel: the unencrypted ping every Mumble server
    // answers, from the relay's UDP socket.
    let mut udp_answered = false;
    if udp {
        let ident = 0x6e75_7363_6865_6c21;
        for _ in 0..3 {
            let _ = outgoing_tx.send(tunnel::mumble_ping(ident).to_vec()).await;
            if let Ok(Some(reply)) =
                tokio::time::timeout(Duration::from_millis(700), datagrams_rx.recv()).await
                && tunnel::is_ping_reply(&reply, ident)
            {
                udp_answered = true;
                break;
            }
        }
    }
    let _ = tls.shutdown().await;
    uplink.abort();
    downlink.abort();
    Ok(Outcome {
        udp,
        udp_answered,
        fingerprint,
        release,
        session,
        welcome,
    })
}
