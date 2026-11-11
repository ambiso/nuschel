// SPDX-License-Identifier: EUPL-1.2
//! `nuschel-crypto-worker --data DIR`: serves `nuschel_ipc::crypto` requests on
//! stdin/stdout until stdin closes. Started by the session in its sandbox
//! (`nuschel_ipc::host::crypto_sandbox_command`); never logs secrets, and logs
//! nothing at all on stdout.
use nuschel_crypto_worker::{KeySource, Store, Worker};
use nuschel_ipc::{
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
            _ => return Err("usage: nuschel-crypto-worker --data DIR".into()),
        }
    }
    let data = data.ok_or("usage: nuschel-crypto-worker --data DIR")?;
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
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nuschel-crypto-worker: {error}");
            ExitCode::FAILURE
        }
    }
}
