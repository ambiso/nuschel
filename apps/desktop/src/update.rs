// SPDX-License-Identifier: EUPL-1.2
//! In-app updates on Windows and macOS (see the `nuschel-update` crate): a
//! check shortly after start and every few hours, a banner when a newer
//! signed release exists, and download, swap and restart on request.
use std::{path::PathBuf, time::Duration};

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

pub fn check() -> Result<Option<Update>, String> {
    nuschel_update::check(root().as_deref(), env!("CARGO_PKG_VERSION")).map_err(|e| e.to_string())
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
