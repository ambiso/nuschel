// SPDX-License-Identifier: EUPL-1.2
//! Test-only launcher probe. Link against the same nuschl-sandbox rlib as the
//! bot; the Python fixture supplies only its owned canary and loopback endpoint.
//! The probe starts itself as a confined codec child, which reports what it
//! could reach.
use nuschl_sandbox::Role::MusicbotCodec;
use std::{ffi::OsStr, io::Write, net::TcpStream, path::Path, time::Duration};

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    match &args[1..] {
        [flag, _, canary, port] if flag == "--codec-worker" => confined(canary, port),
        [flag, executable, operation] if flag == "--worker" => {
            let operation = ["decode", "encode", "duck"]
                .into_iter()
                .find(|known| operation == known)
                .expect("codec role");
            let mut command =
                nuschl_sandbox::command(Path::new(executable), MusicbotCodec { operation })
                    .expect("sandbox must be available");
            let status = command.as_std_mut().status().expect("role launch");
            std::process::exit(status.code().unwrap_or(1));
        }
        [canary, port] => {
            let probe = std::env::current_exe().expect("probe path");
            let mut command = nuschl_sandbox::command(
                &probe,
                MusicbotCodec {
                    operation: "decode",
                },
            )
            .expect("sandbox must be available; no unrestricted fallback");
            let output = command.arg(canary).arg(port).as_std_mut().output();
            let output = output.expect("sandbox probe launch");
            assert!(
                output.status.success(),
                "sandbox probe failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.stdout.len() <= 4096);
            print!("{}", String::from_utf8(output.stdout).expect("probe UTF-8"));
        }
        _ => panic!("probe CANARY PORT | --worker EXEC ROLE"),
    }
}

/// Inside the sandbox, as `/worker --codec-worker decode CANARY PORT`.
fn confined(canary: &OsStr, port: &OsStr) {
    let file_denied = std::fs::File::open(canary).is_err();
    let port: u16 = port
        .to_str()
        .and_then(|port| port.parse().ok())
        .expect("port");
    let network_denied =
        TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(1))
            .and_then(|mut connection| connection.write_all(b"forbidden"))
            .is_err();
    let credential_absent = std::env::var_os("MUSICBOT_TEST_SECRET").is_none();
    let private_tmp = !Path::new(canary).exists();
    let cwd = std::env::current_dir().expect("cwd").display().to_string();
    println!(
        "{{\"file_denied\":{file_denied},\"network_denied\":{network_denied},\
         \"credential_absent\":{credential_absent},\"private_tmp\":{private_tmp},\"cwd\":{cwd:?}}}"
    );
}
