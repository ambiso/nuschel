// SPDX-License-Identifier: EUPL-1.2
//! The build-time check runs the real binary with its runtime schema.
use std::{
    net::TcpListener,
    process::{Command, Output},
};

const CONFIG: &str = r#"
[web]
listen = "127.0.0.1:8474"
public_url = "https://music.example.org"
[[subsonic]]
name = "Home"
url = "https://library.example.org"
api_key_file = "missing-key"
[[channel]]
name = "music"
host = "voice.example.org"
username = "DJ"
"#;

fn check(config: &str) -> Output {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(&path, config).unwrap();
    Command::new(env!("CARGO_BIN_EXE_nuschl-musicbot"))
        .arg("--config")
        .arg(path)
        .arg("--check-config")
        .env_remove("STATE_DIRECTORY")
        .env_remove("CREDENTIALS_DIRECTORY")
        .output()
        .unwrap()
}

#[test]
fn checking_requires_no_state_credentials_or_network() {
    let directory = tempfile::tempdir().unwrap();
    let unavailable_state = directory.path().join("state");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = listener.local_addr().unwrap();
    let text = CONFIG
        .replace("127.0.0.1:8474", &endpoint.to_string())
        .replace("https://library.example.org", &format!("http://{endpoint}"));
    let path = directory.path().join("config.toml");
    std::fs::write(&path, text).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_nuschl-musicbot"))
        .arg("--config")
        .arg(&path)
        .arg("--check-config")
        .env("STATE_DIRECTORY", &unavailable_state)
        .env(
            "CREDENTIALS_DIRECTORY",
            directory.path().join("credentials"),
        )
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        !unavailable_state.exists(),
        "validation created production state"
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    // It also works without the normal runtime's required state-directory argument.
    assert!(check(CONFIG).status.success());
}

#[test]
fn invalid_schema_and_static_settings_are_rejected() {
    let cases = [
        (
            CONFIG.replace("name = \"Home\"", "name = \"Home\"\nunknown = true"),
            "unknown field",
        ),
        (
            CONFIG.replace("username = \"DJ\"", "username = 17"),
            "invalid type",
        ),
        (CONFIG.replace("username = \"DJ\"", ""), "missing field"),
        (
            CONFIG.replace("api_key_file = \"missing-key\"", ""),
            "api_key_file",
        ),
        (
            CONFIG.replace(
                "api_key_file = \"missing-key\"",
                "api_key_file = \"missing-key\"\nusername = \"bot\"\npassword_file = \"password\"",
            ),
            "either api_key_file",
        ),
        (
            CONFIG.replace("[web]", "[audio]\nvolume = 101\n[web]"),
            "audio.volume",
        ),
        (
            CONFIG.replace("[web]", "[web]\ncover_cache_mib = 4097"),
            "web.cover_cache_mib",
        ),
        (
            CONFIG.replace("127.0.0.1:8474", "not-a-socket"),
            "socket address",
        ),
        (
            CONFIG.replace("https://library.example.org", "https://"),
            "subsonic library Home: url",
        ),
        (
            CONFIG.replace(
                "https://library.example.org",
                "https://bot:secret@library.example.org",
            ),
            "subsonic library Home: url",
        ),
        (
            CONFIG.replace(
                "https://music.example.org",
                "https://music.example.org/#queue",
            ),
            "web.public_url",
        ),
        (
            CONFIG.replace("host = \"voice.example.org\"", "host = \"bad host\""),
            "channel music:",
        ),
        (
            CONFIG.replace("username = \"DJ\"", "username = \"DJ\"\nport = 0"),
            "65535",
        ),
        (
            CONFIG.replace("name = \"music\"", "name = \"../music\""),
            "channel name",
        ),
        (
            CONFIG.replace(
                "username = \"DJ\"",
                "username = \"DJ\"\ncertificate_sha256 = \"wrong\"",
            ),
            "64 hexadecimal",
        ),
    ];
    for (text, diagnostic) in cases {
        let result = check(&text);
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(
            !result.status.success(),
            "invalid configuration was accepted: {text}"
        );
        assert!(
            error.contains(diagnostic),
            "expected {diagnostic:?}: {error}"
        );
    }
}

#[test]
fn checking_does_not_open_or_replace_saved_queues() {
    let directory = tempfile::tempdir().unwrap();
    let queues = directory.path().join("queues.json");
    std::fs::write(&queues, b"corrupt saved state").unwrap();
    let config = directory.path().join("config.toml");
    std::fs::write(&config, CONFIG).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_nuschl-musicbot"))
        .arg("--config")
        .arg(config)
        .arg("--check-config")
        .env("STATE_DIRECTORY", directory.path())
        .env_remove("CREDENTIALS_DIRECTORY")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(std::fs::read(queues).unwrap(), b"corrupt saved state");
    assert!(!directory.path().join("tokens.json").exists());
}
