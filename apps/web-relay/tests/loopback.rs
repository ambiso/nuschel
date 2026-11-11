// SPDX-License-Identifier: EUPL-1.2
//! The relay on loopback: a browser-less client (TLS on its side, as rustls in
//! WebAssembly will be) logs in to a Mumble server through it, and the bytes
//! that crossed the relay are recorded at the server to show they were
//! ciphertext only. Then everything the relay must refuse.
//!
//! With `NUSCHEL_TEST_MURMUR=/path/to/mumble-server` the login also runs
//! against a real Murmur on loopback.

use futures_util::{SinkExt, StreamExt};
use nuschel_protocol::{
    control as proto, decode_control, encode_control, kind,
    tunnel::{self, CloseReason, Frame},
};
use nuschel_web_relay::{
    Config, Relay,
    config::Ports,
    probe::{self, Probe},
};
use prost::Message as _;
use sha2::{Digest, Sha256};
use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::{TcpListener, UdpSocket},
};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};

const USERNAME: &str = "relay-probe-secret-name";
const WELCOME: &str = "Welcome text only the client may read";

/// Everything that crossed the TCP connection, both ways, before TLS.
#[derive(Clone, Default)]
struct Recording {
    from_client: Arc<Mutex<Vec<u8>>>,
    to_client: Arc<Mutex<Vec<u8>>>,
}

struct Recorded<S> {
    inner: S,
    recording: Recording,
}

impl<S: AsyncRead + Unpin> AsyncRead for Recorded<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        let new = buf.filled()[before..].to_vec();
        self.recording.from_client.lock().expect("lock").extend(new);
        poll
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Recorded<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write(cx, data);
        if let Poll::Ready(Ok(size)) = poll {
            self.recording
                .to_client
                .lock()
                .expect("lock")
                .extend_from_slice(&data[..size]);
        }
        poll
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// A minimal Mumble server: TLS with a self-signed certificate, `Version` and
/// `ServerSync` after the client's `Authenticate`, and the unencrypted UDP
/// ping on the same port (unless `udp` is false).
async fn mumble_server(udp: bool) -> (u16, String, Recording) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("cert");
    let certificate = certified.cert.der().clone();
    let fingerprint = hex(&Sha256::digest(certificate.as_ref()));
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(certified.signing_key.serialize_der().into());
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("versions")
    .with_no_client_auth()
    .with_single_cert(vec![certificate], key)
    .expect("config");
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("address").port();
    if udp {
        let socket = UdpSocket::bind(("127.0.0.1", port)).await.expect("udp");
        tokio::spawn(async move {
            let mut buffer = [0_u8; 1024];
            while let Ok((size, from)) = socket.recv_from(&mut buffer).await {
                if size == 12 && buffer[..4] == [0; 4] {
                    let mut reply = [0_u8; 24];
                    reply[..4].copy_from_slice(&0x0001_0500_u32.to_be_bytes());
                    reply[4..12].copy_from_slice(&buffer[4..12]);
                    let _ = socket.send_to(&reply, from).await;
                }
            }
        });
    }
    let recording = Recording::default();
    let record = recording.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            let recording = record.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor
                    .accept(Recorded {
                        inner: stream,
                        recording,
                    })
                    .await
                else {
                    return;
                };
                let mut buffer = bytes::BytesMut::new();
                let mut authenticated = false;
                while !authenticated {
                    if tls.read_buf(&mut buffer).await.unwrap_or(0) == 0 {
                        return;
                    }
                    while let Ok(Some(frame)) = decode_control(&mut buffer) {
                        if frame.kind == kind::AUTHENTICATE {
                            let login =
                                proto::Authenticate::decode(frame.payload).expect("authenticate");
                            assert_eq!(login.username.as_deref(), Some(USERNAME));
                            authenticated = true;
                        }
                    }
                }
                let version = proto::Version {
                    version_v2: Some(nuschel_protocol::voice::PROTOBUF_VERSION),
                    release: Some("fake murmur".into()),
                    ..Default::default()
                };
                let sync = proto::ServerSync {
                    session: Some(42),
                    welcome_text: Some(WELCOME.into()),
                    ..Default::default()
                };
                for frame in [
                    encode_control(kind::VERSION, &version),
                    encode_control(kind::SERVER_SYNC, &sync),
                ] {
                    let _ = tls.write_all(&frame.expect("frame")).await;
                }
                // Keep the session until the client leaves.
                let _ = tls.read_buf(&mut buffer).await;
            });
        }
    });
    (port, fingerprint, recording)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A relay on loopback that may connect to loopback (tests only) on any port.
async fn relay(change: impl FnOnce(&mut Config)) -> (String, Arc<Relay>) {
    let mut config = Config {
        ports: vec![Ports(1024..=65535)],
        allow_networks: vec!["127.0.0.0/8".parse().expect("network")],
        ..Config::default()
    };
    change(&mut config);
    config.check().expect("valid config");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!(
        "ws://127.0.0.1:{}/relay",
        listener.local_addr().expect("address").port()
    );
    let relay = Relay::new(config);
    tokio::spawn(relay.clone().serve_tcp(listener));
    (url, relay)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[tokio::test]
async fn logs_in_through_the_relay_with_tls_end_to_end() {
    let (port, fingerprint, recording) = mumble_server(true).await;
    let (url, relay) = relay(|_| {}).await;
    let outcome = probe::run(Probe {
        relay: url,
        origin: None,
        host: "127.0.0.1".into(),
        port,
        username: USERNAME.into(),
        pin: Some(fingerprint.clone()),
    })
    .await
    .expect("logged in through the relay");
    assert_eq!(outcome.session, 42);
    assert_eq!(outcome.welcome.as_deref(), Some(WELCOME));
    assert_eq!(outcome.release.as_deref(), Some("fake murmur"));
    assert_eq!(outcome.fingerprint, fingerprint);
    assert!(outcome.udp, "the server answered the relay's ping");
    assert!(outcome.udp_answered, "a datagram went through the tunnel");

    // What crossed the relay was TLS records only: no username, welcome text
    // or client release in the clear.
    let up = recording.from_client.lock().expect("lock").clone();
    let down = recording.to_client.lock().expect("lock").clone();
    assert!(up.len() > 100 && down.len() > 100);
    assert_eq!(
        &up[..2],
        &[0x16, 0x03],
        "starts with a TLS handshake record"
    );
    assert_eq!(&down[..2], &[0x16, 0x03]);
    for secret in [
        USERNAME.as_bytes(),
        WELCOME.as_bytes(),
        b"Nuschel web relay probe",
        b"fake murmur",
    ] {
        assert!(!contains(&up, secret) && !contains(&down, secret));
    }
    // Each TLS record in the client's direction is well-formed, so the relay
    // forwarded the stream unchanged.
    let mut at = 0;
    while at + 5 <= up.len() {
        assert!(matches!(up[at], 0x14..=0x17), "record type at {at}");
        at += 5 + usize::from(u16::from_be_bytes([up[at + 3], up[at + 4]]));
    }
    assert_eq!(at, up.len());
    // The tunnel is released once the client is gone.
    for _ in 0..50 {
        if relay.tunnels() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(relay.tunnels(), 0);
}

#[tokio::test]
async fn works_without_udp() {
    let (port, fingerprint, _) = mumble_server(false).await;
    let (url, _) = relay(|_| {}).await;
    let outcome = probe::run(Probe {
        relay: url,
        origin: None,
        host: "localhost".into(),
        port,
        username: USERNAME.into(),
        pin: Some(fingerprint),
    })
    .await
    .expect("the TLS handshake is proof enough");
    assert_eq!(outcome.session, 42);
    assert!(!outcome.udp && !outcome.udp_answered);
}

/// Open a tunnel by hand; the relay's answer.
async fn open(url: &str, origin: Option<&str>, host: &str, port: u16) -> Result<bool, CloseReason> {
    let (mut ws, _) = connect(url, origin).await.expect("upgrade");
    let open = tunnel::encode(&Frame::Open { host, port }).expect("frame");
    ws.send(Message::binary(open)).await.expect("send");
    answer(&mut ws).await
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(
    url: &str,
    origin: Option<&str>,
) -> Result<
    (
        Socket,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ),
    String,
> {
    let mut request = url.into_client_request().expect("request");
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static(tunnel::SUBPROTOCOL),
    );
    if let Some(origin) = origin {
        request
            .headers_mut()
            .insert("Origin", HeaderValue::from_str(origin).expect("origin"));
    }
    tokio_tungstenite::connect_async(request)
        .await
        .map_err(|error| error.to_string())
}

async fn answer(ws: &mut Socket) -> Result<bool, CloseReason> {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(20), ws.next())
            .await
            .expect("an answer in time");
        match message {
            Some(Ok(Message::Binary(bytes))) => match tunnel::decode(&bytes).expect("frame") {
                Frame::Opened { udp } => return Ok(udp),
                Frame::Closed(reason) => return Err(reason),
                other => panic!("unexpected {other:?}"),
            },
            Some(Ok(_)) => continue,
            other => panic!("relay went away: {other:?}"),
        }
    }
}

#[tokio::test]
async fn refuses_private_addresses_and_other_ports() {
    let (port, _, _) = mumble_server(true).await;
    // Without the test's allowance, loopback is refused like every private range.
    let (url, _) = relay(|config| config.allow_networks.clear()).await;
    for host in [
        "127.0.0.1",
        "localhost",
        "10.0.0.1",
        "169.254.169.254",
        "::1",
    ] {
        assert_eq!(
            open(&url, None, host, port).await,
            Err(CloseReason::AddressRefused),
            "{host}"
        );
    }
    let (url, _) = relay(|config| config.ports = vec![Ports(64738..=64738)]).await;
    assert_eq!(
        open(&url, None, "127.0.0.1", port).await,
        Err(CloseReason::PortRefused)
    );
    assert_eq!(
        open(&url, None, "no-such-host.invalid", 64738).await,
        Err(CloseReason::Unresolved)
    );
}

#[tokio::test]
async fn refuses_what_is_not_a_mumble_server() {
    // A TCP service that is not TLS, without a UDP ping answer.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("address").port();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let _ = stream.write_all(b"SSH-2.0-OpenSSH_9.9\r\n").await;
                let mut buffer = [0_u8; 1024];
                while stream.read(&mut buffer).await.unwrap_or(0) > 0 {}
            });
        }
    });
    // With require_ping the missing ping answer is enough.
    let (url, _) = relay(|config| config.require_ping = true).await;
    assert_eq!(
        open(&url, None, "127.0.0.1", port).await,
        Err(CloseReason::NotMumble)
    );
    // Without, the connection opens, but the server's first bytes are not a
    // ServerHello: nothing of them is forwarded.
    let (url, _) = relay(|_| {}).await;
    let (mut ws, _) = connect(&url, None).await.expect("upgrade");
    let open = tunnel::encode(&Frame::Open {
        host: "127.0.0.1",
        port,
    })
    .expect("frame");
    ws.send(Message::binary(open)).await.expect("send");
    assert_eq!(answer(&mut ws).await, Ok(false));
    let hello = [0x16, 3, 1, 0, 4, 1, 0, 0, 0];
    ws.send(Message::binary(
        tunnel::encode(&Frame::Stream(&hello)).expect("frame"),
    ))
    .await
    .expect("send");
    assert_eq!(answer(&mut ws).await, Err(CloseReason::NotMumble));
    // The probe (a real TLS client) hears why, at once.
    let started = std::time::Instant::now();
    let error = probe::run(Probe {
        relay: url.clone(),
        origin: None,
        host: "127.0.0.1".into(),
        port,
        username: USERNAME.into(),
        pin: None,
    })
    .await
    .expect_err("not a Mumble server");
    assert!(error.contains("NotMumble"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(10));

    // A client whose first bytes are not a ClientHello cannot talk to the
    // server at all (here a real TLS server).
    let (tls_port, _, recording) = mumble_server(true).await;
    let (mut ws, _) = connect(&url, None).await.expect("upgrade");
    let open = tunnel::encode(&Frame::Open {
        host: "127.0.0.1",
        port: tls_port,
    })
    .expect("frame");
    ws.send(Message::binary(open)).await.expect("send");
    assert_eq!(answer(&mut ws).await, Ok(true));
    ws.send(Message::binary(
        tunnel::encode(&Frame::Stream(b"GET / HTTP/1.1\r\n\r\n")).expect("frame"),
    ))
    .await
    .expect("send");
    assert_eq!(answer(&mut ws).await, Err(CloseReason::Protocol));
    assert!(recording.from_client.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn checks_the_upgrade_and_the_first_frame() {
    let (url, _) = relay(|config| {
        config.allowed_origins = vec!["https://web.example.org".into()];
    })
    .await;
    // The subprotocol is required.
    let request = url.as_str().into_client_request().expect("request");
    let refused = tokio_tungstenite::connect_async(request).await;
    assert!(refused.is_err());
    // Other origins are refused.
    let error = connect(&url, Some("https://evil.example")).await.err();
    assert!(error.is_some_and(|error| error.contains("403")));
    assert!(connect(&url, None).await.is_err());
    // A first frame that is not Open, or garbage.
    let origin = Some("https://web.example.org");
    for first in [
        Message::text("hello"),
        Message::binary(vec![9, 9, 9]),
        Message::binary(tunnel::encode(&Frame::Stream(b"x")).expect("frame")),
    ] {
        let (mut ws, _) = connect(&url, origin).await.expect("upgrade");
        ws.send(first).await.expect("send");
        assert_eq!(answer(&mut ws).await, Err(CloseReason::Protocol));
    }
}

#[tokio::test]
async fn limits_tunnels_per_client() {
    let (port, _, _) = mumble_server(true).await;
    let (url, relay) = relay(|config| {
        config.client_opens_per_minute = 3;
        config.client_tunnels = 2;
    })
    .await;
    let mut kept = Vec::new();
    for _ in 0..2 {
        let (mut ws, _) = connect(&url, None).await.expect("upgrade");
        let open = tunnel::encode(&Frame::Open {
            host: "127.0.0.1",
            port,
        })
        .expect("frame");
        ws.send(Message::binary(open)).await.expect("send");
        assert_eq!(answer(&mut ws).await, Ok(true));
        kept.push(ws);
    }
    // Two open: the third is refused as busy, and the others keep working.
    assert_eq!(
        open(&url, None, "127.0.0.1", port).await,
        Err(CloseReason::Busy)
    );
    assert_eq!(relay.tunnels(), 2);
    kept.clear();
    for _ in 0..50 {
        if relay.tunnels() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Three opens a minute (the busy one did not count): one more, then the
    // address is rate-limited.
    assert_eq!(open(&url, None, "127.0.0.1", port).await, Ok(true));
    assert_eq!(
        open(&url, None, "127.0.0.1", port).await,
        Err(CloseReason::RateLimited)
    );
}

#[tokio::test]
async fn logs_in_to_murmur_through_the_relay() {
    let Some(binary) = std::env::var_os("NUSCHEL_TEST_MURMUR") else {
        eprintln!("NUSCHEL_TEST_MURMUR is not set; skipping the Murmur run");
        return;
    };
    let root = tempfile::tempdir().expect("temporary directory");
    let port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        probe.local_addr().expect("address").port()
    };
    let ini = root.path().join("murmur.ini");
    std::fs::write(
        &ini,
        format!(
            "database={}\nport={port}\nhost=127.0.0.1\nlogfile={}\nwelcometext=\"{WELCOME}\"\n",
            root.path().join("murmur.sqlite").display(),
            root.path().join("murmur.log").display(),
        ),
    )
    .expect("ini");
    let mut murmur = tokio::process::Command::new(binary)
        .arg("-ini")
        .arg(&ini)
        .arg("-fg")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("start murmur");
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Murmur starts accepting TCP before its UDP worker is necessarily ready
    // (notably while generating its initial certificate). Wait for both sides
    // of the fixture instead of measuring server startup with relay timeouts.
    let udp = UdpSocket::bind("127.0.0.1:0").await.expect("UDP probe");
    udp.connect(("127.0.0.1", port)).await.expect("UDP target");
    let ident = 0x6e75_7363_6865_6c31;
    let mut reply = [0_u8; 64];
    let mut ready = false;
    for _ in 0..100 {
        let _ = udp.send(&tunnel::mumble_ping(ident)).await;
        if let Ok(Ok(size)) =
            tokio::time::timeout(Duration::from_millis(100), udp.recv(&mut reply)).await
            && tunnel::is_ping_reply(&reply[..size], ident)
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ready, "local Murmur did not become UDP ready");
    let (url, _) = relay(|_| {}).await;
    let outcome = probe::run(Probe {
        relay: url,
        origin: None,
        host: "127.0.0.1".into(),
        port,
        username: USERNAME.into(),
        pin: None,
    })
    .await
    .expect("logged in to Murmur through the relay");
    let _ = murmur.kill().await;
    assert!(outcome.session > 0);
    assert_eq!(outcome.welcome.as_deref(), Some(WELCOME));
    assert!(outcome.udp && outcome.udp_answered, "{outcome:?}");
}
