// SPDX-License-Identifier: EUPL-1.2
//! Native, test-only adversarial verifier entry points. This module is compiled
//! solely into runtime-fixture, never either distributed verifier or signer.
use nuschl_update::{
    VerifierPaths,
    auth_format::{PublicKey, Signature, signed_bytes},
    verifier_wire::{Algorithm, MAX_REQUEST_BYTES, Request},
};
use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::Path,
    process::{Command, Stdio},
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
    match std::panic::catch_unwind(|| {
        TcpStream::connect_timeout(&address, Duration::from_millis(500))
    }) {
        Ok(Ok(_)) => return Err("guest network reachable inside verifier LPAC".into()),
        Ok(Err(_)) => {}
        Err(panic) => {
            // The pinned Windows std asserts WSAStartup success. LPAC returns
            // WSASYSCALLFAILURE before a socket exists. Accept only this exact
            // observed initialization denial, never an unrelated child crash.
            let expected = "assertion `left == right` failed\n  left: 10107\n right: 0";
            if panic.downcast_ref::<String>().map(String::as_str) != Some(expected) {
                std::panic::resume_unwind(panic);
            }
        }
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
    if let Err(error) = verify(signed, &compromised) {
        diagnostic(&spoof, raw, signed, key)?;
        return Err(error.into());
    }
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
            println!("Native LPAC adversary role={post_quantum} mode={mode}");
            let mut paths = workers.clone();
            let executable = fault(mode)?;
            if post_quantum {
                paths.mldsa44 = executable.clone();
            } else {
                paths.ed25519 = executable.clone();
            }
            let start = Instant::now();
            assert!(verify(signed, &paths).is_err());
            // Windows forbids deleting an image still mapped by a live child.
            // Job termination is asynchronous after the launcher's wait, so
            // allow a bounded 1s for the image mapping to be released.
            let cleanup_deadline = Instant::now() + Duration::from_secs(1);
            loop {
                match fs::remove_file(&executable) {
                    Ok(()) => break,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::PermissionDenied
                            && Instant::now() < cleanup_deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(error) => {
                        return Err(format!(
                            "{mode} image still mapped after bounded reap: {error}"
                        )
                        .into());
                    }
                }
            }
            assert!(start.elapsed() < Duration::from_secs(10));
        }
    }
    Ok(())
}

// Failure diagnostics only, never acceptance evidence: the real test above
// always uses verify_hybrid's production launcher, containment and strict reply.
// This bounded extra launch exposes native errors without changing production
// stderr suppression or granting the worker any additional authority.
fn diagnostic(executable: &Path, raw: &[u8], signature: &Signature, key: &PublicKey) -> Result<()> {
    let request = Request::new(
        Algorithm::Ed25519,
        key.ed25519(),
        signature.ed25519(),
        &signed_bytes(key, raw)?,
    )?;
    let bytes = request.encode();
    assert!(bytes.len() < 4096);
    let mut command = Command::new(executable);
    command.env_clear();
    for name in ["SystemRoot", "WINDIR", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.args([
        "--nuschl-sandbox",
        "--profile",
        "nuschl.update.ed25519",
        "--memory",
        "268435456",
        "--cpu",
        "5",
        "--less-privileged",
        "--",
    ]);
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("diagnostic stdin absent")?
        .write_all(&bytes)?;
    let deadline = Instant::now() + Duration::from_secs(6);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill()?;
            break child.wait()?;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .ok_or("diagnostic stderr absent")?
        .take(4096)
        .read_to_string(&mut stderr)?;
    eprintln!("Non-accepting fault diagnostic {status}: {stderr}");
    Ok(())
}
