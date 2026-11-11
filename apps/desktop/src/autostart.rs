// SPDX-License-Identifier: EUPL-1.2
//! Start Nuschl when the user logs in. Each platform keeps the switch in its
//! own per-user place, which is also the source of truth: an XDG autostart
//! entry on Linux, the `Run` registry key on Windows and a LaunchAgent on macOS.
//! Autostarted instances get `--autostart` and open minimised.

pub const FLAG: &str = "--autostart";

/// Transfer the old application's login preference, after the default profile
/// has migrated. Explicit test/alternate profiles never touch login settings.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn migrate_legacy() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    let (new, old, was_enabled) = {
        let new = entry()?;
        let old = new.with_file_name("org.nuschel.Nuschel.desktop");
        let was_enabled = enabled_at(&old);
        (new, old, was_enabled)
    };
    #[cfg(target_os = "macos")]
    let (new, old, was_enabled) = {
        let new = agent()?;
        let old = new.with_file_name("org.nuschel.Nuschel.plist");
        let was_enabled = old.exists();
        (new, old, was_enabled)
    };
    if std::fs::symlink_metadata(&old).is_err() {
        return Ok(());
    }
    if !new.exists() && was_enabled {
        set(true)?;
    }
    std::fs::remove_file(old).map_err(|error| error.to_string())
}

#[cfg(target_os = "windows")]
pub fn migrate_legacy() -> Result<(), String> {
    let old = reg(&["query", RUN_KEY, "/v", "Nuschel"]).map_err(|error| error.to_string())?;
    if !old.status.success() {
        return Ok(());
    }
    if !enabled() {
        set(true)?;
    }
    let removed =
        reg(&["delete", RUN_KEY, "/v", "Nuschel", "/f"]).map_err(|error| error.to_string())?;
    if removed.status.success() {
        Ok(())
    } else {
        Err("could not remove Nuschel's old login entry".into())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
pub fn migrate_legacy() -> Result<(), String> {
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn home() -> Result<std::path::PathBuf, String> {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())
}

/// The program to start: a launcher on `PATH` survives updates (on Nix the
/// store path of the running binary changes with every update).
#[cfg(target_os = "linux")]
fn program() -> Result<String, String> {
    let on_path = std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| directory.join("nuschl").is_file())
    });
    if on_path {
        return Ok("nuschl".into());
    }
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let path = exe.to_str().ok_or("the program path is not valid UTF-8")?;
    exec_quote(path)
        .ok_or_else(|| "the program path cannot be written to an autostart entry".into())
}

/// Quote a path for a desktop entry's Exec key. String-level escaping applies
/// before the Exec quoting rules, so inside the quotes a backslash becomes four
/// and `"`, `` ` `` and `$` get two; `%` would start a field code and is
/// doubled. Control characters (a line break would end the key) are refused.
#[cfg(any(target_os = "linux", test))]
fn exec_quote(path: &str) -> Option<String> {
    if path.chars().any(char::is_control) {
        return None;
    }
    let mut out = String::from("\"");
    for c in path.chars() {
        match c {
            '\\' => out.push_str("\\\\\\\\"),
            '"' | '`' | '$' => {
                out.push_str("\\\\");
                out.push(c);
            }
            '%' => out.push_str("%%"),
            c => out.push(c),
        }
    }
    out.push('"');
    Some(out)
}

#[cfg(target_os = "linux")]
fn entry() -> Result<std::path::PathBuf, String> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
        .map_or_else(|| home().map(|home| home.join(".config")), Ok)?;
    Ok(config.join("autostart").join("org.nuschl.Nuschl.desktop"))
}

#[cfg(target_os = "linux")]
pub fn enabled() -> bool {
    entry().is_ok_and(|path| enabled_at(&path))
}

#[cfg(target_os = "linux")]
pub fn set(enable: bool) -> Result<(), String> {
    let program = if enable { program()? } else { String::new() };
    set_at(&entry()?, enable, &program)
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
        "[Desktop Entry]\nType=Application\nName=Nuschl\nComment=Voice and text chat\nExec={program} {FLAG}\nIcon=org.nuschl.Nuschl\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
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
    reg(&["query", RUN_KEY, "/v", "Nuschl"]).is_ok_and(|output| output.status.success())
}

#[cfg(target_os = "windows")]
pub fn set(enable: bool) -> Result<(), String> {
    let output = if enable {
        let exe = std::env::current_exe().map_err(|error| error.to_string())?;
        let command = format!("\"{}\" {FLAG}", exe.display());
        reg(&[
            "add", RUN_KEY, "/v", "Nuschl", "/t", "REG_SZ", "/d", &command, "/f",
        ])
    } else {
        if !enabled() {
            return Ok(());
        }
        reg(&["delete", RUN_KEY, "/v", "Nuschl", "/f"])
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
        .join("org.nuschl.Nuschl.plist"))
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
  <key>Label</key><string>org.nuschl.Nuschl</string>
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
    fn exec_paths_are_quoted_by_the_desktop_entry_rules() {
        assert_eq!(
            exec_quote("/opt/nuschl/nuschl").unwrap(),
            "\"/opt/nuschl/nuschl\""
        );
        assert_eq!(
            exec_quote("/a b/$x`y\"z").unwrap(),
            r#""/a b/\\$x\\`y\\"z""#
        );
        assert_eq!(exec_quote(r"/a\b").unwrap(), r#""/a\\\\b""#);
        assert_eq!(exec_quote("/100%/n").unwrap(), "\"/100%%/n\"");
        assert!(exec_quote("/a\nExec=evil").is_none());
    }

    #[test]
    fn the_autostart_entry_round_trips() {
        // A private directory: never the user's real ~/.config/autostart.
        let directory =
            std::env::temp_dir().join(format!("nuschl-autostart-{}", std::process::id()));
        let path = directory
            .join("autostart")
            .join("org.nuschl.Nuschl.desktop");
        assert!(!enabled_at(&path));
        set_at(&path, true, "nuschl").unwrap();
        assert!(enabled_at(&path));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\nExec=nuschl --autostart\n"), "{text}");
        std::fs::write(&path, text + "Hidden=true\n").unwrap();
        assert!(
            !enabled_at(&path),
            "a desktop that hid the entry counts as off"
        );
        set_at(&path, false, "nuschl").unwrap();
        assert!(!enabled_at(&path));
        set_at(&path, false, "nuschl").unwrap();
        std::fs::remove_dir_all(directory).ok();
    }
}
