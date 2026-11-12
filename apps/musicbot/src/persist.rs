// SPDX-License-Identifier: EUPL-1.2
//! Playback state survives deployment restarts in `queues.json`. Writes are
//! serialized, synced and atomically replaced; an unreadable file fails startup.
use crate::{
    Bot,
    player::{Ducking, Entry, HistoryItem, MAX_DUCK_HOLD, Restored, RestoredCurrent},
    subsonic::{LibraryIdentity, Track},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fs::File,
    io::{self, Write},
    path::Path,
    time::Duration,
};

const FILE: &str = "queues.json";
const VERSION: u32 = 1;
const SETTLE: Duration = Duration::from_secs(1);
const POSITION_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Serialize, Deserialize)]
struct Saved {
    version: u32,
    players: Vec<SavedPlayer>,
    channels: Vec<SavedChannel>,
}

#[derive(Serialize, Deserialize)]
struct SavedPlayer {
    name: String,
    paused: bool,
    volume: u8,
    ducking: SavedDucking,
    current: Option<SavedCurrent>,
    queue: Vec<SavedEntry>,
    next_id: u64,
    #[serde(default)]
    history: VecDeque<HistoryItem>,
    #[serde(default)]
    next_history_id: u64,
}

#[derive(Serialize, Deserialize)]
struct SavedDucking {
    enabled: bool,
    level: u8,
    hold_ms: u64,
    sensitivity: u8,
}

#[derive(Serialize, Deserialize)]
struct SavedCurrent {
    entry: SavedEntry,
    position_ms: u64,
    #[serde(default)]
    started_at_ms: Option<i64>,
    #[serde(default)]
    played_ms: u64,
}

#[derive(Serialize, Deserialize)]
struct SavedEntry {
    id: u64,
    // Library identity is independent of its index in the configuration.
    library: LibraryIdentity,
    added_by: String,
    track: Track,
}

#[derive(Serialize, Deserialize)]
struct SavedChannel {
    name: String,
    player: String,
}

fn capture(bot: &Bot) -> Saved {
    let players = bot
        .players
        .iter()
        .map(|player| {
            let state = player.state();
            let entry = |entry: &Entry, source: Option<&LibraryIdentity>| SavedEntry {
                id: entry.id,
                library: source.cloned().unwrap_or_else(|| {
                    LibraryIdentity::new(&bot.config.libraries[entry.track.library])
                }),
                added_by: entry.added_by.clone(),
                track: entry.track.clone(),
            };
            SavedPlayer {
                name: player.name.clone(),
                paused: state.paused,
                volume: state.volume,
                ducking: SavedDucking {
                    enabled: state.ducking.enabled,
                    level: state.ducking.level,
                    hold_ms: state.ducking.hold.as_millis() as u64,
                    sensitivity: state.ducking.sensitivity,
                },
                current: state.current.as_ref().map(|playing| SavedCurrent {
                    entry: entry(&playing.entry, playing.source.as_ref()),
                    position_ms: playing.elapsed().as_millis() as u64,
                    started_at_ms: playing.started_at_ms,
                    played_ms: playing.played_ms,
                }),
                queue: state
                    .queue
                    .iter()
                    .map(|queued| entry(queued, None))
                    .collect(),
                next_id: state.next_id,
                history: state.history.clone(),
                next_history_id: state.next_history_id,
            }
        })
        .collect();
    let channels = bot
        .connections
        .iter()
        .map(|connection| SavedChannel {
            name: connection.name.clone(),
            player: bot.players[*connection.player.borrow()].name.clone(),
        })
        .collect();
    Saved {
        version: VERSION,
        players,
        channels,
    }
}

/// The final shutdown save and the periodic writer share this lock. Capture
/// happens under it, so an earlier snapshot cannot replace a newer one.
pub fn save(bot: &Bot) -> io::Result<()> {
    let _guard = bot
        .persistence
        .lock()
        .map_err(|_| io::Error::other("queue writer lock poisoned"))?;
    let bytes = serde_json::to_vec(&capture(bot)).map_err(io::Error::other)?;
    write(&bot.state_directory.join(FILE), &bytes)
}

pub(crate) fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    let parent = File::open(
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new(".")),
    )?;
    let temporary = path.with_extension("json.new");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    make_private(&file)?;
    file.set_len(0)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    #[cfg(unix)]
    parent.sync_all()?;
    Ok(())
}

fn make_private(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

pub(crate) fn open_private(path: &Path) -> io::Result<File> {
    let file = File::open(path)?;
    make_private(&file)?;
    Ok(file)
}

/// Restores before anything connects or plays. Corrupt state is preserved;
/// the operator can recover it instead of a deployment silently losing it.
pub fn restore(bot: &Bot) -> io::Result<()> {
    let path = bot.state_directory.join(FILE);
    let bytes = match open_private(&path).and_then(|mut file| {
        use std::io::Read;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let saved: Saved = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if saved.version != VERSION {
        return Err(io::Error::other(format!(
            "unsupported queue state version {}",
            saved.version
        )));
    }
    let restore_entry = |mut entry: SavedEntry| {
        let library = bot.config.libraries.iter().position(|library| {
            library.name == entry.library.name
                && library.url.trim_end_matches('/') == entry.library.url
        });
        if let Some(library) = library {
            entry.track.library = library;
        } else {
            tracing::warn!(
                "cannot restore {}: library {} ({}) is no longer configured",
                entry.track.title,
                entry.library.name,
                entry.library.url
            );
        }
        (
            Entry {
                id: entry.id,
                track: entry.track,
                added_by: entry.added_by,
            },
            entry.library,
            library.is_some(),
        )
    };
    for saved in saved.players {
        let Some(player) = bot.players.iter().find(|player| player.name == saved.name) else {
            tracing::warn!("dropping saved queue {}: no longer configured", saved.name);
            continue;
        };
        let mut unavailable_current = false;
        let current = saved.current.map(|current| {
            let (entry, source, available) = restore_entry(current.entry);
            unavailable_current = !available;
            RestoredCurrent {
                entry,
                position: Duration::from_millis(current.position_ms),
                source: Some(source),
                started_at_ms: current.started_at_ms,
                played_ms: current.played_ms,
            }
        });
        let queue = saved
            .queue
            .into_iter()
            .filter_map(|entry| {
                let (entry, _, available) = restore_entry(entry);
                available.then_some(entry)
            })
            .collect();
        player.restore(Restored {
            paused: saved.paused,
            volume: saved.volume,
            ducking: Ducking {
                enabled: saved.ducking.enabled,
                level: saved.ducking.level.min(100),
                hold: Duration::from_millis(saved.ducking.hold_ms).min(MAX_DUCK_HOLD),
                sensitivity: saved.ducking.sensitivity.min(100),
            },
            current,
            queue,
            next_id: saved.next_id,
            history: saved.history,
            next_history_id: saved.next_history_id,
        });
        if unavailable_current {
            player.fail_restored("the song's library is no longer configured".into());
        }
    }
    for saved in saved.channels {
        let connection = bot
            .connections
            .iter()
            .find(|connection| connection.name == saved.name);
        let player = bot
            .players
            .iter()
            .position(|player| player.name == saved.player);
        if let (Some(connection), Some(player)) = (connection, player) {
            connection.player.send_replace(player);
        }
    }
    tracing::info!("restored queues from {}", path.display());
    Ok(())
}

/// Positions are checkpointed every five seconds, queue/settings changes
/// after one second. SIGTERM triggers a final save without this debounce.
pub async fn keep(bot: std::sync::Arc<Bot>) {
    let mut changes = bot.changes.subscribe();
    let mut interval = tokio::time::interval(POSITION_INTERVAL);
    loop {
        tokio::select! {
            changed = changes.changed() => {
                if changed.is_err() { return; }
                tokio::time::sleep(SETTLE).await;
                changes.borrow_and_update();
            }
            _ = interval.tick() => {
                if !bot.players.iter().any(|player| {
                    let state = player.state();
                    state.current.is_some() && !state.paused && player.listeners() > 0
                }) { continue; }
            }
        }
        let saver = bot.clone();
        match tokio::task::spawn_blocking(move || save(&saver)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::error!("cannot save queues: {error}"),
            Err(error) => tracing::error!("queue writer failed: {error}"),
        }
    }
}

#[cfg(test)]
mod tests;
