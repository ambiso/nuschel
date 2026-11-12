// SPDX-License-Identifier: EUPL-1.2
#![forbid(unsafe_code)]
//! Non-distributed test executable. `runtime-fixture` is never enabled in the
//! client, verifier or operator signer packages. All keys are temporary fixtures.
use nuschl_update::{
    Error, VerifierPaths,
    auth_format::{PublicKey, Signature},
};
use nuschl_update_worker::key_file;
use std::{
    fs,
    io::{Read, Write},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 3 {
        return Err("fixture requires ED_WORKER ML_WORKER PYTHON".into());
    }
    let workers = VerifierPaths {
        ed25519: PathBuf::from(&args[0]),
        mldsa44: PathBuf::from(&args[1]),
    };
    let dir = tempfile::tempdir()?;
    let secret_path = dir.path().join("fixture-secret.json");
    let public_path = dir.path().join("fixture-public.json");
    let key = key_file::generate(&secret_path, &public_path)?;
    let secret = key_file::Secret::read(&secret_path)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let manifest = |url: &str, digest: &str, expires| {
        serde_json::to_vec(&serde_json::json!({
        "format":2,"algorithm":"ed25519+mldsa44","version":"9.1.0",
        "released":now-10,"expires":expires,"notes":"fixture\nnew line",
        "platforms":{"windows-x86_64":[{"path":"nuschl.exe","url":url,"sha256":digest,"size":11}]}
    })).unwrap()
    };
    let raw = manifest(
        "https://example.invalid/nuschl.exe",
        &"00".repeat(32),
        now + 3600,
    );
    let signed = secret.sign(&raw)?;
    let verify =
        |bytes: &[u8], signature: &Signature, keys: &[PublicKey], paths: &VerifierPaths| {
            nuschl_update::verify_hybrid(bytes, signature.as_bytes(), keys, paths, now)
        };
    verify(&raw, &signed, std::slice::from_ref(&key), &workers)?;
    let mut ed = *signed.ed25519();
    ed[0] ^= 1;
    let bad_ed = Signature::from_components(&key, &ed, signed.mldsa44())?;
    assert!(verify(&raw, &bad_ed, std::slice::from_ref(&key), &workers).is_err());
    let mut ml = *signed.mldsa44();
    ml[0] ^= 1;
    let bad_ml = Signature::from_components(&key, signed.ed25519(), &ml)?;
    assert!(verify(&raw, &bad_ml, std::slice::from_ref(&key), &workers).is_err());
    let mut changed = raw.clone();
    changed.push(b' ');
    assert!(verify(&changed, &signed, std::slice::from_ref(&key), &workers).is_err());
    let other = PublicKey::from_bytes(&[3; nuschl_update::auth_format::PUBLIC_BYTES])?;
    assert!(verify(&raw, &signed, &[other], &workers).is_err());
    assert!(matches!(
        verify(&raw, &signed, &[], &workers),
        Err(Error::Unenrolled)
    ));
    assert!(
        nuschl_update::verify_hybrid(
            &raw,
            b"untrusted comment: minisign",
            std::slice::from_ref(&key),
            &workers,
            now
        )
        .is_err()
    );
    let expired = manifest(
        "https://example.invalid/nuschl.exe",
        &"00".repeat(32),
        now - 1,
    );
    assert!(matches!(
        verify(
            &expired,
            &secret.sign(&expired)?,
            std::slice::from_ref(&key),
            &workers
        ),
        Err(Error::Expired)
    ));
    let mut duplicate = raw.clone();
    duplicate.splice(1..1, br#""format":2,"#.iter().copied());
    assert!(matches!(
        verify(
            &duplicate,
            &secret.sign(&duplicate)?,
            std::slice::from_ref(&key),
            &workers
        ),
        Err(Error::Manifest(_))
    ));
    let mut downgrade = signed.as_bytes().to_vec();
    downgrade[9] = 1;
    assert!(
        nuschl_update::verify_hybrid(&raw, &downgrade, std::slice::from_ref(&key), &workers, now)
            .is_err()
    );
    println!(
        "PASS: two real confined engines, either-component corruption, raw bytes, wrong pair, absent trust, legacy/downgrade, expiry, duplicate fields"
    );

    // An actually confined adversarial engine can spoof its own success. It
    // still cannot bless a corrupt sibling signature. The probe also attempts
    // reads/writes/network in the very same production sandbox policy.
    let canary = dir.path().join("canary");
    fs::write(&canary, "private fixture")?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let spoof = dir.path().join("spoof.py");
    let script = format!(
        r#"#!{}
import sys,hashlib,socket
canary={:?}
try:
 open(canary,'rb').read()
 raise AssertionError('private file readable')
except (FileNotFoundError,PermissionError): pass
try:
 open(canary+'.write','wb').write(b'bad')
 raise AssertionError('private directory writable')
except (FileNotFoundError,PermissionError): pass
s=socket.socket();s.settimeout(.2)
try:
 s.connect(('127.0.0.1',{}))
 raise AssertionError('host network reachable')
except OSError: pass
b=sys.stdin.buffer.read({})
sys.stdout.buffer.write(b'NUSUVP2\0'+b[8:9]+hashlib.sha256(b'nuschl.update.verifier.reply.v2\0'+b).digest())
"#,
        PathBuf::from(&args[2]).display(),
        canary.to_str().ok_or("non UTF8 fixture path")?,
        port,
        nuschl_update::verifier_wire::MAX_REQUEST_BYTES + 1
    );
    fs::write(&spoof, script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&spoof, fs::Permissions::from_mode(0o755))?;
    }
    let mut compromised = workers.clone();
    compromised.ed25519 = spoof.clone();
    verify(&raw, &signed, std::slice::from_ref(&key), &compromised)?;
    assert!(verify(&raw, &bad_ml, std::slice::from_ref(&key), &compromised).is_err());
    compromised = workers.clone();
    compromised.mldsa44 = spoof.clone();
    verify(&raw, &signed, std::slice::from_ref(&key), &compromised)?;
    assert!(verify(&raw, &bad_ed, std::slice::from_ref(&key), &compromised).is_err());
    fs::write(
        &spoof,
        format!(
            "#!{}\nimport sys\nsys.stdin.buffer.read()\nsys.stdout.buffer.write(b'x'*10000)\n",
            PathBuf::from(&args[2]).display()
        ),
    )?;
    assert!(verify(&raw, &signed, std::slice::from_ref(&key), &compromised).is_err());
    fs::write(
        &spoof,
        format!(
            "#!{}\nimport time\ntime.sleep(30)\n",
            PathBuf::from(&args[2]).display()
        ),
    )?;
    let start = std::time::Instant::now();
    assert!(verify(&raw, &signed, std::slice::from_ref(&key), &compromised).is_err());
    assert!(start.elapsed().as_secs() < 10);
    println!(
        "PASS: actual sandbox file/write/network probes, mandatory AND despite one forged answer, bounded output and timeout/reaping"
    );

    use sha2::{Digest, Sha256};
    let body = b"new program";
    let base = format!("http://127.0.0.1:{port}/");
    let hex: String = Sha256::digest(body)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let raw = manifest(&format!("{base}nuschl.exe"), &hex, now + 3600);
    let sig = secret.sign(&raw)?.as_bytes().to_vec();
    let server = std::thread::spawn(move || {
        for stream in listener.incoming().take(3) {
            let mut stream = stream.unwrap();
            let mut request = [0; 2048];
            let n = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..n]);
            let path = request.split_whitespace().nth(1).unwrap();
            let reply = match path {
                "/update.json" => &raw[..],
                "/update.json.sig" => &sig[..],
                "/nuschl.exe" => &body[..],
                _ => panic!("unexpected private route"),
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                reply.len()
            )
            .unwrap();
            stream.write_all(reply).unwrap();
        }
    });
    let install = dir.path().join("install");
    fs::create_dir(&install)?;
    fs::write(install.join("nuschl.exe"), "old")?;
    let update = nuschl_update::check_at(
        &base,
        &[key],
        &workers,
        "windows-x86_64",
        "0.1.0",
        &dir.path().join("seen.json"),
    )?
    .ok_or("missing update")?;
    let staged = nuschl_update::download(&update, &install, |_, _| {})?;
    nuschl_update::apply(staged)?;
    assert_eq!(fs::read(install.join("nuschl.exe"))?, body);
    nuschl_update::cleanup(&install);
    server.join().unwrap();
    println!(
        "PASS: private HTTP hybrid manifest -> remembered version -> checked download -> atomic install -> cleanup"
    );
    Ok(())
}
