// SPDX-License-Identifier: EUPL-1.2
#![forbid(unsafe_code)]
use nuschl_update_worker::key_file;
use std::path::Path;

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let key = match args.as_slice() {
        [command, a, secret, b, public] if a == "--secret-file" && b == "--public-file" => {
            match command.to_str() {
                Some("keygen") => key_file::generate(Path::new(secret), Path::new(public))?,
                Some("public-key") => key_file::export_public(Path::new(secret), Path::new(public))?,
                Some("key-status") => key_file::status(Path::new(secret), Path::new(public))?,
                _ => return Err("unknown key operation".into()),
            }
        },
        _ => return Err("usage: nuschl-update-sign {keygen|public-key|key-status} --secret-file PRIVATE_PATH --public-file PUBLIC_PATH".into()),
    };
    // Only public identifiers. Never print, debug-format or send seeds through
    // arguments, environment variables, stdout, diagnostics or log metadata.
    println!(
        "format=2 algorithm=ed25519+mldsa44 key_id={}",
        hex::encode(key.id())
    );
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
