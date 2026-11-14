// SPDX-License-Identifier: EUPL-1.2
//! Where Nuschl keeps its files ([`Layout`]), and the move from the places
//! earlier versions used ([`migrate`]).
//!
//! On Linux and the BSDs the folders follow the XDG Base Directory
//! specification. A variable that is unset, empty or not an absolute path is
//! ignored, as the specification asks, and its default applies:
//!
//! | Folder | Default | Holds |
//! | --- | --- | --- |
//! | `$XDG_CONFIG_HOME/nuschl` | `~/.config/nuschl` | what you edit or keep with your dotfiles: `settings.json`, `servers.json` (the bookmarks), `managed.json`. No keys, no caches. |
//! | `$XDG_DATA_HOME/nuschl` | `~/.local/share/nuschl` | `identity/` (the Mumble certificate and its private key), `keys/` (the crypto worker's sealed keystore), `history/` (the storage worker's database), `nuschl-policy.json` (the servers' security records) |
//! | `$XDG_STATE_HOME/nuschl` | `~/.local/state/nuschl` | `last-channels.json`, `update-state.json` |
//! | `$XDG_CACHE_HOME/nuschl` | `~/.cache/nuschl` | `channels.json` (channel trees for browsing offline) and `icons.json` (server icons); safe to delete |
//!
//! macOS keeps configuration and data in `~/Library/Application
//! Support/org.Nuschl.Nuschl` (state in its `state` folder) and the cache in
//! `~/Library/Caches/org.Nuschl.Nuschl`. Windows keeps configuration in
//! `%APPDATA%\Nuschl\Nuschl\config`, data in `%LOCALAPPDATA%\Nuschl\Nuschl\data`
//! (state in its `state` folder) and the cache in
//! `%LOCALAPPDATA%\Nuschl\Nuschl\cache`. `--profile-dir DIR` puts the four
//! under `DIR`, as `config`, `data`, `state` and `cache`.
//!
//! The storage and crypto workers may write anywhere in their folders, so each
//! has one of its own with nothing else in it, and neither reaches the other's
//! (`nuschl_sandbox`). Folders are created owner-only (0700) when first
//! needed, files 0600.
//!
//! ## Moving from earlier versions
//!
//! Earlier versions kept the identity (`identity.json`, its lock and backups),
//! the crypto worker's folder (`nuschel-identity/`), the security records and
//! `update-state.json` in the configuration folder, the channel trees, icons
//! and last channels inside `servers.json`, and the history database directly
//! in the data folder. [`migrate`] moves them on the first start:
//!
//! - one at a time: by a rename or a hard link where both places share a file
//!   system, otherwise by a synced copy that is put in place and only then
//!   removed. An interrupted move leaves the old file, or both, and the next
//!   start finishes it;
//! - never over something at the new place: where both exist and differ, the
//!   new place is used, the old one is left alone and Nuschl says so;
//! - holding the old identity lock, which earlier versions take to read or
//!   create the identity, so that two instances never move or create it at once;
//! - the history first, its sidecars before the database. Until the history
//!   is out of the data folder, which the storage worker could otherwise
//!   reach as a whole, nothing else moves into it;
//! - what could not be moved is used where it is, and reported.
use std::{
    ffi::OsString,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// The folder name under each XDG base directory.
const APPLICATION: &str = "nuschl";
/// Folders under the data folder.
const HISTORY: &str = "history";
const IDENTITY: &str = "identity";
const KEYS: &str = "keys";
/// The crypto worker's folder in earlier versions, in the configuration folder.
const OLD_KEYS: &str = "nuschel-identity";
pub(crate) const HISTORY_FILE: &str = "history.sqlite3";
pub(crate) const IDENTITY_FILE: &str = "identity.json";
pub(crate) const IDENTITY_LOCK: &str = "identity.lock";
pub(crate) const UPDATE_STATE: &str = "update-state.json";
/// How long to wait for another instance that holds the identity lock.
const PATIENCE: Duration = Duration::from_secs(30);

/// Nuschl's four folders. Each holds what its kind should hold and nothing
/// else; the subfolders of `data` are [`Layout::history`],
/// [`Layout::identity`] and [`Layout::keys`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    /// What a user edits or keeps with their dotfiles. No secrets, no caches.
    pub config: PathBuf,
    /// The identity, the keys, the history and the security records.
    pub data: PathBuf,
    /// Where Nuschl resumes from: the last channels, the update check.
    pub state: PathBuf,
    /// Channel trees and server icons: safe to delete at any time.
    pub cache: PathBuf,
}

impl Layout {
    /// This user's folders on this platform.
    pub fn system() -> io::Result<Self> {
        Self::system_named(APPLICATION, "Nuschl")
    }

    pub(crate) fn legacy_system() -> io::Result<Self> {
        Self::system_named("nuschel", "Nuschel")
    }

    fn system_named(application: &str, display: &str) -> io::Result<Self> {
        let _ = display;
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let base = directories::BaseDirs::new()
                .ok_or_else(|| io::Error::other("could not find the home directory"))?;
            Ok(Self::xdg_named(
                |name| std::env::var_os(name),
                base.home_dir(),
                application,
            ))
        }
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        {
            // macOS and Windows have no state folder of their own.
            let paths = directories::ProjectDirs::from("org", display, display)
                .ok_or_else(|| io::Error::other("could not find the application data directory"))?;
            let data = paths.data_local_dir().to_owned();
            Ok(Self {
                config: paths.config_dir().to_owned(),
                state: paths
                    .state_dir()
                    .map_or_else(|| data.join("state"), Path::to_owned),
                cache: paths.cache_dir().to_owned(),
                data,
            })
        }
    }

    /// The XDG Base Directory folders of `home`, with the variables `var`
    /// returns (`std::env::var_os` outside tests).
    pub fn xdg(var: impl Fn(&str) -> Option<OsString>, home: &Path) -> Self {
        Self::xdg_named(var, home, APPLICATION)
    }

    fn xdg_named(var: impl Fn(&str) -> Option<OsString>, home: &Path, application: &str) -> Self {
        let base = |name: &str, default: &str| {
            absolute(var(name))
                .unwrap_or_else(|| home.join(default))
                .join(application)
        };
        Self {
            config: base("XDG_CONFIG_HOME", ".config"),
            data: base("XDG_DATA_HOME", ".local/share"),
            state: base("XDG_STATE_HOME", ".local/state"),
            cache: base("XDG_CACHE_HOME", ".cache"),
        }
    }

    /// The same structure under one folder (`--profile-dir`, tests).
    pub fn under(root: &Path) -> Self {
        Self {
            config: root.join("config"),
            data: root.join("data"),
            state: root.join("state"),
            cache: root.join("cache"),
        }
    }

    /// The storage worker's folder: the local history, and nothing else.
    pub fn history(&self) -> PathBuf {
        self.data.join(HISTORY)
    }

    /// The Mumble certificate and its private key, with lock and backups.
    pub fn identity(&self) -> PathBuf {
        self.data.join(IDENTITY)
    }

    /// The crypto worker's folder: the sealed keystore, its state and store key.
    pub fn keys(&self) -> PathBuf {
        self.data.join(KEYS)
    }

    /// A worker may write anywhere in its folder, so no other folder may lie
    /// inside one (XDG variables could nest them).
    pub(crate) fn check(&self) -> io::Result<()> {
        let workers = [self.history(), self.keys()];
        let all = [
            &self.config,
            &self.state,
            &self.cache,
            &self.identity(),
            &workers[0],
            &workers[1],
        ];
        for worker in &workers {
            if let Some(inside) = all
                .iter()
                .find(|other| **other != worker && other.starts_with(worker))
            {
                return Err(io::Error::other(format!(
                    "{} lies inside {}, which a sandboxed worker may write; set the XDG \
                     variables to folders apart from each other",
                    inside.display(),
                    worker.display()
                )));
            }
        }
        Ok(())
    }
}

/// An XDG variable's value, if it is an absolute path. Unset, empty and
/// relative values are invalid under the specification and ignored.
fn absolute(value: Option<OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|path| path.is_absolute())
}

/// `$XDG_CONFIG_DIRS` (`/etc/xdg` when unset or empty), most important first,
/// without relative entries.
pub fn xdg_config_dirs(var: impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let value = var("XDG_CONFIG_DIRS")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "/etc/xdg".into());
    std::env::split_paths(&value)
        .filter(|directory| directory.is_absolute())
        .collect()
}

/// Create `directory` and any missing parents owner-only, and make an
/// existing `directory` owner-only as well (the XDG specification asks for
/// 0700 on base directories Nuschl creates).
pub fn create_private_dir(directory: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Where each thing is now: its place in the [`Layout`], or where an earlier
/// version kept it while it could not be moved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Places {
    /// The storage worker's folder.
    pub history: PathBuf,
    /// The folder of identity.json, its lock and its backups.
    pub identity: PathBuf,
    /// The crypto worker's folder.
    pub keys: PathBuf,
    /// The folder of nuschl-policy.json.
    pub policy: PathBuf,
    /// update-state.json.
    pub update_state: PathBuf,
    /// The channel trees, icons and last channels are still in servers.json.
    pub servers_hold_caches: bool,
}

impl Places {
    pub fn new(layout: &Layout) -> Self {
        Self {
            history: layout.history(),
            identity: layout.identity(),
            keys: layout.keys(),
            policy: layout.data.clone(),
            update_state: layout.state.join(UPDATE_STATE),
            servers_hold_caches: false,
        }
    }

    /// Earlier versions' places, all of them.
    fn old(layout: &Layout, old: &Old) -> Self {
        Self {
            history: old.history.clone(),
            identity: layout.config.clone(),
            keys: layout.config.join(OLD_KEYS),
            policy: layout.config.clone(),
            update_state: layout.config.join(UPDATE_STATE),
            servers_hold_caches: true,
        }
    }
}

/// Where earlier versions kept the history: in the data folder itself (on
/// macOS only the oldest did; later ones used its `history` folder already).
struct Old {
    history: PathBuf,
}

impl Old {
    fn of(layout: &Layout) -> Self {
        Self {
            history: layout.data.clone(),
        }
    }
}

/// What moving from earlier versions' places did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Migration {
    /// What moved, in sentences for the log.
    pub moved: Vec<String>,
    /// What stays where it was, and why, in sentences to show.
    pub problems: Vec<String>,
}

/// What the servers.json split needs to know and say (see `Profile`).
pub(crate) trait SplitServers {
    /// Whether servers.json still holds channel trees, icons or last channels.
    fn pending(&self) -> bool;
    /// Move them to the cache and state folders; false if they stay.
    fn split(&self, report: &mut Migration) -> bool;
}

/// Move what earlier versions kept elsewhere into `layout` (see the module
/// documentation), and say where each thing is now.
pub(crate) fn migrate(layout: &Layout, servers: &dyn SplitServers) -> (Places, Migration) {
    let old = Old::of(layout);
    let mut report = Migration::default();
    if !pending(layout, &old, servers) {
        return (Places::new(layout), report);
    }
    let lock_path = layout.config.join(IDENTITY_LOCK);
    let lock = match take_lock(&lock_path) {
        Ok(lock) => lock,
        Err(error) => {
            report.problems.push(format!(
                "Nuschl could not take {} to move its files to their new places ({error}). \
                 It uses them where they are and tries again on the next start.",
                lock_path.display()
            ));
            let mut places = places(layout, &old, &mut report);
            places.servers_hold_caches |= servers.pending();
            return (places, report);
        }
    };
    sweep(layout);
    move_history(layout, &old, &mut report);
    if history_place(layout, &old, &mut Migration::default()) == layout.data {
        report.problems.push(format!(
            "Nuschl could not move the history out of {}, so everything else stays where \
             it was until it can.",
            layout.data.display()
        ));
        return (places(layout, &old, &mut report), report);
    }
    move_identity(layout, &mut report);
    let moves = [
        (OLD_KEYS, layout.keys(), "The end-to-end keys"),
        (
            crate::sticky::FILE,
            layout.data.join(crate::sticky::FILE),
            "The servers' security records",
        ),
        (
            UPDATE_STATE,
            layout.state.join(UPDATE_STATE),
            "The update check's state",
        ),
    ];
    for (name, to, what) in moves {
        let from = layout.config.join(name);
        let outcome = if name == OLD_KEYS {
            move_dir(&from, &to)
        } else if exists(&from) {
            // The new folder is created only for something to put in it.
            to.parent()
                .map_or(Ok(()), create_private_dir)
                .and_then(|()| move_file(&from, &to))
        } else {
            Ok(Outcome::Absent)
        };
        settle_move(outcome, what, &from, &to, &mut report);
    }
    let mut places = places(layout, &old, &mut report);
    places.servers_hold_caches = servers.pending() && !servers.split(&mut report);
    // Earlier versions' lock goes with their identity file.
    if places.identity != layout.config && !exists(&layout.config.join(IDENTITY_FILE)) {
        drop(lock);
        let _ = fs::remove_file(&lock_path);
    }
    (places, report)
}

/// Whether anything is still at an earlier version's place.
fn pending(layout: &Layout, old: &Old, servers: &dyn SplitServers) -> bool {
    let config = &layout.config;
    (old.history != layout.history() && exists(&old.history.join(HISTORY_FILE)))
        || exists(&config.join(IDENTITY_FILE))
        || exists(&config.join(IDENTITY_LOCK))
        || !identity_backups(config).is_empty()
        || exists(&config.join(OLD_KEYS))
        || (*config != layout.data && exists(&config.join(crate::sticky::FILE)))
        || (*config != layout.state && exists(&config.join(UPDATE_STATE)))
        || servers.pending()
}

/// Where each thing is, from what is on disk: an earlier version's place
/// where something is still there and nothing at the new one.
fn places(layout: &Layout, old: &Old, report: &mut Migration) -> Places {
    let history = history_place(layout, old, report);
    if history == layout.data {
        // The storage worker gets the whole data folder: nothing else may be in it.
        return Places::old(layout, old);
    }
    let stays = |from: &Path, to: &Path| exists(from) && !exists(to);
    let keys = layout.config.join(OLD_KEYS);
    let policy = layout.config.join(crate::sticky::FILE);
    let state = layout.config.join(UPDATE_STATE);
    Places {
        history,
        identity: if stays(
            &layout.config.join(IDENTITY_FILE),
            &layout.identity().join(IDENTITY_FILE),
        ) {
            layout.config.clone()
        } else {
            layout.identity()
        },
        keys: if exists(&keys) && !nonempty_dir(&layout.keys()) {
            keys
        } else {
            layout.keys()
        },
        policy: if stays(&policy, &layout.data.join(crate::sticky::FILE)) {
            layout.config.clone()
        } else {
            layout.data.clone()
        },
        update_state: if stays(&state, &layout.state.join(UPDATE_STATE)) {
            state
        } else {
            layout.state.join(UPDATE_STATE)
        },
        servers_hold_caches: false,
    }
}

/// The history's place: the old one while its database could not move, but
/// never a data folder that holds anything besides the history.
fn history_place(layout: &Layout, old: &Old, report: &mut Migration) -> PathBuf {
    let new = layout.history();
    if old.history == new
        || !exists(&old.history.join(HISTORY_FILE))
        || exists(&new.join(HISTORY_FILE))
    {
        return new;
    }
    if old.history == layout.data && !only_history(&layout.data) {
        report.problems.push(format!(
            "The history in {} could not be moved, and the folder now holds more than the \
             history, so the storage worker may not have it. Nuschl keeps a new history in {} \
             and leaves the earlier one where it is; quit every other Nuschl and start it \
             again to move it.",
            layout.data.display(),
            new.display()
        ));
        return new;
    }
    old.history.clone()
}

/// Whether `directory` holds only the history: its database and sidecars,
/// and the new history folder.
fn only_history(directory: &Path) -> bool {
    fs::read_dir(directory).is_ok_and(|entries| {
        entries.flatten().all(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name == HISTORY || name.starts_with(HISTORY_FILE)
        })
    })
}

/// The database last: until it has moved, the old place stays the history's,
/// and the next start finishes an interrupted move.
fn move_history(layout: &Layout, old: &Old, report: &mut Migration) {
    let new = layout.history();
    let database = old.history.join(HISTORY_FILE);
    if old.history == new || !exists(&database) {
        return;
    }
    if exists(&new.join(HISTORY_FILE)) {
        report.problems.push(format!(
            "There is a history in {} and an earlier one in {}. Nuschl uses the first and \
             leaves the earlier one alone.",
            new.display(),
            old.history.display()
        ));
        return;
    }
    if let Err(error) = create_private_dir(&new) {
        report.problems.push(format!(
            "The history stays in {}: {} could not be created ({error}).",
            old.history.display(),
            new.display()
        ));
        return;
    }
    for suffix in ["-wal", "-shm", ""] {
        let name = format!("{HISTORY_FILE}{suffix}");
        let (from, to) = (old.history.join(&name), new.join(&name));
        match move_file(&from, &to) {
            Ok(Outcome::Conflict) => {
                report.problems.push(format!(
                    "The history stays in {}: {} differs from {}.",
                    old.history.display(),
                    to.display(),
                    from.display()
                ));
                return;
            }
            Ok(_) => {}
            Err(error) => {
                report.problems.push(format!(
                    "The history stays in {}: {} could not be moved ({error}).",
                    old.history.display(),
                    from.display()
                ));
                return;
            }
        }
    }
    report.moved.push(format!(
        "The history moved from {} to {}.",
        old.history.display(),
        new.display()
    ));
}

/// identity.json and its backups, holding the new identity lock as well, so
/// that no instance reads or creates the identity in either place meanwhile.
fn move_identity(layout: &Layout, report: &mut Migration) {
    let backups = identity_backups(&layout.config);
    let current = layout.config.join(IDENTITY_FILE);
    if !exists(&current) && backups.is_empty() {
        return;
    }
    let target = layout.identity();
    let _lock =
        match create_private_dir(&target).and_then(|()| take_lock(&target.join(IDENTITY_LOCK))) {
            Ok(lock) => lock,
            Err(error) => {
                report.problems.push(format!(
                    "The identity stays in {}: {} could not be prepared ({error}).",
                    layout.config.display(),
                    target.display()
                ));
                return;
            }
        };
    settle_move(
        move_file(&current, &target.join(IDENTITY_FILE)),
        "The identity (certificate and private key)",
        &current,
        &target.join(IDENTITY_FILE),
        report,
    );
    for backup in backups {
        let Some(name) = backup.file_name() else {
            continue;
        };
        let to = target.join(name);
        settle_move(
            move_file(&backup, &to),
            "An earlier identity",
            &backup,
            &to,
            report,
        );
    }
}

/// `identity.<seconds>.json.bak` files, as `Profile::import_identity` writes them.
pub(crate) fn identity_backups(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut backups: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("identity.") && name.ends_with(".json.bak")
        })
        .map(|entry| entry.path())
        .collect();
    backups.sort();
    backups
}

/// Report one move's outcome.
fn settle_move(
    outcome: io::Result<Outcome>,
    what: &str,
    from: &Path,
    to: &Path,
    report: &mut Migration,
) {
    let (from_shown, to_shown) = (from.display(), to.display());
    match outcome {
        Ok(Outcome::Absent | Outcome::Duplicate) => {}
        Ok(Outcome::Moved) => report
            .moved
            .push(format!("{what} moved from {from_shown} to {to_shown}.")),
        Ok(Outcome::Copied { link: None }) => report.moved.push(format!(
            "{what} was copied from {from_shown} to {to_shown}, and the old copy removed."
        )),
        Ok(Outcome::Copied { link: Some(target) }) => report.problems.push(format!(
            "{what} was a link at {from_shown} to {}. Nuschl copied it to {to_shown} and \
             removed the link; the original is still at {}, so remove it if it should not \
             be there (a dotfiles repository, for example).",
            target.display(),
            target.display()
        )),
        Ok(Outcome::Conflict) => report.problems.push(format!(
            "{what} is in {to_shown}, and an older copy that differs from it is in \
             {from_shown}. Nuschl uses the first and leaves the other alone."
        )),
        Err(error) => report.problems.push(format!(
            "{what} could not be moved from {from_shown} to {to_shown} ({error}). Nuschl \
             tries again on the next start."
        )),
    }
}

/// What a move did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// There was nothing to move (or both places are one).
    Absent,
    /// Renamed, or linked and the old name removed.
    Moved,
    /// Copied (to another file system, or from a symbolic link, whose target
    /// is given and left alone) and the old one removed.
    Copied { link: Option<PathBuf> },
    /// The new place held the same already; the old copy was removed.
    Duplicate,
    /// The new place holds something else; both stay.
    Conflict,
}

/// Move the file `from` to `to`, never replacing anything at `to`. Within a
/// file system a hard link (the one way to create a name only if it is free)
/// or a rename; otherwise, or from a symbolic link, a synced copy put in place
/// before `from` is removed. `to`'s folder must exist.
pub(crate) fn move_file(from: &Path, to: &Path) -> io::Result<Outcome> {
    if from == to {
        return Ok(Outcome::Absent);
    }
    let metadata = match fs::symlink_metadata(from) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Outcome::Absent),
        metadata => metadata?,
    };
    if exists(to) {
        return settle_file(from, to);
    }
    let link = metadata.file_type().is_symlink();
    if !link && !metadata.is_file() {
        return Err(io::Error::other("it is not a regular file"));
    }
    if !link {
        match fs::hard_link(from, to) {
            Ok(()) => {
                // The old name stays the file's while it cannot go (Windows,
                // where another process has it open).
                if let Err(error) = fs::remove_file(from) {
                    let _ = fs::remove_file(to);
                    return Err(error);
                }
                sync_parent(to);
                sync_parent(from);
                return Ok(Outcome::Moved);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return settle_file(from, to);
            }
            // No hard links there: rename, or copy to another file system.
            Err(_) => match fs::rename(from, to) {
                Ok(()) => {
                    sync_parent(to);
                    sync_parent(from);
                    return Ok(Outcome::Moved);
                }
                Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {}
                Err(error) => return Err(error),
            },
        }
    }
    let temporary = temporary_beside(to);
    let copied = copy_contents(from, &temporary).and_then(|()| place(&temporary, to));
    let _ = fs::remove_file(&temporary);
    match copied {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => settle_file(from, to),
        Err(error) => Err(error),
        Ok(()) => {
            let target = if link { fs::read_link(from).ok() } else { None };
            if let Err(error) = fs::remove_file(from) {
                let _ = fs::remove_file(to);
                return Err(error);
            }
            sync_parent(to);
            sync_parent(from);
            Ok(Outcome::Copied { link: target })
        }
    }
}

/// Move the folder `from` to `to` (which may exist empty), never into or over
/// a folder that holds anything. A rename within a file system; otherwise a
/// synced copy into a temporary folder beside `to`, renamed into place before
/// `from` is removed.
pub(crate) fn move_dir(from: &Path, to: &Path) -> io::Result<Outcome> {
    if from == to {
        return Ok(Outcome::Absent);
    }
    let metadata = match fs::symlink_metadata(from) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Outcome::Absent),
        metadata => metadata?,
    };
    let link = metadata.file_type().is_symlink();
    if !link && !metadata.is_dir() {
        return Err(io::Error::other("it is not a folder"));
    }
    if exists(to) {
        if nonempty_dir(to) {
            return settle_dir(from, to);
        }
        fs::remove_dir(to)?;
    }
    if let Some(parent) = to.parent() {
        create_private_dir(parent)?;
    }
    if !link {
        match fs::rename(from, to) {
            Ok(()) => {
                sync_parent(to);
                sync_parent(from);
                // Owner-only in its new place, whatever it was before.
                create_private_dir(to)?;
                return Ok(Outcome::Moved);
            }
            Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {}
            Err(error) => return Err(error),
        }
    }
    let temporary = temporary_beside(to);
    if let Err(error) = copy_tree(from, &temporary).and_then(|()| fs::rename(&temporary, to)) {
        let _ = fs::remove_dir_all(&temporary);
        return if nonempty_dir(to) {
            settle_dir(from, to)
        } else {
            Err(error)
        };
    }
    sync_parent(to);
    if link {
        let target = fs::read_link(from).ok();
        fs::remove_file(from)?;
        return Ok(Outcome::Copied { link: target });
    }
    // The copy is complete; a folder only partly removed is settled (or
    // reported as differing) on the next start.
    let _ = fs::remove_dir_all(from);
    sync_parent(from);
    Ok(Outcome::Copied { link: None })
}

/// Both places hold a file: drop the old one if it is the same.
fn settle_file(from: &Path, to: &Path) -> io::Result<Outcome> {
    if same_contents(from, to)? {
        fs::remove_file(from)?;
        Ok(Outcome::Duplicate)
    } else {
        Ok(Outcome::Conflict)
    }
}

/// Both places hold a folder: drop the old one if it holds the same.
fn settle_dir(from: &Path, to: &Path) -> io::Result<Outcome> {
    if same_tree(from, to)? {
        if fs::symlink_metadata(from)?.file_type().is_symlink() {
            fs::remove_file(from)?;
        } else {
            fs::remove_dir_all(from)?;
        }
        Ok(Outcome::Duplicate)
    } else {
        Ok(Outcome::Conflict)
    }
}

fn same_contents(a: &Path, b: &Path) -> io::Result<bool> {
    let (mut a, mut b) = (fs::File::open(a)?, fs::File::open(b)?);
    if a.metadata()?.len() != b.metadata()?.len() {
        return Ok(false);
    }
    let (mut left, mut right) = (vec![0; 64 * 1024], vec![0; 64 * 1024]);
    loop {
        let read = a.read(&mut left)?;
        if read == 0 {
            return Ok(b.read(&mut right[..1])? == 0);
        }
        b.read_exact(&mut right[..read])?;
        if left[..read] != right[..read] {
            return Ok(false);
        }
    }
}

fn same_tree(a: &Path, b: &Path) -> io::Result<bool> {
    let names = |directory: &Path| -> io::Result<Vec<OsString>> {
        let mut names = fs::read_dir(directory)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<io::Result<Vec<_>>>()?;
        names.sort();
        Ok(names)
    };
    let names_a = names(a)?;
    if names_a != names(b)? {
        return Ok(false);
    }
    for name in names_a {
        let (a, b) = (a.join(&name), b.join(&name));
        let (kind_a, kind_b) = (fs::metadata(&a)?, fs::metadata(&b)?);
        let same = match (kind_a.is_dir(), kind_b.is_dir()) {
            (true, true) => same_tree(&a, &b)?,
            (false, false) => same_contents(&a, &b)?,
            _ => false,
        };
        if !same {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Copy `from`'s contents (following a link) into the new file `to`, 0600, synced.
fn copy_contents(from: &Path, to: &Path) -> io::Result<()> {
    let mut source = fs::File::open(from)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut target = options.open(to)?;
    io::copy(&mut source, &mut target)?;
    target.sync_all()
}

/// Copy the folder `from` (regular files and folders only) to the new `to`.
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    // Never into an existing folder: the temporary is new.
    builder.recursive(false);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            copy_contents(&entry.path(), &target)?;
        } else {
            return Err(io::Error::other(format!(
                "{} is neither a file nor a folder",
                entry.path().display()
            )));
        }
    }
    sync_dir(to);
    Ok(())
}

/// Give the synced `temporary` the name `to` if `to` is free.
fn place(temporary: &Path, to: &Path) -> io::Result<()> {
    match fs::hard_link(temporary, to) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(error),
        // No hard links on this file system: check, then rename.
        Err(_) if exists(to) => Err(io::ErrorKind::AlreadyExists.into()),
        Err(_) => fs::rename(temporary, to),
    }
}

/// `.<name>.<random>.moving` beside `path`; [`sweep`] removes leftovers.
fn temporary_beside(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}.{}.moving", uuid::Uuid::new_v4()))
}

/// Remove what an interrupted copy left beside its targets. Only the holder
/// of the migration lock copies, so none of these is in use.
fn sweep(layout: &Layout) {
    for directory in [
        layout.data.clone(),
        layout.history(),
        layout.identity(),
        layout.state.clone(),
    ] {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with('.') && name.ends_with(".moving")) {
                continue;
            }
            let _ = match entry.file_type() {
                Ok(kind) if kind.is_dir() => fs::remove_dir_all(entry.path()),
                _ => fs::remove_file(entry.path()),
            };
        }
    }
}

/// Lock `path` (created 0600), waiting a while for another holder.
pub(crate) fn take_lock(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(path)?;
    let deadline = Instant::now() + PATIENCE;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(fs::TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "another Nuschl holds it",
                ));
            }
            Err(fs::TryLockError::Error(error)) => return Err(error),
        }
    }
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn nonempty_dir(path: &Path) -> bool {
    fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_some())
}

/// Make a rename, link or removal in `path`'s folder durable.
fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        sync_dir(parent);
    }
}

/// Folders can be synced on Unix; Windows has no handle for it here (and
/// NTFS journals its metadata).
fn sync_dir(directory: &Path) {
    #[cfg(unix)]
    if let Ok(directory) = fs::File::open(directory) {
        let _ = directory.sync_all();
    }
    #[cfg(not(unix))]
    {
        let _ = directory;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn temporary(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("nuschl-layout-{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn environment(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: BTreeMap<String, OsString> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn xdg_variables_apply_only_when_absolute() {
        let home = Path::new("/home/ada");
        let defaults = Layout::xdg(environment(&[]), home);
        assert_eq!(defaults.config, home.join(".config/nuschl"));
        assert_eq!(defaults.data, home.join(".local/share/nuschl"));
        assert_eq!(defaults.state, home.join(".local/state/nuschl"));
        assert_eq!(defaults.cache, home.join(".cache/nuschl"));
        assert_eq!(defaults.history(), home.join(".local/share/nuschl/history"));
        assert_eq!(
            defaults.identity(),
            home.join(".local/share/nuschl/identity")
        );
        assert_eq!(defaults.keys(), home.join(".local/share/nuschl/keys"));
        // Empty and relative values are invalid and ignored.
        let invalid = Layout::xdg(
            environment(&[
                ("XDG_CONFIG_HOME", ""),
                ("XDG_DATA_HOME", "relative/data"),
                ("XDG_STATE_HOME", "./state"),
                ("XDG_CACHE_HOME", "~/.cache"),
            ]),
            home,
        );
        assert_eq!(invalid, defaults);
        let set = Layout::xdg(
            environment(&[
                ("XDG_CONFIG_HOME", "/c"),
                ("XDG_DATA_HOME", "/d"),
                ("XDG_STATE_HOME", "/s"),
                ("XDG_CACHE_HOME", "/k"),
            ]),
            home,
        );
        assert_eq!(
            (set.config, set.data, set.state, set.cache),
            (
                "/c/nuschl".into(),
                "/d/nuschl".into(),
                "/s/nuschl".into(),
                "/k/nuschl".into()
            )
        );
        assert_eq!(
            xdg_config_dirs(environment(&[])),
            [PathBuf::from("/etc/xdg")]
        );
        assert_eq!(
            xdg_config_dirs(environment(&[("XDG_CONFIG_DIRS", "")])),
            [PathBuf::from("/etc/xdg")]
        );
        assert_eq!(
            xdg_config_dirs(environment(&[(
                "XDG_CONFIG_DIRS",
                "/opt/xdg:relative::/etc/xdg"
            )])),
            [PathBuf::from("/opt/xdg"), PathBuf::from("/etc/xdg")]
        );
    }

    /// The `directories` crate, which Nuschl used before and still uses on
    /// macOS and Windows, reads the variables the same way (dirs-sys ignores
    /// relative and empty values): the folders do not move under anyone.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn xdg_folders_agree_with_the_directories_crate() {
        let ours = Layout::system().unwrap();
        let theirs = directories::ProjectDirs::from("org", "Nuschl", "Nuschl").unwrap();
        assert_eq!(ours.config, theirs.config_dir());
        assert_eq!(ours.data, theirs.data_local_dir());
        assert_eq!(ours.cache, theirs.cache_dir());
        assert_eq!(Some(ours.state.as_path()), theirs.state_dir());
    }

    #[test]
    fn worker_folders_never_contain_another_folder() {
        let root = Path::new("/p");
        assert!(Layout::under(root).check().is_ok());
        let home = Path::new("/home/ada");
        // Configuration inside the data folder (macOS) is fine...
        let shared = Layout::xdg(
            environment(&[
                ("XDG_CONFIG_HOME", "/home/ada/.local/share"),
                ("XDG_STATE_HOME", "/home/ada/.local/share"),
            ]),
            home,
        );
        assert!(shared.check().is_ok());
        // ...inside a worker's folder it is not.
        let nested = Layout::xdg(
            environment(&[("XDG_CACHE_HOME", "/home/ada/.local/share/nuschl/history")]),
            home,
        );
        let error = nested.check().unwrap_err().to_string();
        assert!(error.contains("/history/nuschl lies inside"), "{error}");
    }

    #[test]
    fn files_move_without_replacing_anything() {
        let root = temporary("files");
        let (from, to) = (root.join("a"), root.join("b"));
        assert_eq!(move_file(&from, &to).unwrap(), Outcome::Absent);
        fs::write(&from, b"one").unwrap();
        assert_eq!(move_file(&from, &to).unwrap(), Outcome::Moved);
        assert!(!from.exists());
        assert_eq!(fs::read(&to).unwrap(), b"one");
        // An interrupted move left both: the same contents are settled.
        fs::write(&from, b"one").unwrap();
        assert_eq!(move_file(&from, &to).unwrap(), Outcome::Duplicate);
        assert!(!from.exists());
        // Different contents: neither is touched.
        fs::write(&from, b"two").unwrap();
        assert_eq!(move_file(&from, &to).unwrap(), Outcome::Conflict);
        assert_eq!(fs::read(&from).unwrap(), b"two");
        assert_eq!(fs::read(&to).unwrap(), b"one");
        // Same length, different bytes.
        fs::write(&from, b"onf").unwrap();
        assert_eq!(move_file(&from, &to).unwrap(), Outcome::Conflict);
        fs::remove_dir_all(root).unwrap();
    }

    /// A link (a dotfiles manager's) is copied, never followed into a move
    /// of someone else's file: the link goes, its target stays.
    #[cfg(unix)]
    #[test]
    fn a_linked_file_is_copied_and_its_target_left_alone() {
        let root = temporary("link");
        let dotfiles = root.join("dotfiles-identity.json");
        fs::write(&dotfiles, b"secret").unwrap();
        let link = root.join("identity.json");
        std::os::unix::fs::symlink(&dotfiles, &link).unwrap();
        let target = root.join("moved.json");
        assert_eq!(
            move_file(&link, &target).unwrap(),
            Outcome::Copied {
                link: Some(dotfiles.clone())
            }
        );
        assert!(fs::symlink_metadata(&link).is_err());
        assert_eq!(fs::read(&dotfiles).unwrap(), b"secret");
        assert_eq!(fs::read(&target).unwrap(), b"secret");
        assert!(!fs::symlink_metadata(&target).unwrap().is_symlink());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn folders_move_into_an_empty_place_only() {
        let root = temporary("folders");
        let (from, to) = (root.join("nuschl-identity"), root.join("data/keys"));
        fs::create_dir_all(&from).unwrap();
        fs::write(from.join("store.key"), [7; 32]).unwrap();
        fs::write(from.join("keystore"), b"sealed").unwrap();
        // An empty folder at the new place (a crypto worker started there and
        // never wrote) is replaced.
        fs::create_dir_all(&to).unwrap();
        assert_eq!(move_dir(&from, &to).unwrap(), Outcome::Moved);
        assert_eq!(fs::read(to.join("keystore")).unwrap(), b"sealed");
        assert!(!from.exists());
        // An interrupted copy (to another file system) left both, the same.
        copy_tree(&to, &from).unwrap();
        assert_eq!(move_dir(&from, &to).unwrap(), Outcome::Duplicate);
        assert!(!from.exists());
        // A different keystore at each place: neither is touched.
        copy_tree(&to, &from).unwrap();
        fs::write(from.join("keystore"), b"other").unwrap();
        assert_eq!(move_dir(&from, &to).unwrap(), Outcome::Conflict);
        assert_eq!(fs::read(from.join("keystore")).unwrap(), b"other");
        assert_eq!(fs::read(to.join("keystore")).unwrap(), b"sealed");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn copies_are_put_in_place_only_when_complete() {
        let root = temporary("copies");
        // The copy path (another file system) by hand: the temporary goes in
        // under its final name only if that is free.
        let (from, to) = (root.join("a"), root.join("b"));
        fs::write(&from, b"contents").unwrap();
        let temporary = temporary_beside(&to);
        copy_contents(&from, &temporary).unwrap();
        fs::write(&to, b"newer").unwrap();
        assert_eq!(
            place(&temporary, &to).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&to).unwrap(), b"newer");
        fs::remove_file(&to).unwrap();
        place(&temporary, &to).unwrap();
        assert_eq!(fs::read(&to).unwrap(), b"contents");
        // Leftovers of an interrupted copy are swept before the next move.
        let layout = Layout::under(&root);
        create_private_dir(&layout.identity()).unwrap();
        let leftover = temporary_beside(&layout.identity().join(IDENTITY_FILE));
        fs::write(&leftover, b"half").unwrap();
        let folder = temporary_beside(&layout.keys());
        fs::create_dir_all(folder.join("inner")).unwrap();
        sweep(&layout);
        assert!(!leftover.exists() && !folder.exists());
        assert!(layout.identity().exists());
        fs::remove_dir_all(root).unwrap();
    }
}
