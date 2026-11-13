// SPDX-License-Identifier: EUPL-1.2
//! Real content confinement and preview supervisor lifecycle checks. Run as a
//! standalone executable; its own `/worker` entry is an intentionally bad child.
use std::{fs::File, io, path::Path, process::Stdio, time::Duration};

use base64::Engine as _;
use nuschl_ipc::{
    ContentRequest, ContentResponse, Envelope,
    host::{self, Error},
    preview::{self, Charset, Metadata, Request, Response},
    read_frame, write_frame,
};

fn document(bytes: &[u8]) -> Request {
    Request::Document {
        schema: preview::SCHEMA,
        body: base64::engine::general_purpose::STANDARD.encode(bytes),
        charset: Charset::Utf8,
        truncated: false,
    }
}

fn fake_worker() {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let Some(request) = read_frame::<ContentRequest>(&mut input).unwrap() else {
        panic!("missing request");
    };
    let ContentRequest::Preview(Request::Document { body, .. }) = request.payload else {
        panic!("unexpected operation");
    };
    let body = base64::engine::general_purpose::STANDARD
        .decode(body)
        .unwrap();
    if body == b"hang" {
        std::thread::sleep(Duration::from_secs(60));
        return;
    }
    if body == b"crash" {
        std::process::exit(1);
    }
    if body == b"oversize" {
        use io::Write as _;
        let length = u32::try_from(preview::MAX_RESPONSE_FRAME_BYTES + 1).unwrap();
        output.write_all(&length.to_be_bytes()).unwrap();
        output.flush().unwrap();
        return;
    }
    if body == b"descendant" || body == b"descendant-live" {
        // A real namespace descendant which closes every protocol pipe. The
        // wrapper's initial command can exit while PID1 must still reap this.
        let mut descendant = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--owned-descendant")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // Main still exits without joining this thread, orphaning the live
        // descendant for the namespace-retirement test. If it exits first,
        // retain its Child and reap it rather than leaving a zombie behind.
        std::thread::spawn(move || {
            let _ = descendant.wait();
        });
    }
    let mut reply = Envelope::new(
        request.request_id,
        ContentResponse::Preview(Response::Metadata {
            schema: preview::SCHEMA,
            metadata: Metadata {
                title: "A title".into(),
                description: String::new(),
                image: None,
                image_alt: String::new(),
            },
        }),
    );
    if body == b"wrong-id" {
        reply.request_id += 1;
    }
    if body == b"wrong-version" {
        reply.version += 1;
    }
    if body == b"wrong-kind" {
        reply.payload = ContentResponse::Rejected("fixed fixture rejection".into());
    }
    if body == b"invalid-metadata" {
        reply.payload = ContentResponse::Preview(Response::Metadata {
            schema: preview::SCHEMA,
            metadata: Metadata {
                title: "bad\nmetadata".into(),
                description: String::new(),
                image: None,
                image_alt: String::new(),
            },
        });
    }
    write_frame(&mut output, &reply).unwrap();
    if body == b"trailing" {
        use io::Write as _;
        output.write_all(b"x").unwrap();
        output.flush().unwrap();
    }
    if body == b"linger" || body == b"progress" || body == b"descendant-live" {
        std::thread::sleep(Duration::from_secs(60));
    }
}

fn children() -> Vec<u32> {
    let mut children = Vec::new();
    for task in std::fs::read_dir("/proc/self/task").unwrap() {
        let text = std::fs::read_to_string(task.unwrap().path().join("children")).unwrap();
        children.extend(
            text.split_whitespace()
                .map(|pid| pid.parse::<u32>().unwrap()),
        );
    }
    children.sort_unstable();
    children.dedup();
    children
}

async fn check(worker: &Path) {
    let fake = std::env::current_exe().unwrap();
    assert!(children().is_empty(), "fixture must start without children");
    for mode in [
        "wrong-id",
        "wrong-version",
        "wrong-kind",
        "invalid-metadata",
        "trailing",
        "oversize",
        "crash",
        "linger",
        "descendant-live",
    ] {
        let answer = host::preview(&fake, document(mode.as_bytes())).await;
        match mode {
            "wrong-id" | "wrong-version" | "wrong-kind" | "invalid-metadata" | "trailing" => {
                assert!(matches!(answer, Err(Error::InvalidResponse)));
            }
            "oversize" => assert!(matches!(
                answer,
                Err(Error::Frame(nuschl_ipc::Error::TooLarge))
            )),
            "crash" => assert!(matches!(answer, Err(Error::Io(_)))),
            "linger" | "descendant-live" => assert!(matches!(answer, Err(Error::Timeout))),
            _ => unreachable!(),
        }
        assert!(children().is_empty(), "unreaped child after {mode}");
    }

    // Prepare once; rendering those exact carried bytes must not compete with
    // the extraction/preparation owner held by the next cancellation case.
    let prepare = Request::PrepareImage {
        schema: preview::SCHEMA,
        image: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGM4EaABAAMkAUEdGZ/EAAAAAElFTkSuQmCC".into(),
        alt: "known pixel".into(),
    };
    let Response::Image {
        image: prepared_image,
        ..
    } = host::preview(worker, prepare.clone()).await.unwrap()
    else {
        panic!("expected a prepared carried image");
    };

    // Cancel the caller after a real confined child has started. Its supervisor
    // must keep the preparation slot until timeout and actual retirement.
    let hanging = fake.clone();
    let caller = tokio::spawn(async move { host::preview(&hanging, document(b"hang")).await });
    tokio::time::timeout(Duration::from_secs(3), async {
        while children().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    assert!(matches!(
        host::preview(worker, document(b"<title>ok</title>")).await,
        Err(Error::Busy)
    ));
    assert!(!children().is_empty(), "cancel dropped the live supervisor");
    let preparation_busy = matches!(host::preview(worker, prepare).await, Err(Error::Busy));
    let render_request = Request::RenderImage {
        schema: preview::SCHEMA,
        image: prepared_image.clone(),
    };
    let mut rendering = std::pin::pin!(host::preview(worker, render_request.clone()));
    // Poll admission once on this current-thread runtime, before yielding to
    // either supervisor. The old shared slot returns Ready(Busy) here; separate
    // render admission is Pending and retains its own one-worker credit.
    let first_poll = std::future::poll_fn(|cx| {
        std::task::Poll::Ready(std::future::Future::poll(rendering.as_mut(), cx))
    })
    .await;
    let render_admitted = first_poll.is_pending();
    let document_still_busy = matches!(
        host::preview(worker, document(b"<title>still held</title>")).await,
        Err(Error::Busy)
    );
    let second_render_busy = matches!(
        host::preview(worker, render_request).await,
        Err(Error::Busy)
    );
    let rendered = match first_poll {
        std::task::Poll::Ready(reply) => reply,
        std::task::Poll::Pending => rendering.await,
    };
    // Retire the held Document before asserting the concurrency observations,
    // so the shared-slot negative control also finishes with no live child.
    let result = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            match host::preview(worker, document(b"<title>Recovered</title>")).await {
                Err(Error::Busy) => tokio::time::sleep(Duration::from_millis(25)).await,
                answer => break answer,
            }
        }
    })
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(result, Response::Metadata { metadata, .. } if metadata.title == "Recovered"));
    assert!(children().is_empty(), "unreaped child after recovery");
    assert!(preparation_busy, "PrepareImage escaped the Document slot");
    assert!(
        document_still_busy,
        "render admission released Document credit"
    );
    assert!(render_admitted, "RenderImage competed for Document credit");
    assert!(
        second_render_busy,
        "a second render escaped its one-worker cap"
    );
    let Response::Pixels { pixels, .. } = rendered.unwrap() else {
        panic!("expected source-free pixels from the carried bytes");
    };
    assert_eq!((pixels.width, pixels.height), (1, 1));
    assert_eq!(pixels.decode(), Some(vec![200, 80, 40, 255]));

    // Extraction returns an inert image reference; it must not retrieve it.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let image = format!("http://{}/image.png", listener.local_addr().unwrap());
    let html = format!(
        "<meta property='og:title' content='Sender title'><meta property='og:image' content='{image}'>"
    );
    let result = host::preview(worker, document(html.as_bytes()))
        .await
        .unwrap();
    assert!(
        matches!(result, Response::Metadata { metadata, .. } if metadata.title == "Sender title" && metadata.image.as_deref() == Some(image.as_str()))
    );
    assert!(matches!(listener.accept(), Err(e) if e.kind() == io::ErrorKind::WouldBlock));
    assert!(children().is_empty(), "unreaped actual content worker");

    let mut wrong = prepared_image;
    wrong.width = 2;
    assert_eq!(
        host::preview(
            worker,
            Request::RenderImage {
                schema: preview::SCHEMA,
                image: wrong
            }
        )
        .await
        .unwrap(),
        Response::Unavailable {
            schema: preview::SCHEMA,
            reason: preview::Unavailable::Malformed
        }
    );
    assert!(children().is_empty(), "unreaped image operation worker");

    observed_namespace_retirement(&fake).await;
    unknown_startup_remains_pending(&fake).await;
}

/// Hold our own observation of the ORIGINAL PID1 proc inode. This is stronger
/// than interpreting a snapshot of descendants as an entire-tree proof.
fn owned_init(wrapper: u32) -> File {
    let path = format!("/proc/{wrapper}/task/{wrapper}/children");
    let children = std::fs::read_to_string(path).unwrap();
    let ids: Vec<u32> = children
        .split_ascii_whitespace()
        .map(|s| s.parse().unwrap())
        .collect();
    assert_eq!(
        ids.len(),
        1,
        "one namespace init is expected after readiness"
    );
    File::open(format!("/proc/{}/stat", ids[0])).unwrap()
}

fn original_init_has_retired(stat: &mut File) -> bool {
    use io::{Read as _, Seek as _, SeekFrom};
    stat.seek(SeekFrom::Start(0)).unwrap();
    let mut bytes = Vec::new();
    match stat.take(4097).read_to_end(&mut bytes) {
        Err(error) if error.raw_os_error() == Some(3) => true,
        Ok(_) => {
            assert!(bytes.len() <= 4096);
            let text = std::str::from_utf8(&bytes).unwrap();
            text.rsplit_once(") ").unwrap().1.starts_with("Z ")
        }
        Err(error) => panic!("ambiguous original init observation: {error}"),
    }
}

async fn observed_namespace_retirement(fake: &Path) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    for mode in ["descendant", "descendant-live", "progress"] {
        let mut child = host::preview_sandbox_command(fake, &[])
            .unwrap()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let wrapper = child.id().unwrap();
        let mut retirement = host::preview_lifecycle::Retirement::new(&mut child);
        retirement.ready().await.unwrap();
        let mut init = owned_init(wrapper);
        assert!(!original_init_has_retired(&mut init));
        let mut frame = Vec::new();
        write_frame(
            &mut frame,
            &Envelope::new(1, ContentRequest::Preview(document(mode.as_bytes()))),
        )
        .unwrap();
        let mut input = child.stdin.take().unwrap();
        input.write_all(&frame).await.unwrap();
        input.shutdown().await.unwrap();
        drop(input);
        let mut output = child.stdout.take().unwrap();
        // An actual bounded reply proves the command consumed the input before
        // we cancel the outer monitor. The descendant has already been spawned.
        let length = output.read_u32().await.unwrap() as usize;
        assert!(length > 0 && length <= preview::MAX_RESPONSE_FRAME_BYTES);
        let mut reply = vec![0; length];
        output.read_exact(&mut reply).await.unwrap();
        let reply: Envelope<ContentResponse> = serde_json::from_slice(&reply).unwrap();
        assert_eq!(reply.request_id, 1);
        assert!(matches!(
            reply.payload,
            ContentResponse::Preview(Response::Metadata { .. })
        ));
        tokio::time::timeout(Duration::from_secs(5), retirement.retire(&mut child))
            .await
            .unwrap();
        assert!(
            original_init_has_retired(&mut init),
            "role credit returned before original PID1 retirement"
        );
        assert!(child.try_wait().unwrap().is_some());
        assert!(children().is_empty(), "owned wrapper was not reaped");
    }
}

async fn unknown_startup_remains_pending(fake: &Path) {
    for mode in ["malformed", "oversized", "identity"] {
        // This trusted fixture helper does not spawn anything and receives no
        // data. Its wrapper is therefore the whole owned test process; after
        // reaping it we may cancel the intentionally-pending observation test.
        let mut child = tokio::process::Command::new(fake)
            .arg("--status-fixture")
            .arg(mode)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut retirement = host::preview_lifecycle::Retirement::new(&mut child);
        assert!(retirement.ready().await.is_err());
        child.kill().await.unwrap();
        assert!(child.wait().await.is_ok());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), retirement.retire(&mut child))
                .await
                .is_err(),
            "unknown startup was incorrectly credited as retired"
        );
        assert!(
            child.try_wait().unwrap().is_some(),
            "trusted no-descendant control was not reaped"
        );
    }
}

fn status_fixture(mode: &str) {
    use io::Write as _;
    let mut stderr = io::stderr().lock();
    match mode {
        "malformed" => stderr.write_all(b"{bad}\n").unwrap(),
        "oversized" => stderr.write_all(&[b'x'; 2049]).unwrap(),
        // A well-formed record cannot claim the monitor itself as its init.
        "identity" => writeln!(
            stderr,
            "{{\"child-pid\":{},\"pid-namespace\":1}}",
            std::process::id()
        )
        .unwrap(),
        _ => panic!("unexpected fixed fixture mode"),
    }
    stderr.flush().unwrap();
    std::thread::sleep(Duration::from_secs(60));
}

fn main() {
    let arguments: Vec<_> = std::env::args().collect();
    if arguments.get(1).is_some_and(|s| s == "--owned-descendant") {
        std::thread::sleep(Duration::from_secs(60));
        return;
    }
    if arguments.get(1).is_some_and(|s| s == "--status-fixture") {
        status_fixture(arguments.get(2).expect("fixed status fixture mode"));
        return;
    }
    if std::env::current_exe().is_ok_and(|path| path == Path::new("/worker")) {
        fake_worker();
        return;
    }
    if !cfg!(target_os = "linux") {
        panic!("this confinement fixture requires Linux");
    }
    let worker = std::env::args_os()
        .nth(1)
        .expect("actual content worker path");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(check(Path::new(&worker)));
    println!("preview: bounded confined replies, cancellation retirement and recovery ... ok");
}
