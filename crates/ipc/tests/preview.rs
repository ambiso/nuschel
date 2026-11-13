// SPDX-License-Identifier: EUPL-1.2
//! Real content confinement and preview supervisor lifecycle checks. Run as a
//! standalone executable; its own `/worker` entry is an intentionally bad child.
use std::{io, path::Path, time::Duration};

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
    let body = base64::engine::general_purpose::STANDARD.decode(body).unwrap();
    if body == b"hang" {
        std::thread::sleep(Duration::from_secs(60));
        return;
    }
    if body == b"crash" {
        std::process::exit(1);
    }
    if body == b"oversize" {
        use io::Write as _;
        output.write_all(&((preview::MAX_RESPONSE_FRAME_BYTES + 1) as u32).to_be_bytes()).unwrap();
        output.flush().unwrap();
        return;
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
    write_frame(&mut output, &reply).unwrap();
    if body == b"linger" {
        std::thread::sleep(Duration::from_secs(60));
    }
}

fn children() -> Vec<u32> {
    let mut children = Vec::new();
    for task in std::fs::read_dir("/proc/self/task").unwrap() {
        let text = std::fs::read_to_string(task.unwrap().path().join("children")).unwrap();
        children.extend(text.split_whitespace().map(|pid| pid.parse::<u32>().unwrap()));
    }
    children.sort_unstable();
    children.dedup();
    children
}

async fn check(worker: &Path) {
    let fake = std::env::current_exe().unwrap();
    assert!(children().is_empty(), "fixture must start without children");
    for mode in ["wrong-id", "wrong-version", "oversize", "crash", "linger"] {
        let answer = host::preview(&fake, document(mode.as_bytes())).await;
        match mode {
            "wrong-id" | "wrong-version" => assert!(matches!(answer, Err(Error::InvalidResponse))),
            "oversize" => assert!(matches!(answer, Err(Error::Frame(nuschl_ipc::Error::TooLarge)))),
            "crash" => assert!(matches!(answer, Err(Error::Io(_)))),
            "linger" => assert!(matches!(answer, Err(Error::Timeout))),
            _ => unreachable!(),
        }
        assert!(children().is_empty(), "unreaped child after {mode}");
    }

    // Cancel the caller after a real confined child has started. Its supervisor
    // must keep the only process slot until timeout and actual retirement.
    let caller = tokio::spawn(async move { host::preview(&fake, document(b"hang")).await });
    tokio::time::timeout(Duration::from_secs(3), async {
        while children().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    let started = tokio::time::Instant::now();
    assert!(matches!(host::preview(worker, document(b"<title>ok</title>")).await, Err(Error::Busy)));
    let result = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            match host::preview(worker, document(b"<title>Recovered</title>")).await {
                Err(Error::Busy) => tokio::time::sleep(Duration::from_millis(25)).await,
                answer => break answer,
            }
        }
    }).await.unwrap().unwrap();
    assert!(started.elapsed() >= Duration::from_secs(4), "cancel released the process slot early");
    assert!(matches!(result, Response::Metadata { metadata, .. } if metadata.title == "Recovered"));
    assert!(children().is_empty(), "unreaped child after recovery");

    // Extraction returns an inert image reference; it must not retrieve it.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let image = format!("http://{}/image.png", listener.local_addr().unwrap());
    let html = format!("<meta property='og:title' content='Sender title'><meta property='og:image' content='{image}'>");
    let result = host::preview(worker, document(html.as_bytes())).await.unwrap();
    assert!(matches!(result, Response::Metadata { metadata, .. } if metadata.title == "Sender title" && metadata.image.as_deref() == Some(image.as_str())));
    assert!(matches!(listener.accept(), Err(e) if e.kind() == io::ErrorKind::WouldBlock));
    assert!(children().is_empty(), "unreaped actual content worker");
}

fn main() {
    if std::env::current_exe().is_ok_and(|path| path == Path::new("/worker")) {
        fake_worker();
        return;
    }
    assert!(cfg!(target_os = "linux"), "this confinement fixture requires Linux");
    let worker = std::env::args_os().nth(1).expect("actual content worker path");
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(check(Path::new(&worker)));
    println!("preview: bounded confined replies, cancellation retirement and recovery ... ok");
}
