// SPDX-License-Identifier: EUPL-1.2
//! In-app updates on Windows and macOS (see the `nuschl-update` crate): a
//! check shortly after start and every few hours, a banner when a newer
//! signed release exists, and download, swap and restart on request.
use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};

pub use nuschl_update::Update;

/// First check after start, then this often.
pub const FIRST_CHECK: Duration = Duration::from_secs(20);
pub const INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Clone, Debug, Default, PartialEq)]
pub enum UpdateState {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(Update),
    Installing {
        done: u64,
        total: u64,
    },
    /// Files swapped; restart into the new version.
    Installed,
    Failed(String),
}

#[derive(Debug)]
pub enum UpdateEvent {
    Checked(Result<Option<Update>, String>),
    Progress { done: u64, total: u64 },
    Installed(Result<(), String>),
}

/// The installation this process can update, if any.
pub fn root() -> Option<PathBuf> {
    nuschl_update::platform()?;
    let executable = std::env::current_exe().ok()?;
    nuschl_update::install_root(&executable)
}

/// Whether this build updates itself (Windows, macOS); Linux packages do not.
pub fn supported() -> bool {
    root().is_some()
}

/// Remove files an earlier update moved aside.
pub fn cleanup() {
    if let Some(root) = root() {
        nuschl_update::cleanup(&root);
    }
}

/// Where the newest update manifest seen is remembered (in the state folder).
static STATE: OnceLock<PathBuf> = OnceLock::new();
static PROFILE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Retain only the typed persistent profile selection. Startup URLs, offers
/// and autostart are one-shot actions and must not replay on an update restart.
pub fn remember_profile(path: Option<&Path>) -> std::io::Result<()> {
    let profile = path.map(std::path::absolute).transpose()?;
    PROFILE
        .set(profile)
        .map_err(|_| std::io::Error::other("the startup profile was already recorded"))
}

/// Set once at start, from `Profile::update_state_path`.
pub fn remember_state(path: &Path) {
    let _ = STATE.set(path.to_owned());
}

pub fn check() -> Result<Option<Update>, String> {
    let state = STATE.get().ok_or("the update state location is not set")?;
    // The state folder is created when first needed.
    if let Some(folder) = state.parent() {
        nuschl_session::layout::create_private_dir(folder).map_err(|e| e.to_string())?;
    }
    nuschl_update::check(root().as_deref(), env!("CARGO_PKG_VERSION"), state)
        .map_err(|e| e.to_string())
}

pub fn install(update: &Update, mut progress: impl FnMut(u64, u64)) -> Result<(), String> {
    let root = root().ok_or("this installation does not update itself")?;
    let staged =
        nuschl_update::download(update, &root, &mut progress).map_err(|e| e.to_string())?;
    nuschl_update::apply(staged).map_err(|e| e.to_string())
}

/// Start the updated program; the caller quits.
pub fn relaunch() -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let profile = PROFILE
        .get()
        .ok_or("the startup profile was not recorded")?;
    relaunch_command(&executable, profile.as_deref())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn relaunch_command(executable: &Path, profile: Option<&Path>) -> std::process::Command {
    let mut command = std::process::Command::new(executable);
    if let Some(profile) = profile {
        // Command owns the native OsString arguments. No shell or manual
        // quoting is used, including for spaces and non-UTF8/Unicode paths.
        command.arg("--profile-dir").arg(profile);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn default_profile_restart_does_not_replay_startup_actions() {
        let executable = Path::new("nuschl");
        let command = relaunch_command(executable, None);
        assert_eq!(command.get_program(), executable.as_os_str());
        assert_eq!(command.get_args().count(), 0);
    }

    #[test]
    fn explicit_relative_space_and_unicode_profile_is_absolute_and_one_native_argument() {
        let relative = Path::new("relative profile µ");
        let absolute = std::path::absolute(relative).unwrap();
        assert!(absolute.is_absolute());
        let command = relaunch_command(Path::new("nuschl"), Some(&absolute));
        let arguments: Vec<_> = command.get_args().map(OsString::from).collect();
        assert_eq!(
            arguments,
            vec![OsString::from("--profile-dir"), absolute.into_os_string()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_profile_arguments_do_not_require_utf8_conversion() {
        use std::os::unix::ffi::OsStringExt;
        let profile = PathBuf::from(OsString::from_vec(b"/private/profile-\xff".to_vec()));
        let command = relaunch_command(Path::new("nuschl"), Some(&profile));
        assert_eq!(command.get_args().nth(1).unwrap(), profile.as_os_str());
    }
}
