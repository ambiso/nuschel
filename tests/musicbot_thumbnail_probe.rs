// SPDX-License-Identifier: EUPL-1.2
//! Test-only real/faulting content-worker harness; uses the production supervisor.
use nuschl_core::{ContentBlock, MessageDocument};
use nuschl_ipc::{ContentRequest, ContentResponse, Envelope};
use std::{
    io::{self, Read, Write},
    path::Path,
    time::{Duration, Instant},
};

fn fault_worker() {
    let mut input = io::stdin().lock();
    let request = nuschl_ipc::read_frame::<ContentRequest>(&mut input)
        .unwrap()
        .unwrap();
    let ContentRequest::Thumbnail { image, .. } = request.payload else {
        std::process::exit(2);
    };
    let mode = match image.as_str() {
        "dmFsaWQ=" => "valid",
        "bGluZ2Vy" => "linger",
        "Y3Jhc2g=" => "crash",
        "YWR2ZXJ0aXNl" => "advertise",
        "ZHVwbGljYXRl" => "duplicate",
        "d3JvbmctaWQ=" => "wrong-id",
        "ZGltZW5zaW9ucw==" => "dimensions",
        "Ynl0ZXM=" => "bytes",
        "dHJhaWxpbmctYml0cw==" => "trailing-bits",
        "dHdv" => "two",
        _ => "hang",
    };
    if mode == "hang" {
        std::thread::sleep(Duration::from_secs(60));
        return;
    }
    if mode == "crash" {
        std::process::exit(1);
    }
    let mut output = io::stdout().lock();
    if mode == "advertise" {
        output.write_all(&65536u32.to_be_bytes()).unwrap();
        output.flush().unwrap();
        return;
    }
    let image = || ContentBlock::Image {
        width: if mode == "dimensions" { 2 } else { 1 },
        height: 1,
        rgba: match mode {
            "bytes" => "AA==",
            "trailing-bits" => "AQIDBB==",
            _ => "AQIDBA==",
        }
        .into(),
        source: Some("https://never-fetch.invalid/not-authority".into()),
    };
    let response = || {
        ContentResponse::Rendered(MessageDocument {
            blocks: if mode == "two" {
                vec![image(), image()]
            } else {
                vec![image()]
            },
        })
    };
    nuschl_ipc::write_frame(
        &mut output,
        &Envelope::new(if mode == "wrong-id" { 2 } else { 1 }, response()),
    )
    .unwrap();
    if mode == "duplicate" {
        nuschl_ipc::write_frame(&mut output, &Envelope::new(1, response())).unwrap();
    }
    output.flush().unwrap();
    if mode == "linger" {
        std::thread::sleep(Duration::from_secs(60));
    }
}

fn main() {
    if std::env::current_exe().is_ok_and(|path| path == Path::new("/worker")) {
        fault_worker();
        return;
    }
    let args: Vec<_> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("valid");
    let own = std::env::current_exe().unwrap();
    let (worker, bytes, edge) = if mode == "real" {
        assert_eq!(args.len(), 4);
        let mut bytes = Vec::new();
        std::fs::File::open(&args[3])
            .unwrap()
            .take((nuschl_ipc::MAX_THUMBNAIL_INPUT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .unwrap();
        (std::path::PathBuf::from(&args[2]), bytes, 400)
    } else if mode == "oversize" {
        (own, vec![0; nuschl_ipc::MAX_THUMBNAIL_INPUT_BYTES + 1], 1)
    } else if mode == "missing" {
        (
            std::path::PathBuf::from("/does-not-exist/owned-thumbnail-fixture"),
            b"valid".to_vec(),
            1,
        )
    } else {
        (own, mode.as_bytes().to_vec(), 1)
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let start = Instant::now();
    let result = runtime.block_on(nuschl_ipc::host::thumbnail(&worker, &bytes, edge, edge));
    let (kind, width, height) = match result {
        Ok(ContentBlock::Image {
            width,
            height,
            source: None,
            ..
        }) => ("accepted", width, height),
        Ok(_) => panic!("worker authority/shape escaped validation"),
        Err(nuschl_ipc::host::Error::Frame(nuschl_ipc::Error::TooLarge)) => ("too_large", 0, 0),
        Err(nuschl_ipc::host::Error::MissingSandboxHelper(_)) => ("missing_helper", 0, 0),
        Err(nuschl_ipc::host::Error::Timeout) => ("timeout", 0, 0),
        Err(_) => ("rejected", 0, 0),
    };
    println!(
        "{{\"mode\":\"{mode}\",\"kind\":\"{kind}\",\"width\":{width},\"height\":{height},\"elapsed_ms\":{}}}",
        start.elapsed().as_millis()
    );
}
