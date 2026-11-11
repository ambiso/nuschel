// SPDX-License-Identifier: EUPL-1.2
//! `nuschl-witness` over HTTP on loopback: enrolment against a server's hello,
//! submissions and cosignatures that clients accept, restarts, and refusals.
use nuschl_kt::witness::auditor::AddEpochs;
use nuschl_kt::witness::note::{CosignerKey, Note, NoteSignature};
use nuschl_kt::{
    AccountId, AkdDirectory, Directory, Ed25519HeadSigner, Label, MemoryStore, VrfSecret,
    WitnessOutcome, WitnessPolicy, WitnessedHead, check_witnessed, verify_head,
};
use nuschl_protocol::nuschl::v1;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

const T0: u64 = 1_790_000_000_000;

async fn directory(seed: u8) -> AkdDirectory<Arc<MemoryStore>> {
    AkdDirectory::open(
        Arc::new(MemoryStore::new()),
        VrfSecret::from_bytes([0x22; 32]),
        Box::new(Ed25519HeadSigner::from_seed(&[seed; 32])),
        T0,
    )
    .await
    .unwrap()
}

/// A Nuschl server's first words: TLS with ALPN `nuschl/1`, then its `Version`
/// with the hello announcing `kt_log`.
async fn fake_server(kt_log: Vec<u8>) -> SocketAddr {
    let generated = rcgen::generate_simple_self_signed(vec!["Nuschl server".into()]).unwrap();
    let certificate = rustls::pki_types::CertificateDer::from(generated.cert.der().to_vec());
    let key =
        rustls::pki_types::PrivateKeyDer::try_from(generated.signing_key.serialize_der()).unwrap();
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![certificate], key)
    .unwrap();
    config.alpn_protocols = vec![b"nuschl/1".to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let hello = v1::Hello {
        protocol: 1,
        capabilities: vec!["kt/1".into()],
        kt_log,
        ..Default::default()
    };
    let frame = nuschl_protocol::nuschl::version_frame(&Default::default(), Some(&hello)).unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            let frame = frame.clone();
            tokio::spawn(async move {
                if let Ok(mut tls) = acceptor.accept(stream).await {
                    let _ = tls.write_all(&frame).await;
                    let _ = tls.flush().await;
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            });
        }
    });
    address
}

async fn witness(state: &std::path::Path, private: bool) -> nuschl_witness::Running {
    let config = nuschl_witness::Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        enrol_private_addresses: private,
        max_request_bytes: 4 * 1024 * 1024,
        ..nuschl_witness::Config::new("witness.test/nuschl-kt", state)
    };
    nuschl_witness::start(config).await.unwrap()
}

/// POSTs a body; the status and the response text.
async fn post(witness: SocketAddr, body: Vec<u8>) -> (u16, String) {
    let url = format!("http://{witness}{}", nuschl_witness::ADD_EPOCHS_PATH);
    tokio::task::spawn_blocking(move || {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .new_agent();
        let mut response = agent
            .post(&url)
            .header("Content-Type", "application/octet-stream")
            .send(&body[..])
            .unwrap();
        let status = response.status().as_u16();
        (
            status,
            response.body_mut().read_to_string().unwrap_or_default(),
        )
    })
    .await
    .unwrap()
}

/// Any value: the directory commits to bytes (nuschld checks what it publishes).
fn binding(n: u8) -> nuschl_kt::DirectoryValue {
    nuschl_kt::DirectoryValue::new(9, vec![n; 40]).unwrap()
}

/// A request head claiming a body of `length` bytes, and the status line answered.
async fn claim(witness: SocketAddr, length: usize) -> String {
    use tokio::io::AsyncReadExt;
    let mut stream = tokio::net::TcpStream::connect(witness).await.unwrap();
    let head = format!(
        "POST {} HTTP/1.1\r\nHost: w\r\nContent-Length: {length}\r\n\r\n",
        nuschl_witness::ADD_EPOCHS_PATH
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut answer = Vec::new();
    let _ = stream.read_to_end(&mut answer).await;
    String::from_utf8_lossy(&answer)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn servers_enrol_and_their_checkpoints_are_cosigned() {
    let state = tempfile::tempdir().unwrap();
    let server = directory(0x11).await;
    let fake = fake_server(server.descriptor().encode()).await;
    let origin = format!("{fake}/nuschl-kt/v1");
    let running = witness(state.path(), true).await;
    let key = CosignerKey::from_vkey(&running.vkey).unwrap();
    let policy = WitnessPolicy::all(vec![key.clone()], 24 * 3600 * 1000);

    for n in 1..=3u8 {
        server
            .publish(
                vec![(Label::Account(AccountId([n; 16])), binding(n))],
                T0 + u64::from(n),
            )
            .await
            .unwrap();
    }
    // A first guess at the auditor's size is answered with its size (none yet).
    let request = server.add_epochs(&origin, 4, 4096).await.unwrap();
    let (status, body) = post(running.address, request.encode().unwrap()).await;
    assert_eq!((status, body.as_str()), (409, "0\n"));
    let request = server.add_epochs(&origin, 0, 4096).await.unwrap();
    let (status, body) = post(running.address, request.encode().unwrap()).await;
    assert_eq!(status, 200, "{body}");

    // The cosignature, appended to the server's checkpoint, is what clients need.
    let line = NoteSignature::parse_line(body.trim_end_matches('\n')).unwrap();
    let mut note = Note::parse(&request.checkpoint).unwrap();
    note.signatures.push(line);
    let head = server.latest_head().await.unwrap();
    let witnessed = WitnessedHead {
        checkpoint: note.format().into_bytes(),
        inclusion: server.inclusion(head.epoch, head.epoch + 1).await.unwrap(),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let verified = verify_head(server.descriptor(), &head).unwrap();
    assert!(matches!(
        check_witnessed(
            server.descriptor(),
            &origin,
            &verified,
            &witnessed,
            &policy,
            now
        ),
        Ok(WitnessOutcome::Witnessed { size: 4, .. })
    ));

    // Monitors read the newest cosigned checkpoint.
    let stem: String = {
        use sha2::Digest as _;
        sha2::Sha256::digest(origin.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    let url = format!("http://{}/nuschl-kt/v1/checkpoints/{stem}", running.address);
    let published = tokio::task::spawn_blocking(move || {
        ureq::get(&url)
            .call()
            .unwrap()
            .body_mut()
            .read_to_string()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(Note::parse(published.as_bytes()).is_ok());

    // A restart keeps the state: the refresh at the same size is cosigned, a
    // submission from scratch conflicts.
    drop(running);
    let running = witness(state.path(), true).await;
    assert_eq!(running.vkey, key.vkey());
    let refresh = server.add_epochs(&origin, 4, 4096).await.unwrap();
    assert!(refresh.audit.is_none());
    assert_eq!(
        post(running.address, refresh.encode().unwrap()).await.0,
        200
    );
    let again = server.add_epochs(&origin, 0, 4096).await.unwrap();
    assert_eq!(
        post(running.address, again.encode().unwrap()).await,
        (409, "4\n".into())
    );
}

#[tokio::test]
async fn a_server_announcing_another_log_is_not_enrolled() {
    let state = tempfile::tempdir().unwrap();
    let server = directory(0x11).await;
    let other = directory(0x33).await;
    // The network shows another log at this address than the one submitted.
    let fake = fake_server(other.descriptor().encode()).await;
    let origin = format!("{fake}/nuschl-kt/v1");
    let running = witness(state.path(), true).await;
    let request = server.add_epochs(&origin, 0, 4096).await.unwrap();
    assert_eq!(
        post(running.address, request.encode().unwrap()).await.0,
        403
    );
    // Nothing listens there at all.
    let request = server
        .add_epochs("127.0.0.1:9/nuschl-kt/v1", 0, 4096)
        .await
        .unwrap();
    assert_eq!(
        post(running.address, request.encode().unwrap()).await.0,
        502
    );
}

#[tokio::test]
async fn private_addresses_and_bad_requests_are_refused() {
    let state = tempfile::tempdir().unwrap();
    let server = directory(0x11).await;
    let fake = fake_server(server.descriptor().encode()).await;
    let running = witness(state.path(), false).await;
    let request = server
        .add_epochs(&format!("{fake}/nuschl-kt/v1"), 0, 4096)
        .await
        .unwrap();
    // Loopback is not enrolled without `enrol_private_addresses`.
    assert_eq!(
        post(running.address, request.encode().unwrap()).await.0,
        502
    );
    // Malformed and oversized bodies.
    assert_eq!(post(running.address, b"hello".to_vec()).await.0, 400);
    assert_eq!(
        claim(running.address, 5 * 1024 * 1024).await,
        "HTTP/1.1 413 Content Too Large"
    );
    // Another origin form.
    let mut wrong = request.clone();
    wrong.origin = "example.org:1/other".into();
    let bytes = AddEpochs {
        checkpoint: Vec::new(),
        ..wrong
    }
    .encode()
    .unwrap();
    assert_eq!(post(running.address, bytes).await.0, 400);
}
