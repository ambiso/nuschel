// SPDX-License-Identifier: EUPL-1.2
//! A batch of content requests against a misbehaving worker. This test binary
//! is its own fake worker when it runs inside the sandbox, as `/worker`.
use std::{
    io,
    time::{Duration, Instant},
};

use nuschel_ipc::{
    ContentRequest, ContentResponse, Envelope,
    host::{self, Error},
    read_frame, write_frame,
};

fn main() {
    if std::env::current_exe().is_ok_and(|path| path == std::path::Path::new("/worker")) {
        fake_worker();
    } else if cfg!(target_os = "linux") {
        a_failing_request_costs_only_itself();
        println!("exchange: a failing request costs only itself ... ok");
    }
}

/// Echoes each text (as an `Encoded` reply), except: "crash" exits, "hang"
/// never answers, "garble" answers out of turn and "linger" stays after
/// its last reply.
fn fake_worker() {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut linger = false;
    while let Ok(Some(request)) = read_frame::<ContentRequest>(&mut input) {
        let ContentRequest::Render { text } = request.payload else {
            std::process::exit(2);
        };
        let id = match text.as_str() {
            "crash" => std::process::exit(1),
            "hang" => loop {
                std::thread::sleep(Duration::from_secs(60));
            },
            "garble" => request.request_id + 7,
            "linger" => {
                linger = true;
                request.request_id
            }
            _ => request.request_id,
        };
        let reply = Envelope::new(id, ContentResponse::Encoded(text));
        if write_frame(&mut output, &reply).is_err() {
            return;
        }
    }
    if linger {
        std::thread::sleep(Duration::from_secs(60));
    }
}

fn a_failing_request_costs_only_itself() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let executable = std::env::current_exe().unwrap();
    let texts = ["a", "crash", "b", "garble", "c", "hang", "d", "linger"];
    let requests: Vec<_> = texts
        .iter()
        .map(|text| ContentRequest::Render {
            text: text.to_string(),
        })
        .collect();
    let started = Instant::now();
    let results = runtime
        .block_on(host::exchange(&executable, &requests))
        .unwrap();
    let elapsed = started.elapsed();
    let outcome: Vec<String> = results
        .iter()
        .map(|result| match result {
            Ok(ContentResponse::Encoded(text)) => text.clone(),
            Ok(_) => "?".into(),
            Err(Error::Io(_)) => "exited".into(),
            Err(Error::InvalidResponse) => "invalid".into(),
            Err(Error::Timeout) => "timeout".into(),
            Err(error) => error.to_string(),
        })
        .collect();
    assert_eq!(
        outcome,
        [
            "a", "exited", "b", "invalid", "c", "timeout", "d", "timeout"
        ]
    );
    // Only the hanging request waited out its five seconds, and the lingering
    // worker its two to exit.
    assert!(elapsed < Duration::from_secs(11), "{elapsed:?}");
    // Every worker was reaped: no child, zombie or not, is left.
    for task in std::fs::read_dir("/proc/self/task").unwrap() {
        let children = std::fs::read_to_string(task.unwrap().path().join("children")).unwrap();
        assert_eq!(children.trim(), "", "leftover worker processes");
    }
}
