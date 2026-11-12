// SPDX-License-Identifier: EUPL-1.2
//! Test-only launcher probe. Link against the same nuschl-ipc rlib as the bot;
//! the Python fixture supplies only its owned canary and loopback endpoint.
use std::{ffi::OsString, path::PathBuf};

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    assert_eq!(
        args.len(),
        4,
        "probe PYTHON CANARY PORT | --worker EXEC ROLE"
    );
    if args[1] == "--worker" {
        assert!(
            ["decode", "encode", "duck"]
                .iter()
                .any(|role| args[3] == *role)
        );
        let arguments = [OsString::from("--codec-worker"), args[3].clone()];
        let mut command =
            nuschl_ipc::host::musicbot_codec_sandbox_command(&PathBuf::from(&args[2]), &arguments)
                .expect("sandbox must be available");
        let status = command.as_std_mut().status().expect("role launch");
        std::process::exit(status.code().unwrap_or(1));
    }
    let script = r#"
import json,os,socket,sys
canary,port=sys.argv[1:]
try:
    with open(canary,'rb') as file: file.read(1)
    file_denied=False
except OSError:
    file_denied=True
try:
    connection=socket.create_connection(('127.0.0.1',int(port)),timeout=1)
    connection.sendall(b'forbidden'); connection.close(); network_denied=False
except OSError:
    network_denied=True
print(json.dumps({'file_denied':file_denied,'network_denied':network_denied,
 'credential_absent':'MUSICBOT_TEST_SECRET' not in os.environ,
 'private_tmp':not os.path.exists(canary),'cwd':os.getcwd()}))
"#;
    let arguments = [
        OsString::from("-c"),
        OsString::from(script),
        args[2].clone(),
        args[3].clone(),
    ];
    let mut command =
        nuschl_ipc::host::musicbot_codec_sandbox_command(&PathBuf::from(&args[1]), &arguments)
            .expect("sandbox must be available; no unrestricted fallback");
    let output = command.as_std_mut().output().expect("sandbox probe launch");
    assert!(
        output.status.success(),
        "sandbox probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.len() <= 4096);
    print!("{}", String::from_utf8(output.stdout).expect("probe UTF-8"));
}
