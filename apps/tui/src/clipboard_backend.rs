// SPDX-License-Identifier: EUPL-1.2
//! Focus-free clipboard providers. Never run wl-clipboard's core-protocol
//! popup fallback: only the reviewed Rust helper can read native Wayland.
use std::{process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};

const PROBE_BYTES: u64 = 16 * 1024;
const PROBE_TIME: Duration = Duration::from_secs(2);
const PNG_BYTES: u64 = 8 * 1024 * 1024;
const PNG_TIME: Duration = Duration::from_secs(3);

pub struct ClipboardImage {
    pub bytes: Vec<u8>,
    pub provider: &'static str,
}

#[derive(Default, Debug, PartialEq, Eq)]
struct Protocols {
    ext: bool,
    wlr: bool,
}

fn protocols(output: &[u8]) -> Protocols {
    let mut protocols = Protocols::default();
    for line in String::from_utf8_lossy(output).lines() {
        let Some(tail) = line.trim_start().strip_prefix("interface: '") else {
            continue;
        };
        let Some((interface, tail)) = tail.split_once('\'') else {
            continue;
        };
        let version = tail
            .split_once("version:")
            .and_then(|(_, tail)| tail.trim_start().split([',', ' ']).next())
            .and_then(|value| value.parse::<u32>().ok());
        if version.is_none_or(|version| version < 1) {
            continue;
        }
        match interface {
            "ext_data_control_manager_v1" => protocols.ext = true,
            "zwlr_data_control_manager_v1" => protocols.wlr = true,
            _ => {}
        }
    }
    protocols
}

fn known_native_helper(version: &[u8]) -> bool {
    // Exact reviewed helper, pinned by flake.lock. The C implementation reports
    // "wl-clipboard 2.x" and may open a window if its registry changes.
    std::str::from_utf8(version).is_ok_and(|version| version.trim() == "wl-paste 0.9.5")
}

fn png_offered(targets: &[u8]) -> bool {
    std::str::from_utf8(targets)
        .is_ok_and(|text| text.lines().any(|line| line.trim() == "image/png"))
}

/// One explicit action, bounded overall as well as per subprocess. The caller
/// additionally rejects SSH before starting any clipboard service.
pub async fn read() -> Result<ClipboardImage, String> {
    tokio::time::timeout(Duration::from_secs(8), read_provider())
        .await
        .unwrap_or_else(|_| Err("The clipboard request timed out; no image was attached.".into()))
}

async fn read_provider() -> Result<ClipboardImage, String> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty());
    let x11 = std::env::var_os("DISPLAY").is_some_and(|value| !value.is_empty());
    if wayland {
        let globals = probe("wayland-info", &["--interface", "data_control"]).await;
        if globals.is_err() && !x11 {
            return Err("Could not verify focus-free Wayland clipboard support: wayland-info is unavailable or its registry query failed. No helper window was opened.".into());
        }
        let offered = globals.as_deref().map(protocols).unwrap_or_default();
        if offered.ext || offered.wlr {
            let version = probe("wl-paste", &["--version"]).await;
            if version.as_deref().is_ok_and(known_native_helper) {
                let bytes = read_png("wl-paste", &["--type", "image/png", "--no-newline"]).await?;
                return Ok(ClipboardImage {
                    bytes,
                    provider: "Wayland data-control",
                });
            }
            if !x11 {
                return Err("Native Wayland clipboard images require the reviewed wl-clipboard-rs 0.9.5 helper; a helper that opens a popup is never used.".into());
            }
        }
        if !x11 {
            return Err("Clipboard images are unavailable in this Wayland session: no focus-free data-control or XWayland clipboard bridge is available. No helper window was opened.".into());
        }
    }
    if x11 {
        // Initializing a lazy private XWayland server can precede selection
        // bridging. Query only MIME metadata first, retrying it once, never
        // switching to a popup or returning a previous image on failure.
        let mut png = false;
        let mut answered = false;
        for attempt in 0..2 {
            let targets = probe(
                "xclip",
                &["-selection", "clipboard", "-target", "TARGETS", "-out"],
            )
            .await;
            answered |= targets.is_ok();
            if targets.as_deref().is_ok_and(png_offered) {
                png = true;
                break;
            }
            if attempt == 0 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        if !png {
            if !answered {
                return Err("The X11 clipboard connection or xclip helper is unavailable; no image was read.".into());
            }
            return Err("The local clipboard does not provide a PNG image through the X11 clipboard connection.".into());
        }
        let bytes = read_png(
            "xclip",
            &["-selection", "clipboard", "-target", "image/png", "-out"],
        )
        .await?;
        return Ok(ClipboardImage {
            bytes,
            provider: if wayland {
                "XWayland clipboard bridge"
            } else {
                "X11 clipboard"
            },
        });
    }
    Err("No local Wayland or X11 clipboard connection is available.".into())
}

enum Failure {
    Unavailable,
    TooLarge,
    Failed,
    Timeout,
}

/// `program` with fixed `arguments` under prlimit (256 MiB, 3 CPU seconds):
/// at most `limit` bytes of its output within `deadline`. A helper that
/// writes more, fails or is late is killed.
async fn run(
    program: &str,
    arguments: &[&str],
    limit: u64,
    deadline: Duration,
) -> Result<Vec<u8>, Failure> {
    let mut child = Command::new("prlimit")
        .args(["--as=268435456", "--cpu=3", "--", program])
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| Failure::Unavailable)?;
    let mut output = child.stdout.take().ok_or(Failure::Failed)?;
    let result = tokio::time::timeout(deadline, async {
        let mut bytes = Vec::new();
        let mut output = (&mut output).take(limit + 1);
        output
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| Failure::Failed)?;
        if bytes.len() as u64 > limit {
            return Err(Failure::TooLarge);
        }
        match child.wait().await {
            Ok(status) if status.success() => Ok(bytes),
            _ => Err(Failure::Failed),
        }
    })
    .await
    .unwrap_or(Err(Failure::Timeout));
    if result.is_err() {
        let _ = child.kill().await;
    }
    result
}

async fn probe(program: &str, arguments: &[&str]) -> Result<Vec<u8>, ()> {
    run(program, arguments, PROBE_BYTES, PROBE_TIME)
        .await
        .map_err(|_| ())
}

/// Encoded PNG for the content worker; this process never decodes it.
async fn read_png(program: &str, arguments: &[&str]) -> Result<Vec<u8>, String> {
    match run(program, arguments, PNG_BYTES, PNG_TIME).await {
        Ok(bytes) if !bytes.is_empty() => Ok(bytes),
        Err(Failure::Unavailable) => Err(format!(
            "{program} is unavailable; install it to read PNG clipboard images."
        )),
        Err(Failure::TooLarge) => Err("The clipboard image exceeds 8 MiB.".into()),
        Err(Failure::Timeout) => Err("The clipboard read timed out; no image was attached.".into()),
        Ok(_) | Err(Failure::Failed) => Err("The clipboard does not provide a PNG image.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_exact_supported_globals_and_reviewed_helper_enable_native_reads() {
        assert_eq!(
            protocols(b"interface: 'wl_data_device_manager', version: 3, name: 5\n"),
            Protocols::default()
        );
        assert_eq!(
            protocols(b"interface: 'ext_data_control_manager_v1', version: 1, name: 2\n"),
            Protocols {
                ext: true,
                wlr: false
            }
        );
        assert_eq!(
            protocols(b"interface: 'zwlr_data_control_manager_v1', version: 2, name: 8\n"),
            Protocols {
                ext: false,
                wlr: true
            }
        );
        assert_eq!(protocols(b"interface: 'fake_ext_data_control_manager_v1', version: 1\ninterface: 'ext_data_control_manager_v1', version: 0\n"), Protocols::default());
        assert!(known_native_helper(b"wl-paste 0.9.5\n"));
        assert!(!known_native_helper(b"wl-clipboard 2.3.0\n"));
        assert!(!known_native_helper(b"wl-paste 999.0.0\n"));
        assert!(png_offered(b"TARGETS\nimage/png\n"));
        assert!(!png_offered(b"text/plain\nimage/png-untrusted\n"));
    }
}
