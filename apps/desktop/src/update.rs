// SPDX-License-Identifier: EUPL-1.2
//! In-app updates on Windows and macOS (see the `nuschel-update` crate): a
//! check shortly after start and every few hours, a banner when a newer
//! signed release exists, and download, swap and restart on request.
use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};

pub use nuschel_update::Update;

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
    nuschel_update::platform()?;
    let executable = std::env::current_exe().ok()?;
    nuschel_update::install_root(&executable)
}

/// Whether this build updates itself (Windows, macOS); Linux packages do not.
pub fn supported() -> bool {
    root().is_some()
}

/// Remove files an earlier update moved aside.
pub fn cleanup() {
    if let Some(root) = root() {
        nuschel_update::cleanup(&root);
    }
}

/// Where the newest update manifest seen is remembered (in the state folder).
static STATE: OnceLock<PathBuf> = OnceLock::new();

/// Set once at start, from `Profile::update_state_path`.
pub fn remember_state(path: &Path) {
    let _ = STATE.set(path.to_owned());
}

pub fn check() -> Result<Option<Update>, String> {
    let state = STATE.get().ok_or("the update state location is not set")?;
    // The state folder is created when first needed.
    if let Some(folder) = state.parent() {
        nuschel_session::layout::create_private_dir(folder).map_err(|e| e.to_string())?;
    }
    nuschel_update::check(root().as_deref(), env!("CARGO_PKG_VERSION"), state)
        .map_err(|e| e.to_string())
}

pub fn install(update: &Update, mut progress: impl FnMut(u64, u64)) -> Result<(), String> {
    let root = root().ok_or("this installation does not update itself")?;
    let staged =
        nuschel_update::download(update, &root, &mut progress).map_err(|e| e.to_string())?;
    nuschel_update::apply(staged).map_err(|e| e.to_string())
}

/// Start the updated program; the caller quits.
pub fn relaunch() -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    std::process::Command::new(executable)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}
