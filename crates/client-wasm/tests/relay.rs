// SPDX-License-Identifier: EUPL-1.2
//! The browser's session (TLS in the client) through the real web relay to a
//! Mumble server on loopback, natively: the same code the WebAssembly build
//! runs, with tokio and tungstenite moving the bytes instead of the browser.

use futures_util::{SinkExt, StreamExt};
use nuschl_client::input::ServerEvent;
use nuschl_client_wasm::session::{Event, Login, Session};
use nuschl_protocol::{control as proto, decode_control, encode_control, kind, tunnel};
use nuschl_web_relay::{Config, Relay, config::Ports};
use prost::Message as _;
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};

/// A TLS Mumble server that sends the root channel, our user and
/// `ServerSync` after `Authenticate`; its port and certificate fingerprint.
async fn mumble_server() -> (u16, String) {
    mumble_server_with_budget(None, None).await
}

async fn mumble_server_with_budget(initial: Option<u32>, update: Option<u32>) -> (u16, String) {
    mumble_server_with_limits(initial, update, None).await
}
async fn mumble_server_with_limits(
    initial: Option<u32>,
    update: Option<u32>,
    content_limit: Option<u32>,
) -> (u16, String) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("cert");
    let certificate = certified.cert.der().clone();
    let fingerprint = nuschl_client_wasm::session::hex(&sha2_digest(certificate.as_ref()));
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
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(stream).await else {
                    return;
                };
                let mut buffer = bytes::BytesMut::new();
                'login: loop {
                    if tls.read_buf(&mut buffer).await.unwrap_or(0) == 0 {
                        return;
                    }
                    while let Ok(Some(frame)) = decode_control(&mut buffer) {
                        if frame.kind == kind::AUTHENTICATE {
                            break 'login;
                        }
                    }
                }
                if let Some(limit) = content_limit {
                    let config = proto::ServerConfig {
                        allow_html: Some(true),
                        message_length: Some(0),
                        image_message_length: Some(limit),
                        ..Default::default()
                    };
                    tls.write_all(&encode_control(kind::SERVER_CONFIG, &config).unwrap())
                        .await
                        .unwrap();
                }
                let frames = [
                    encode_control(
                        kind::VERSION,
                        &proto::Version {
                            version_v2: Some(nuschl_protocol::voice::PROTOBUF_VERSION),
                            ..Default::default()
                        },
                    ),
                    encode_control(
                        kind::CHANNEL_STATE,
                        &proto::ChannelState {
                            channel_id: Some(0),
                            name: Some("Root".into()),
                            ..Default::default()
                        },
                    ),
                    encode_control(
                        kind::USER_STATE,
                        &proto::UserState {
                            session: Some(3),
                            name: Some("web".into()),
                            channel_id: Some(0),
                            ..Default::default()
                        },
                    ),
                    encode_control(
                        kind::SERVER_SYNC,
                        &proto::ServerSync {
                            session: Some(3),
                            max_bandwidth: initial,
                            ..Default::default()
                        },
                    ),
                ];
                for frame in frames {
                    let _ = tls.write_all(&frame.expect("frame")).await;
                }
                let mut update = update;
                loop {
                    while let Ok(Some(frame)) = decode_control(&mut buffer) {
                        if frame.kind == kind::USER_STATE {
                            // Full/sparse real server acknowledgements use the same control frame.
                            let state =
                                proto::UserState::decode(frame.payload).expect("self state");
                            let reply = proto::UserState {
                                session: Some(3),
                                ..state
                            };
                            let _ = tls
                                .write_all(&encode_control(kind::USER_STATE, &reply).unwrap())
                                .await;
                        } else if frame.kind == kind::UDP_TUNNEL {
                            let mut audio =
                                nuschl_protocol::udp::Audio::decode(&frame.payload[1..])
                                    .expect("protobuf Opus");
                            assert_eq!(audio.sender_session, 0);
                            audio.sender_session = 3;
                            let mut packet = vec![0];
                            audio.encode(&mut packet).unwrap();
                            let _ = tls
                                .write_all(&nuschl_protocol::encode_tunnel(&packet))
                                .await;
                            if let Some(max_bandwidth) = update.take() {
                                let config = proto::ServerConfig {
                                    max_bandwidth: Some(max_bandwidth),
                                    ..Default::default()
                                };
                                let _ = tls
                                    .write_all(
                                        &encode_control(kind::SERVER_CONFIG, &config).unwrap(),
                                    )
                                    .await;
                            }
                        } else if frame.kind == kind::TEXT_MESSAGE {
                            let text = proto::TextMessage::decode(frame.payload).expect("text");
                            let reply = proto::TextMessage {
                                actor: Some(3),
                                channel_id: text.channel_id,
                                message: format!("fixture received: {}", text.message),
                                ..Default::default()
                            };
                            let _ = tls
                                .write_all(
                                    &encode_control(kind::TEXT_MESSAGE, &reply)
                                        .expect("text frame"),
                                )
                                .await;
                        }
                    }
                    if tls.read_buf(&mut buffer).await.unwrap_or(0) == 0 {
                        break;
                    }
                }
            });
        }
    });
    (port, fingerprint)
}

fn sha2_digest(bytes: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).to_vec()
}

async fn relay() -> String {
    let config = Config {
        ports: vec![Ports(1024..=65535)],
        allow_networks: vec!["127.0.0.0/8".parse().expect("network")],
        ..Config::default()
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!(
        "ws://127.0.0.1:{}/relay",
        listener.local_addr().expect("address").port()
    );
    tokio::spawn(Relay::new(config).serve_tcp(listener));
    url
}

/// Drive a session over a WebSocket until it is connected or closed.
async fn run(relay: &str, login: Login) -> Vec<Event> {
    let mut request = relay.into_client_request().expect("request");
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static(tunnel::SUBPROTOCOL),
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("relay");
    let mut session = Session::new(login).expect("session");
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        for message in session.take_outgoing() {
            ws.send(Message::binary(message)).await.expect("send");
        }
        let new = session.take_events();
        let done = new.iter().any(|event| {
            matches!(
                event,
                Event::Closed { .. } | Event::Server(ServerEvent::Connected { .. })
            )
        });
        events.extend(new);
        if done {
            return events;
        }
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(Message::Binary(bytes)))) => session.receive(&bytes),
            Ok(Some(Ok(_))) => {}
            Ok(_) => session.disconnected(),
            Err(_) => panic!("no login in time: {events:?}"),
        }
    }
}

#[tokio::test]
async fn logs_in_through_the_relay_with_tls_in_the_client() {
    let (port, fingerprint) = mumble_server().await;
    let relay = relay().await;
    let events = run(
        &relay,
        Login {
            host: "127.0.0.1".into(),
            port,
            username: "web".into(),
            password: None,
            tokens: Vec::new(),
            pin: Some(fingerprint.clone()),
        },
    )
    .await;
    assert!(
        matches!(events[0], Event::Opened { udp: false }),
        "{events:?}"
    );
    assert!(events.iter().any(
        |event| matches!(event, Event::Server(ServerEvent::PeerCertificate(pin)) if *pin == fingerprint)
    ));
    assert!(events.iter().any(
        |event| matches!(event, Event::Server(ServerEvent::Channel(channel)) if channel.name == "Root")
    ));
    assert!(events.iter().any(
        |event| matches!(event, Event::Server(ServerEvent::User(user)) if user.session == 3 && user.name == "web")
    ));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Server(ServerEvent::Connected { session: 3 })))
    );
}

#[tokio::test]
async fn legacy_text_preserves_newlines_and_escapes_html_across_tls_relay() {
    let (port, fingerprint) = mumble_server().await;
    let relay = relay().await;
    let mut request = relay.into_client_request().expect("request");
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static(tunnel::SUBPROTOCOL),
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("relay");
    let mut session = Session::new(Login {
        host: "127.0.0.1".into(),
        port,
        username: "web".into(),
        password: None,
        tokens: Vec::new(),
        pin: Some(fingerprint),
    })
    .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let body = "literal <script>\n\n```rust\nfn main() {}\n```";
    let mut sent = false;
    loop {
        for message in session.take_outgoing() {
            ws.send(Message::binary(message)).await.expect("send");
        }
        for event in session.take_events() {
            match event {
                Event::Server(ServerEvent::Connected { .. }) => {
                    session
                        .send_text(
                            nuschl_client::model::TextTarget::Channel(0),
                            body.into(),
                            None,
                        )
                        .unwrap();
                    sent = true;
                }
                Event::Text {
                    body,
                    outgoing: false,
                    ..
                } => {
                    assert!(sent);
                    assert_eq!(
                        body,
                        "fixture received: literal &lt;script&gt;<br/><br/>```rust<br/>fn main() {}<br/>```"
                    );
                    return;
                }
                Event::Closed { reason } => panic!("closed: {reason}"),
                _ => {}
            }
        }
        // Sending from the event handler queues another tunnel record.
        for message in session.take_outgoing() {
            ws.send(Message::binary(message)).await.expect("send text");
        }
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(Message::Binary(bytes)))) => session.receive(&bytes),
            Ok(Some(Ok(_))) => {}
            other => panic!("text relay ended: {other:?}"),
        }
    }
}

// Valid public synthetic 1×1 PNG, with a large ancillary text chunk. This
// exercises transport size without adding a codec to the session test graph.
fn large_png_html() -> String {
    fn chunk(png: &mut Vec<u8>, name: &[u8; 4], body: &[u8]) {
        png.extend_from_slice(&(body.len() as u32).to_be_bytes());
        png.extend_from_slice(name);
        png.extend_from_slice(body);
        let mut crc = u32::MAX;
        for byte in name.iter().chain(body) {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ if crc & 1 == 0 { 0 } else { 0xedb8_8320 };
            }
        }
        png.extend_from_slice(&(!crc).to_be_bytes());
    }
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut png, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
    let mut padding = b"padding\0".to_vec();
    padding.extend(std::iter::repeat_n(b'A', 150_000));
    chunk(&mut png, b"tEXt", &padding);
    chunk(
        &mut png,
        b"IDAT",
        &[120, 156, 99, 208, 8, 168, 248, 15, 0, 3, 132, 1, 240],
    );
    chunk(&mut png, b"IEND", &[]);
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::new();
    for bytes in png.chunks(3) {
        let value = (u32::from(bytes[0]) << 16)
            | (u32::from(*bytes.get(1).unwrap_or(&0)) << 8)
            | u32::from(*bytes.get(2).unwrap_or(&0));
        for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            let byte = if index > bytes.len() {
                b'='
            } else {
                alphabet[((value >> shift) & 63) as usize]
            };
            match byte {
                b'+' => encoded.push_str("%2B"),
                b'/' => encoded.push_str("%2F"),
                b'=' => encoded.push_str("%3D"),
                _ => encoded.push(char::from(byte)),
            }
        }
    }
    format!("<img src=\"data:image/png;base64,{encoded}\" />")
}

#[tokio::test]
async fn large_text_and_image_cross_default_rustls_buffer_without_truncation_or_interleaving() {
    let (port, fingerprint) = mumble_server_with_limits(None, None, Some(0)).await;
    let relay = relay().await;
    let mut request = relay.into_client_request().unwrap();
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static(tunnel::SUBPROTOCOL),
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let mut session = Session::new(Login {
        host: "127.0.0.1".into(),
        port,
        username: "web".into(),
        password: None,
        tokens: vec![],
        pin: Some(fingerprint),
    })
    .unwrap();
    let text = "x".repeat(200_000);
    let image = large_png_html();
    assert!(image.len() > 200_000 && image.len() < 256 * 1024);
    let image_wire = format!("{image}<br/>one &amp; &lt;caption&gt;");
    let expected = [
        format!("fixture received: {text}"),
        format!("fixture received: {image_wire}"),
    ];
    let mut channel_instance = None;
    let mut own_instance = None;
    let mut received = 0;
    let mut sent_boundaries = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        for frame in session.take_outgoing() {
            ws.send(Message::binary(frame)).await.unwrap();
        }
        for event in session.take_events() {
            match event {
                Event::Server(ServerEvent::Channel(channel)) if channel.id == 0 => {
                    channel_instance = Some(channel.instance)
                }
                Event::Server(ServerEvent::User(user)) if user.session == 3 => {
                    own_instance = Some(user.instance)
                }
                Event::Server(ServerEvent::Connected { .. }) => {
                    session
                        .send_text(
                            nuschl_client::model::TextTarget::Channel(0),
                            text.clone(),
                            None,
                        )
                        .unwrap();
                    // Queue both before the transport drains: the TLS records
                    // must contain complete first/second application frames.
                    session
                        .send_inline_image(
                            nuschl_client::model::TextTarget::Channel(0),
                            &image,
                            "one & <caption>",
                            None,
                            channel_instance,
                            own_instance.unwrap(),
                        )
                        .unwrap();
                    assert!(!session.is_closed());
                }
                Event::Sent { transport, .. } => sent_boundaries.push(transport),
                Event::Text {
                    outgoing: false,
                    body,
                    ..
                } => {
                    assert_eq!(body, expected[received]);
                    received += 1;
                    if received == 2 {
                        assert_eq!(sent_boundaries.len(), 2);
                        assert!(sent_boundaries[0] < sent_boundaries[1]);
                        assert!(!session.is_closed());
                        return;
                    }
                }
                Event::Closed { reason } => panic!("large TLS send closed: {reason}"),
                _ => {}
            }
        }
        for frame in session.take_outgoing() {
            ws.send(Message::binary(frame)).await.unwrap();
        }
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(Message::Binary(bytes)))) => session.receive(&bytes),
            other => panic!("large TLS relay ended: {other:?}"),
        }
    }
}

#[tokio::test]
async fn an_unknown_certificate_is_the_users_decision() {
    let (port, fingerprint) = mumble_server().await;
    let relay = relay().await;
    // No pin, and no authority vouches for a self-signed certificate.
    let login = Login {
        host: "127.0.0.1".into(),
        port,
        username: "web".into(),
        password: None,
        tokens: Vec::new(),
        pin: None,
    };
    let events = run(&relay, login.clone()).await;
    assert!(
        events.iter().any(|event| matches!(event, Event::Certificate { sha256, changed: false } if *sha256 == fingerprint)),
        "{events:?}"
    );
    assert!(matches!(events.last(), Some(Event::Closed { .. })));
    // A wrong pin is a changed certificate.
    let events = run(
        &relay,
        Login {
            pin: Some("00".repeat(32)),
            ..login
        },
    )
    .await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Certificate { changed: true, .. })),
        "{events:?}"
    );
}

#[tokio::test]
async fn logs_in_to_murmur() {
    let Some(binary) = std::env::var_os("NUSCHL_TEST_MURMUR") else {
        eprintln!("NUSCHL_TEST_MURMUR is not set; skipping the Murmur run");
        return;
    };
    let root = tempfile::tempdir().expect("temporary directory");
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("address")
        .port();
    let ini = root.path().join("murmur.ini");
    std::fs::write(
        &ini,
        format!(
            "database={}\nport={port}\nhost=127.0.0.1\nlogfile={}\n",
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
    let relay = relay().await;
    let login = Login {
        host: "127.0.0.1".into(),
        port,
        username: "web".into(),
        password: None,
        tokens: Vec::new(),
        pin: None,
    };
    // First contact: the certificate is for the user to accept, then pinned.
    let events = run(&relay, login.clone()).await;
    let sha256 = events
        .iter()
        .find_map(|event| match event {
            Event::Certificate { sha256, .. } => Some(sha256.clone()),
            _ => None,
        })
        .expect("Murmur's self-signed certificate");
    let events = run(
        &relay,
        Login {
            pin: Some(sha256),
            ..login
        },
    )
    .await;
    let _ = murmur.kill().await;
    assert!(
        matches!(events[0], Event::Opened { udp: true }),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Server(ServerEvent::Connected { .. }))),
        "{events:?}"
    );
    assert!(events.iter().any(
        |event| matches!(event, Event::Server(ServerEvent::Channel(channel)) if channel.id == 0)
    ));
}

#[tokio::test]
async fn opus_voice_round_trips_only_after_explicit_unmute_over_private_tls_relay() {
    let (port, pin) = mumble_server().await;
    let relay = relay().await;
    let mut request = relay.into_client_request().unwrap();
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static(tunnel::SUBPROTOCOL),
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let mut session = Session::new(Login {
        host: "127.0.0.1".into(),
        port,
        username: "web".into(),
        password: None,
        tokens: vec![],
        pin: Some(pin),
    })
    .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut requested = false;
    let mut sent = false;
    loop {
        for event in session.take_events() {
            match event {
                Event::Server(ServerEvent::Connected { .. }) => {
                    assert!(session.send_voice(0, vec![1, 2, 3], false).is_err());
                    session.set_self(false, false);
                    requested = true;
                }
                Event::Server(ServerEvent::User(_))
                    if requested && !sent && !session.voice_policy().0 =>
                {
                    session
                        .send_voice(9007199254740993, vec![1, 2, 3], true)
                        .unwrap();
                    sent = true;
                }
                Event::Voice(packet) => {
                    assert!(sent);
                    assert_eq!(packet.sender, 3);
                    assert_eq!(packet.sequence, 9007199254740993);
                    assert_eq!(packet.opus, [1, 2, 3]);
                    assert!(packet.end);
                    session.set_self(true, false);
                    assert!(
                        session
                            .send_voice(9007199254740995, vec![1, 2, 3], true)
                            .is_err()
                    );
                    return;
                }
                Event::Closed { reason } => panic!("closed: {reason}"),
                _ => {}
            }
        }
        for message in session.take_outgoing() {
            ws.send(Message::binary(message)).await.unwrap();
        }
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(Message::Binary(bytes)))) => session.receive(&bytes),
            Ok(Some(Ok(_))) => {}
            other => panic!("voice relay ended: {other:?}"),
        }
    }
}

#[tokio::test]
async fn authenticated_live_bandwidth_reduction_caps_outgoing_audio_through_the_private_tls_relay()
{
    let (port, pin) = mumble_server_with_budget(Some(200_000), Some(48_000)).await;
    let relay = relay().await;
    let mut request = relay.into_client_request().unwrap();
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static(tunnel::SUBPROTOCOL),
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let mut session = Session::new(Login {
        host: "127.0.0.1".into(),
        port,
        username: "budget-fixture".into(),
        password: None,
        tokens: vec![],
        pin: Some(pin),
    })
    .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let (mut requested, mut high_sent, mut high_received, mut low_sent) =
        (false, false, false, false);
    loop {
        for event in session.take_events() {
            match event {
                Event::Server(ServerEvent::Connected { .. }) => {
                    assert_eq!(session.voice_bitrate(), 128_000);
                    session.set_self(false, false);
                    requested = true;
                }
                Event::Server(ServerEvent::User(_))
                    if requested && !high_sent && !session.voice_policy().0 =>
                {
                    session.send_voice(0, vec![1; 320], false).unwrap();
                    high_sent = true;
                }
                Event::Voice(packet) if !high_received => {
                    assert_eq!(packet.opus.len(), 320);
                    high_received = true;
                }
                Event::Server(ServerEvent::Limits(limits))
                    if limits.max_bandwidth == Some(48_000) =>
                {
                    assert!(high_received);
                    assert_eq!(session.voice_bitrate(), 24_000);
                    assert!(session.send_voice(2, vec![1; 320], false).is_err());
                    session.send_voice(2, vec![1; 60], false).unwrap();
                    low_sent = true;
                }
                Event::Voice(packet) if low_sent => {
                    assert_eq!(packet.opus.len(), 60);
                    return;
                }
                Event::Closed { reason } => panic!("closed: {reason}"),
                _ => {}
            }
        }
        for message in session.take_outgoing() {
            ws.send(Message::binary(message)).await.unwrap();
        }
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(Message::Binary(bytes)))) => session.receive(&bytes),
            Ok(Some(Ok(_))) => {}
            other => panic!("budget relay ended: {other:?}"),
        }
    }
}
