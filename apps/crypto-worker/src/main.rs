// SPDX-License-Identifier: EUPL-1.2
//! `nuschl-crypto-worker --data DIR`: serves `nuschl_ipc::crypto` requests on
//! stdin/stdout until stdin closes. Started by the session in its sandbox
//! (`nuschl_ipc::host::crypto_sandbox_command`); never logs secrets, and logs
//! nothing at all on stdout.
use nuschl_crypto_worker::{KeySource, Store, Worker};
use nuschl_ipc::{
    Envelope,
    crypto::{CryptoRequest, CryptoResponse},
    read_frame, write_frame,
};
use std::{io, path::PathBuf, process::ExitCode};

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let mut data = None;
    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--data") => data = arguments.next().map(PathBuf::from),
            _ => return Err("usage: nuschl-crypto-worker --data DIR".into()),
        }
    }
    let data = data.ok_or("usage: nuschl-crypto-worker --data DIR")?;
    // Two threads for OpenMLS's TreeKEM work: one per CPU would not fit the
    // sandbox's address-space limit (crates/ipc host.rs) on a large machine.
    nuschl_e2ee::limit_threads(2);
    let store = Store::open(&data, KeySource::File(data.join("store.key")))?;
    let mut worker = Worker::open(store)?;
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    while let Some(request) = read_frame::<CryptoRequest>(&mut input)? {
        let response: CryptoResponse = worker.handle(request.payload);
        write_frame(&mut output, &Envelope::new(request.request_id, response))?;
    }
    Ok(())
}

fn main() -> ExitCode {
    // On Windows, the launch stage starts this worker again in its AppContainer.
    #[cfg(windows)]
    nuschl_sandbox_windows::launch_if_requested();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nuschl-crypto-worker: {error}");
            ExitCode::FAILURE
        }
    }
}
