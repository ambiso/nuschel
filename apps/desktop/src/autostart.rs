// SPDX-License-Identifier: EUPL-1.2
//! Start Nuschel when the user logs in. Each platform keeps the switch in its
//! own per-user place, which is also the source of truth: an XDG autostart
//! entry on Linux, the `Run` registry key on Windows and a LaunchAgent on macOS.
//! Autostarted instances get `--autostart` and open minimised.

pub const FLAG: &str = "--autostart";

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn home() -> Result<std::path::PathBuf, String> {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())
}

/// The program to start: a launcher on `PATH` survives updates (on Nix the
/// store path of the running binary changes with every update).
#[cfg(target_os = "linux")]
fn program() -> String {
    let on_path = std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| directory.join("nuschel").is_file())
    });
    if on_path {
        "nuschel".into()
    } else {
        let exe = std::env::current_exe().unwrap_or_else(|_| "nuschel".into());
        // Quote for the desktop entry Exec key.
        let escaped = exe
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('`', "\\`")
            .replace('$', "\\$");
        format!("\"{escaped}\"")
    }
}

#[cfg(target_os = "linux")]
fn entry() -> Result<std::path::PathBuf, String> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
        .map_or_else(|| home().map(|home| home.join(".config")), Ok)?;
    Ok(config.join("autostart").join("org.nuschel.Nuschel.desktop"))
}

#[cfg(target_os = "linux")]
pub fn enabled() -> bool {
    entry().is_ok_and(|path| enabled_at(&path))
}

#[cfg(target_os = "linux")]
pub fn set(enable: bool) -> Result<(), String> {
    set_at(&entry()?, enable, &program())
}

#[cfg(target_os = "linux")]
fn enabled_at(path: &std::path::Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|text| {
        !text.lines().any(|line| {
            matches!(
                line.trim(),
                "Hidden=true" | "X-GNOME-Autostart-enabled=false"
            )
        })
    })
}

#[cfg(target_os = "linux")]
fn set_at(path: &std::path::Path, enable: bool, program: &str) -> Result<(), String> {
    if !enable {
        return match std::fs::remove_file(path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.to_string()),
            _ => Ok(()),
        };
    }
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    }
    let text = format!(
        "[Desktop Entry]\nType=Application\nName=Nuschel\nComment=Voice and text chat\nExec={program} {FLAG}\nIcon=org.nuschel.Nuschel\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
    );
    std::fs::write(path, text).map_err(|error| error.to_string())
}

#[cfg(target_os = "windows")]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

/// `reg.exe` keeps the registry access out of this process's code; no console flashes.
#[cfg(target_os = "windows")]
fn reg(arguments: &[&str]) -> std::io::Result<std::process::Output> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("reg")
        .args(arguments)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
}

#[cfg(target_os = "windows")]
pub fn enabled() -> bool {
    reg(&["query", RUN_KEY, "/v", "Nuschel"]).is_ok_and(|output| output.status.success())
}

#[cfg(target_os = "windows")]
pub fn set(enable: bool) -> Result<(), String> {
    let output = if enable {
        let exe = std::env::current_exe().map_err(|error| error.to_string())?;
        let command = format!("\"{}\" {FLAG}", exe.display());
        reg(&[
            "add", RUN_KEY, "/v", "Nuschel", "/t", "REG_SZ", "/d", &command, "/f",
        ])
    } else {
        if !enabled() {
            return Ok(());
        }
        reg(&["delete", RUN_KEY, "/v", "Nuschel", "/f"])
    }
    .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

#[cfg(target_os = "macos")]
fn agent() -> Result<std::path::PathBuf, String> {
    Ok(home()?
        .join("Library")
        .join("LaunchAgents")
        .join("org.nuschel.Nuschel.plist"))
}

#[cfg(target_os = "macos")]
pub fn enabled() -> bool {
    agent().is_ok_and(|path| path.exists())
}

#[cfg(target_os = "macos")]
pub fn set(enable: bool) -> Result<(), String> {
    let path = agent()?;
    if !enable {
        return match std::fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.to_string()),
            _ => Ok(()),
        };
    }
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let escaped = exe
        .to_string_lossy()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let text = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>org.nuschel.Nuschel</string>
  <key>ProgramArguments</key><array><string>{escaped}</string><string>{FLAG}</string></array>
  <key>RunAtLoad</key><true/>
  <key>ProcessType</key><string>Interactive</string>
</dict>
</plist>
"#
    );
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    }
    std::fs::write(&path, text).map_err(|error| error.to_string())
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
pub fn enabled() -> bool {
    false
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
pub fn set(_: bool) -> Result<(), String> {
    Err("Starting at login is not supported on this system.".into())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn the_autostart_entry_round_trips() {
        // A private directory: never the user's real ~/.config/autostart.
        let directory =
            std::env::temp_dir().join(format!("nuschel-autostart-{}", std::process::id()));
        let path = directory
            .join("autostart")
            .join("org.nuschel.Nuschel.desktop");
        assert!(!enabled_at(&path));
        set_at(&path, true, "nuschel").unwrap();
        assert!(enabled_at(&path));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\nExec=nuschel --autostart\n"), "{text}");
        std::fs::write(&path, text + "Hidden=true\n").unwrap();
        assert!(
            !enabled_at(&path),
            "a desktop that hid the entry counts as off"
        );
        set_at(&path, false, "nuschel").unwrap();
        assert!(!enabled_at(&path));
        set_at(&path, false, "nuschel").unwrap();
        std::fs::remove_dir_all(directory).ok();
    }
}
