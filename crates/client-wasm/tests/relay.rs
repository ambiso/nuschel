// SPDX-License-Identifier: EUPL-1.2
//! The browser's session (TLS in the client) through the real web relay to a
//! Mumble server on loopback, natively: the same code the WebAssembly build
//! runs, with tokio and tungstenite moving the bytes instead of the browser.

use futures_util::{SinkExt, StreamExt};
use nuschel_client::input::ServerEvent;
use nuschel_client_wasm::session::{Event, Login, Session};
use nuschel_protocol::{control as proto, decode_control, encode_control, kind, tunnel};
use nuschel_web_relay::{Config, Relay, config::Ports};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};

/// A TLS Mumble server that sends the root channel, our user and
/// `ServerSync` after `Authenticate`; its port and certificate fingerprint.
async fn mumble_server() -> (u16, String) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("cert");
    let certificate = certified.cert.der().clone();
    let fingerprint = nuschel_client_wasm::session::hex(&sha2_digest(certificate.as_ref()));
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
                let frames = [
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
                            ..Default::default()
                        },
                    ),
                ];
                for frame in frames {
                    let _ = tls.write_all(&frame.expect("frame")).await;
                }
                while tls.read_buf(&mut buffer).await.unwrap_or(0) > 0 {}
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
    assert!(matches!(
        events.last(),
        Some(Event::Server(ServerEvent::Connected { session: 3 }))
    ));
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
    let Some(binary) = std::env::var_os("NUSCHEL_TEST_MURMUR") else {
        eprintln!("NUSCHEL_TEST_MURMUR is not set; skipping the Murmur run");
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
