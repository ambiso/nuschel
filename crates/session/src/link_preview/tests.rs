// SPDX-License-Identifier: EUPL-1.2
//! Ignored, serialized real-network fixture. Never enabled in a production lib.
use super::*;
use nuschl_link_preview::policy::{DnsSettings, Mode};
use rustls::pki_types::CertificateDer;
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpStream,
    path::Path,
    sync::{Mutex, atomic::AtomicUsize},
};

static TEST_ROOT: Mutex<Option<CertificateDer<'static>>> = Mutex::new(None);
static ADMISSIONS: [AtomicUsize; 4] = [const { AtomicUsize::new(0) }; 4];

pub(super) enum Admission {
    Dns,
    Socket,
    HttpChild,
    Offline,
}
pub(super) fn admit(kind: Admission) {
    ADMISSIONS[kind as usize].fetch_add(1, Ordering::Relaxed);
}
pub(super) fn with_fixture_root(mut roots: rustls::RootCertStore) -> rustls::RootCertStore {
    if let Some(root) = TEST_ROOT.lock().unwrap().clone() {
        roots.add(root).unwrap();
    }
    roots
}
fn admissions() -> [usize; 4] {
    std::array::from_fn(|i| ADMISSIONS[i].load(Ordering::Relaxed))
}
fn control(operation: &str) -> Value {
    let mut socket = TcpStream::connect(("127.0.0.1", 39211)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let input = serde_json::to_vec(&json!({"operation":operation})).unwrap();
    socket
        .write_all(&(input.len() as u32).to_be_bytes())
        .unwrap();
    socket.write_all(&input).unwrap();
    let mut size = [0; 4];
    socket.read_exact(&mut size).unwrap();
    let size = u32::from_be_bytes(size) as usize;
    assert!(size <= 512 * 1024);
    let mut bytes = vec![0; size];
    socket.read_exact(&mut bytes).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}
fn events() -> Vec<Value> {
    let value = control("snapshot");
    assert_eq!(value["overrun"], false);
    let events = value["events"].as_array().unwrap();
    assert!(
        events
            .iter()
            .all(|event| event["kind"] != "peer_error" || event["error"] != "AssertionError"),
        "synthetic peer assertion failed: {events:?}"
    );
    events.clone()
}
fn write_new(path: &Path, bytes: &[u8]) {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut output = options.open(path).unwrap();
    output.write_all(bytes).unwrap();
}
fn fixture_cert(directory: &Path) -> CertificateDer<'static> {
    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa,
        KeyPair, KeyUsagePurpose,
    };
    let mut parameters = CertificateParams::new(Vec::<String>::new()).unwrap();
    parameters.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    parameters.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca = CertifiedIssuer::self_signed(parameters, KeyPair::generate().unwrap()).unwrap();
    let mut parameters = CertificateParams::new(vec![
        "resolver.preview.example.org".into(),
        "page.preview.example.org".into(),
    ])
    .unwrap();
    parameters.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let key = KeyPair::generate().unwrap();
    let cert = parameters.signed_by(&key, &ca).unwrap();
    write_new(&directory.join("server.pem"), cert.pem().as_bytes());
    write_new(
        &directory.join("server-key.pem"),
        key.serialize_pem().as_bytes(),
    );
    ca.der().clone()
}
fn direct() -> Settings {
    Settings {
        mode: Mode::Direct,
        ..Settings::default()
    }
}
fn proxy() -> Settings {
    Settings {
        mode: Mode::ProxyRequired,
        proxy: Some("socks5://127.0.0.1:1080".into()),
        dns: Some(DnsSettings {
            url: "https://resolver.preview.example.org/dns-query".into(),
            bootstrap: vec!["93.184.215.53".parse().unwrap()],
        }),
    }
}
async fn request(worker: &Path, settings: Settings, url: &str) -> Result<Card, Unavailable> {
    let (_sender, cancel) = watch::channel(false);
    // This deadline is a test failure, never a claim that dropping the future
    // retires its actor. The namespace supervisor subsequently owns cleanup.
    tokio::time::timeout(
        Duration::from_secs(12),
        fetch(
            worker.to_owned(),
            settings,
            Target::parse(url).unwrap(),
            cancel,
        ),
    )
    .await
    .expect("actual fetch failed to retire within fixture deadline")
}
async fn wait_event(kind: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !events().iter().any(|event| event["kind"] == kind) {
        assert!(Instant::now() < deadline, "missing fixture event {kind}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn actual_stream_quota(worker: &Path, restart: bool) {
    use nuschl_ipc::{host, preview_fetch as wire};
    use std::{ffi::OsString, process::Stdio};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut child = host::preview_sandbox_command(worker, &[OsString::from("--preview-fetch")])
        .unwrap()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut retirement = host::preview_lifecycle::Retirement::new(&mut child);
    let result = async {
        retirement
            .ready()
            .await
            .map_err(|error| error.to_string())?;
        let mut input = child.stdin.take().ok_or("missing input")?;
        let mut output = child.stdout.take().ok_or("missing output")?;
        let start = wire::Request::Start {
            schema: wire::SCHEMA,
            purpose: wire::Purpose::Document,
        };
        let count = if restart { 2 } else { wire::MAX_CHUNKS + 2 };
        for id in 1..=count {
            let chunk = wire::Request::Chunk {
                data: wire::encode_data(if id == 2 {
                    b"HTTP/1.1 200 OK\r\nX-Padding: "
                } else {
                    b"x"
                }),
                eof: false,
            };
            let frame =
                wire::encode_request(id as u64, if id == 1 || restart { &start } else { &chunk })
                    .map_err(|error| error.to_string())?;
            input
                .write_all(&frame)
                .await
                .map_err(|error| error.to_string())?;
            let size = output.read_u32().await.map_err(|error| error.to_string())? as usize;
            if !(1..=1024).contains(&size) {
                return Err("unexpected quota reply size".to_owned());
            }
            let mut bytes = vec![0; size];
            output
                .read_exact(&mut bytes)
                .await
                .map_err(|error| error.to_string())?;
            let reply = wire::decode_reply(id as u64, &bytes).map_err(|error| error.to_string())?;
            let expected = if restart {
                wire::Failure::Malformed
            } else {
                wire::Failure::TooLarge
            };
            if id == count {
                if !matches!(reply, wire::Reply::Rejected(reason) if reason == expected) {
                    return Err(format!("quota not enforced: {reply:?}"));
                }
            } else if !matches!(reply, wire::Reply::More) {
                return Err(format!("premature quota response: {reply:?}"));
            }
        }
        Ok::<(), String>(())
    };
    // Preserve the same original namespace handle on success, error, timeout.
    let outcome = tokio::time::timeout(Duration::from_secs(8), result).await;
    drop(child.stdin.take());
    retirement.retire(&mut child).await;
    outcome.expect("stream quota deadline").unwrap();
}

/// Requires the reviewed private network namespace wrapper and real content
/// worker. No other ignored test may use TEST_ROOT concurrently.
#[tokio::test(flavor = "current_thread")]
#[ignore = "owned no-Internet network namespace and actual content worker required"]
async fn actual_fetch_privacy() {
    let directory =
        PathBuf::from(std::env::var_os("NUSCHL_PREVIEW_FIXTURE").expect("owned fixture directory"));
    let worker =
        PathBuf::from(std::env::var_os("NUSCHL_PREVIEW_WORKER").expect("actual content worker"));
    assert!(worker.is_absolute() && worker.is_file());
    assert!(directory.join("peers-ready.json").is_file());
    assert!(
        cfg!(target_os = "linux"),
        "owned Linux namespace fixture required"
    );
    let mut status = String::new();
    std::fs::File::open("/proc/self/status")
        .unwrap()
        .take(16 * 1024 + 1)
        .read_to_string(&mut status)
        .unwrap();
    assert!(status.len() <= 16 * 1024);
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
            .expect("required process status field")
            .trim()
    };
    for name in ["CapEff", "CapPrm", "CapAmb"] {
        assert_eq!(
            field(name),
            "0000000000000000",
            "test actor retained {name}"
        );
    }
    assert_eq!(field("NoNewPrivs"), "1");
    for name in ["Uid", "Gid"] {
        let ids: Vec<u32> = field(name)
            .split_ascii_whitespace()
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(ids.len(), 4);
        assert!(
            ids.iter().all(|id| *id != 0),
            "test actor must have a nonroot {name}"
        );
    }
    let mut cgroup = String::new();
    std::fs::File::open("/proc/self/cgroup")
        .unwrap()
        .take(4097)
        .read_to_string(&mut cgroup)
        .unwrap();
    assert!(cgroup.len() <= 4096);
    write_new(
        &directory.join("test-process-authority.json"),
        &serde_json::to_vec_pretty(&json!({
            "CapEff": field("CapEff"), "CapPrm": field("CapPrm"), "CapAmb": field("CapAmb"),
            "NoNewPrivs": field("NoNewPrivs"), "Uid": field("Uid"), "Gid": field("Gid"),
            "cgroup": cgroup,
            "scope": "Actual ignored test process before DNS/socket/worker admission"
        }))
        .unwrap(),
    );
    let root = fixture_cert(&directory);
    let mut checks = Vec::new();
    macro_rules! checked {
        ($name:expr) => {
            checks.push($name);
        };
    }

    let before = admissions();
    let peers = events();
    assert!(
        request(
            &worker,
            Settings::default(),
            "http://off.preview.example.org/text"
        )
        .await
        .is_err()
    );
    assert!(
        request(
            &worker,
            Settings {
                mode: Mode::ProxyRequired,
                ..Settings::default()
            },
            "http://off.preview.example.org/text"
        )
        .await
        .is_err()
    );
    assert_eq!(admissions(), before);
    assert_eq!(events(), peers);
    checked!("Off and incomplete ProxyRequired perform zero DNS/socket/child/offline admission");

    actual_stream_quota(&worker, false).await;
    actual_stream_quota(&worker, true).await;
    checked!(
        "Actual isolated HTTP worker rejects chunk 513 and second Start without budget reset; tracked retirement awaited"
    );

    let card = request(
        &worker,
        direct(),
        "http://93.184.215.14/redirect#sender-fragment",
    )
    .await
    .unwrap();
    assert!(card.is_for("http://93.184.215.14/redirect#sender-fragment"));
    assert_eq!(card.title(), "Synthetic preview");
    assert!(
        card.image().is_some(),
        "real isolated image preparation required"
    );
    checked!(
        "Direct numeric public alias follows redirect and embeds prepared image using actual worker"
    );

    let before = admissions();
    assert!(
        request(&worker, direct(), "http://direct.preview.example.org/text")
            .await
            .is_ok()
    );
    assert_eq!(admissions()[0], before[0] + 1);
    checked!("Direct hostname uses owned system resolver and numeric pinned socket");
    for name in ["mixed.preview.example.org", "private.preview.example.org"] {
        let before = admissions();
        assert_eq!(
            request(&worker, direct(), &format!("http://{name}/text"))
                .await
                .unwrap_err(),
            Unavailable::Address
        );
        assert_eq!(admissions()[1..], before[1..]);
    }
    checked!("Direct complete mixed/private DNS answers reject before sockets/children");

    for path in [
        "private-redirect",
        "loop/0",
        "compressed",
        "ambiguous",
        "large-head",
        "large-body",
        "raw-overhead",
    ] {
        let before = admissions();
        assert!(
            request(&worker, direct(), &format!("http://93.184.215.14/{path}"))
                .await
                .is_err(),
            "accepted {path}"
        );
        assert_eq!(
            admissions()[3],
            before[3],
            "invalid HTTP reached offline extraction: {path}"
        );
    }
    checked!(
        "Actual HTTP worker rejects private redirects, redirect quota, compression and raw/header/body/framing bounds"
    );
    let card = request(&worker, direct(), "http://93.184.215.14/private-image")
        .await
        .unwrap();
    assert!(card.image().is_none());
    checked!("Private image URL never connects and metadata survives");

    // Exact production root path: only the cfg(test) CA injection differs in
    // subsequent positives. No verifier, hostname or encryption bypass exists.
    assert!(
        request(&worker, direct(), "https://page.preview.example.org/text")
            .await
            .is_err()
    );
    checked!("Unmodified production WebPKI roots reject synthetic CA");
    *TEST_ROOT.lock().unwrap() = Some(root);
    assert!(
        request(&worker, direct(), "https://page.preview.example.org/text")
            .await
            .is_ok()
    );
    assert!(
        request(&worker, direct(), "https://wrong.preview.example.org/text")
            .await
            .is_err()
    );
    checked!("Test CA still requires valid certificate hostname");

    let start = events().len();
    let before = admissions();
    assert!(
        request(&worker, proxy(), "http://proxied.preview.example.org/text")
            .await
            .is_ok()
    );
    assert_eq!(
        admissions()[0],
        before[0],
        "ProxyRequired called system DNS"
    );
    let observed = events();
    let observed = &observed[start..];
    assert_eq!(observed.iter().filter(|e| e["kind"] == "doh").count(), 2);
    assert!(
        observed
            .iter()
            .filter(|e| e["kind"] == "http")
            .all(|e| e["peer"] == "93.184.215.99")
    );
    assert!(
        observed
            .iter()
            .filter(|e| e["kind"] == "socks")
            .all(|e| e["family"] == 1 || e["family"] == 4)
    );
    checked!(
        "ProxyRequired numeric SOCKS carries DoH bootstrap and target; no system lookup/direct fallback"
    );
    for name in ["mixed.preview.example.org", "private.preview.example.org"] {
        let start = events().len();
        assert_eq!(
            request(&worker, proxy(), &format!("http://{name}/text"))
                .await
                .unwrap_err(),
            Unavailable::Address
        );
        assert!(
            events()[start..]
                .iter()
                .filter(|e| e["kind"] == "socks")
                .all(|e| e["destination"] == "93.184.215.53")
        );
    }
    let mut unavailable_proxy = proxy();
    unavailable_proxy.proxy = Some("socks5://127.0.0.1:1081".into());
    let before = admissions();
    assert!(
        request(
            &worker,
            unavailable_proxy,
            "http://failed.preview.example.org/text"
        )
        .await
        .is_err()
    );
    assert_eq!(admissions()[0], before[0]);
    checked!("Proxy DNS private/mixed and unavailable proxy fail closed without direct fallback");
    *TEST_ROOT.lock().unwrap() = None;

    let (cancel, receiver) = watch::channel(false);
    let old_worker = worker.clone();
    let pending = tokio::spawn(async move {
        fetch(
            old_worker,
            direct(),
            Target::parse("http://93.184.215.14/stall").unwrap(),
            receiver,
        )
        .await
    });
    // A kind unique to the stall request avoids mistaking an earlier request.
    let deadline = Instant::now() + Duration::from_secs(3);
    while !events()
        .iter()
        .any(|e| e["kind"] == "http" && e["path"] == "/stall")
    {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    cancel.send(true).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(4), pending)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    wait_event("tcp_closed").await;
    assert!(
        request(&worker, direct(), "http://93.184.215.14/text")
            .await
            .is_ok()
    );
    checked!(
        "HTTP cancellation closes TCP and fetch returns only after tracked parser retirement; capacity reused"
    );

    let (cancel, receiver) = watch::channel(false);
    let old_worker = worker.clone();
    let pending = tokio::spawn(async move {
        fetch(
            old_worker,
            direct(),
            Target::parse("http://hold.preview.example.org/text").unwrap(),
            receiver,
        )
        .await
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    while !events()
        .iter()
        .any(|e| e["kind"] == "dns" && e["name"] == "hold.preview.example.org")
    {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let before = admissions();
    cancel.send(true).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !pending.is_finished(),
        "libc resolver ownership was dropped on cancellation"
    );
    assert!(
        request(&worker, direct(), "http://93.184.215.14/text")
            .await
            .is_err(),
        "capacity released before actual DNS retirement"
    );
    assert_eq!(admissions()[1..], before[1..]);
    assert_eq!(control("release_dns")["released"], true);
    assert!(
        tokio::time::timeout(Duration::from_secs(8), pending)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_eq!(admissions()[1..], before[1..]);
    assert!(
        request(&worker, direct(), "http://93.184.215.14/text")
            .await
            .is_ok()
    );
    checked!(
        "Cancelled started libc DNS retains capacity until actual return and admits no later I/O"
    );

    let observed = events();
    assert!(
        observed
            .iter()
            .filter(|e| e["kind"] == "http")
            .all(|e| e["destination"] != "127.0.0.1"),
        "private honeypot reached"
    );
    write_new(&directory.join("fetch-result.json"), &serde_json::to_vec_pretty(&json!({"passed":true,"checks":checks,"admissions":admissions(),"scope":"native real sockets and actual isolated content worker; synthetic private no-route namespace; not GUI/browser/platform proof"})).unwrap());
    assert_eq!(control("stop")["stopping"], true);
}
