// SPDX-License-Identifier: EUPL-1.2
#![forbid(unsafe_code)]
use nuschl_update_worker::key_file;
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let bounded = |path: &Path, limit: usize| -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let mut bytes = Vec::new();
        fs::File::open(path)?
            .take((limit + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            return Err("oversized update input".into());
        }
        Ok(bytes)
    };
    let mut authentication = None;
    let key = match args.as_slice() {
        [command, a, secret, b, public, c, manifest, d, signature]
            if command == "sign" && a == "--secret-file" && b == "--public-file"
            && c == "--manifest" && d == "--signature-file" => {
                let key = key_file::status(Path::new(secret), Path::new(public))?;
                let bytes = bounded(Path::new(manifest), nuschl_update::auth_format::MAX_MANIFEST_BYTES)?;
                let signed = key_file::Secret::read(Path::new(secret))?.sign(&bytes)?;
                let mut file = fs::OpenOptions::new().write(true).create_new(true).open(signature)?;
                file.write_all(signed.as_bytes())?;
                file.sync_all()?;
                key
            }
        [command, a, public, b, manifest, c, signature]
            if (command == "verify" || command == "authenticate") && a == "--public-file" && b == "--manifest" && c == "--signature-file" => {
                let keys = nuschl_update::trust::parse(&bounded(Path::new(public), nuschl_update::trust::MAX_TRUST_BYTES)?)?;
                let bytes = bounded(Path::new(manifest), nuschl_update::auth_format::MAX_MANIFEST_BYTES)?;
                let signed = nuschl_update::auth_format::Signature::from_bytes(&bounded(Path::new(signature), nuschl_update::auth_format::SIGNATURE_BYTES)?)?;
                let key = keys.into_iter().find(|key| signed.check_key(key).is_ok()).ok_or("signature pair is not enrolled")?;
                nuschl_update_worker::verify_ed25519(&key, &signed, &bytes)?;
                nuschl_update_worker::verify_mldsa44(&key, &signed, &bytes)?;
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
                if command == "authenticate" {
                    // Operator audit only: validate all normal v2 structural
                    // and interval limits at the signed issuance instant. This
                    // does not change runtime/current-time acceptance or the
                    // ordinary verify command. Both raw signatures were already
                    // checked above, including the full public-pair binding.
                    let candidate: nuschl_update::Manifest = serde_json::from_slice(&bytes)?;
                    let checked = nuschl_update::manifest::parse(&bytes, candidate.released)?;
                    let status = if checked.expires <= now { "expired" }
                        else if checked.released > now.saturating_add(nuschl_update::manifest::FUTURE_SKEW_SECONDS) { "not-yet-valid" }
                        else { "current" };
                    authentication = Some((status, checked.released, checked.expires));
                } else {
                    nuschl_update::manifest::parse(&bytes, now)?;
                }
                key
            }
        [command, a, secret, b, public] if a == "--secret-file" && b == "--public-file" => {
            match command.to_str() {
                Some("keygen") => key_file::generate(Path::new(secret), Path::new(public))?,
                Some("public-key") => key_file::export_public(Path::new(secret), Path::new(public))?,
                Some("key-status") => key_file::status(Path::new(secret), Path::new(public))?,
                _ => return Err("unknown key operation".into()),
            }
        },
        _ => return Err("usage: nuschl-update-sign {keygen|public-key|key-status|sign|verify|authenticate} --secret-file PRIVATE_PATH --public-file PUBLIC_PATH".into()),
    };
    // Only public identifiers. Never print, debug-format or send seeds through
    // arguments, environment variables, stdout, diagnostics or log metadata.
    println!(
        "format=2 algorithm=ed25519+mldsa44 key_id={}",
        hex::encode(key.id())
    );
    if let Some((status, released, expires)) = authentication {
        println!(
            "authentication_only=true status={status} current_time_accepted={} released={released} expires={expires}",
            status == "current"
        );
    }
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
