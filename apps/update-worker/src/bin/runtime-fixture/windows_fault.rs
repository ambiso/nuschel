// SPDX-License-Identifier: EUPL-1.2
//! Native, test-only adversarial verifier entry points. This module is compiled
//! solely into runtime-fixture, never either distributed verifier or signer.
use nuschl_update::{
    VerifierPaths,
    auth_format::{PublicKey, Signature},
    verifier_wire::{Algorithm, MAX_REQUEST_BYTES, Request},
};
use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::Path,
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub fn dispatch() -> Result<bool> {
    let executable = std::env::current_exe()?;
    let name = executable
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let Some(mode) = name.strip_prefix("nuschl-update-fixture-") else {
        return Ok(false);
    };
    let (mode, port) = mode.rsplit_once('-').ok_or("invalid fault fixture name")?;
    let port: u16 = port.parse()?;
    match mode {
        "stall" => {
            std::thread::sleep(Duration::from_secs(30));
            return Err("stalled verifier was not terminated".into());
        }
        "spoof" | "overflow" => {}
        _ => return Err("unknown test-only fault mode".into()),
    }
    let mut frame = Vec::new();
    std::io::stdin()
        .lock()
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut frame)?;
    if frame.len() > MAX_REQUEST_BYTES {
        return Err("oversized request".into());
    }
    let algorithm = match frame.get(8) {
        Some(1) => Algorithm::Ed25519,
        Some(2) => Algorithm::MlDsa44,
        _ => return Err("invalid test request algorithm".into()),
    };
    let request = Request::read(frame.as_slice(), algorithm)?;
    if mode == "overflow" {
        std::io::stdout().lock().write_all(&[b'x'; 10000])?;
        return Ok(true);
    }

    // The launch grants the EXE itself, not its sibling files or parent folder.
    // A valid forged reply below proves all these actual attempts ran and were
    // denied; a crash or bootstrap failure cannot count as sandbox evidence.
    let canary = executable
        .parent()
        .ok_or("fault worker has no parent")?
        .join("canary");
    if fs::read(&canary).is_ok() {
        return Err("private file readable inside verifier LPAC".into());
    }
    if fs::write(canary.with_extension("write"), b"must be denied").is_ok() {
        return Err("private parent directory writable inside verifier LPAC".into());
    }
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    if TcpStream::connect_timeout(&address, Duration::from_millis(500)).is_ok() {
        return Err("guest network reachable inside verifier LPAC".into());
    }
    // Deliberately skip asymmetric verification: compromise of one algorithm
    // process still must not authorize a corrupt signature in its sibling.
    request.accepted(std::io::stdout().lock())?;
    Ok(true)
}

pub fn exercise(
    directory: &Path,
    port: u16,
    fixtures: (&[u8], &Signature, &Signature, &Signature),
    key: &PublicKey,
    workers: &VerifierPaths,
    now: u64,
) -> Result<()> {
    let (raw, signed, bad_ed, bad_ml) = fixtures;
    let verify = |signature: &Signature, paths: &VerifierPaths| {
        nuschl_update::verify_hybrid(
            raw,
            signature.as_bytes(),
            std::slice::from_ref(key),
            paths,
            now,
        )
    };
    let own = std::env::current_exe()?;
    let fault = |mode| -> Result<_> {
        let path = directory.join(format!("nuschl-update-fixture-{mode}-{port}.exe"));
        fs::copy(&own, &path)?;
        Ok(path)
    };
    let spoof = fault("spoof")?;
    let mut compromised = workers.clone();
    compromised.ed25519 = spoof.clone();
    verify(signed, &compromised)?;
    assert!(verify(bad_ml, &compromised).is_err());
    compromised = workers.clone();
    compromised.mldsa44 = spoof;
    verify(signed, &compromised)?;
    assert!(verify(bad_ed, &compromised).is_err());
    assert!(!directory.join("canary.write").exists());

    // Exercise both distinct production LPAC identities for malicious output
    // and stalls, not merely a portable simulation of the launcher.
    for post_quantum in [false, true] {
        for mode in ["overflow", "stall"] {
            let mut paths = workers.clone();
            let executable = fault(mode)?;
            if post_quantum {
                paths.mldsa44 = executable.clone();
            } else {
                paths.ed25519 = executable.clone();
            }
            let start = Instant::now();
            assert!(verify(signed, &paths).is_err());
            assert!(start.elapsed() < Duration::from_secs(10));
            // Windows forbids deleting an image still mapped by a live child.
            // This makes successful child reaping observable after each case.
            fs::remove_file(executable)?;
        }
    }
    Ok(())
}
