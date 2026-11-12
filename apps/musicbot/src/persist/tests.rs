// SPDX-License-Identifier: EUPL-1.2
use super::*;
use crate::{
    config::{Config, Credentials},
    mumble::Connection,
    player::{Changes, HistoryOutcome, MAX_HISTORY, Player},
    subsonic::Library,
    tokens::Tokens,
};
use std::sync::{Arc, Mutex};

fn bot(directory: &Path, reverse: bool) -> Bot {
    let mut config = Config::parse(
        r#"
[web]
listen = "127.0.0.1:8474"
public_url = "http://127.0.0.1:8474"
[[subsonic]]
name = "first"
url = "http://127.0.0.1/first"
api_key_file = "key"
[[subsonic]]
name = "second"
url = "http://127.0.0.1/second"
api_key_file = "key"
[[channel]]
name = "music"
host = "localhost"
username = "DJ"
[[channel]]
name = "lobby"
host = "localhost"
username = "DJ2"
"#,
        directory,
    )
    .unwrap();
    if reverse {
        config.libraries.reverse();
        config.channels.reverse();
    }
    let changes = Changes::default();
    let libraries = Arc::new(
        config
            .libraries
            .iter()
            .enumerate()
            .map(|(index, config)| Library::new(index, config, Credentials::ApiKey("test".into())))
            .collect(),
    );
    let players = config
        .channels
        .iter()
        .map(|channel| {
            Arc::new(Player::new(
                channel.name.clone(),
                40,
                Ducking::default(),
                changes.clone(),
            ))
        })
        .collect();
    let connections = config
        .channels
        .iter()
        .enumerate()
        .map(|(index, channel)| {
            Connection::new(channel.name.clone(), "localhost:64738".into(), index)
        })
        .collect();
    Bot {
        config,
        changes,
        libraries,
        players,
        connections,
        state_directory: directory.into(),
        web_assets: None,
        tokens: Tokens::load(directory.join("tokens.json")).unwrap(),
        persistence: Mutex::new(()),
    }
}

fn entry(id: u64) -> Entry {
    Entry {
        id,
        added_by: "ada".into(),
        track: Track {
            library: 1,
            id: "song".into(),
            title: "A song".into(),
            artist: None,
            album: None,
            album_id: None,
            artist_id: None,
            duration: Some(60),
            track: None,
            suffix: Some("flac".into()),
            content_type: Some("audio/flac".into()),
            cover_art: None,
        },
    }
}

#[test]
fn deployment_restores_identity_position_duplicates_and_settings() {
    let directory = tempfile::tempdir().unwrap();
    let original = bot(directory.path(), false);
    original.players[0].restore(Restored {
        paused: true,
        volume: 67,
        ducking: Ducking {
            level: 18,
            sensitivity: 75,
            ..Ducking::default()
        },
        current: Some(RestoredCurrent {
            entry: entry(41),
            position: Duration::from_secs(12),
            source: Some(LibraryIdentity::new(&original.config.libraries[1])),
            started_at_ms: Some(1000),
            played_ms: 1800,
        }),
        queue: vec![entry(42), entry(43)],
        next_id: 50,
        history: VecDeque::new(),
        next_history_id: 1,
    });
    original.connections[1].player.send_replace(0);
    save(&original).unwrap();

    let restarted = bot(directory.path(), true);
    restore(&restarted).unwrap();
    let restored = restarted.players[1].state();
    assert!(restored.paused);
    assert_eq!(restored.volume, 67);
    assert_eq!(restored.ducking.level, 18);
    assert_eq!(restored.ducking.sensitivity, 75);
    let current = restored.current.as_ref().unwrap();
    assert_eq!(current.entry.id, 41);
    assert_eq!(
        current.entry.track.library, 0,
        "library identity follows configuration reorder"
    );
    assert_eq!(current.entry.track.suffix.as_deref(), Some("flac"));
    assert_eq!(current.elapsed(), Duration::from_secs(12));
    assert_eq!(current.started_at_ms, Some(1000));
    assert_eq!(
        current.played_ms, 1800,
        "a restart does not count the saved position as audio"
    );
    assert_eq!(current.source.as_ref().unwrap().name, "second");
    assert!(restored.history.is_empty(), "restart is not a second play");
    assert_eq!(
        restored
            .queue
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        [42, 43]
    );
    assert_eq!(restored.next_id, 50);
    assert_eq!(*restarted.connections[0].player.borrow(), 1);
    drop(restored);
    restarted.players[1].enqueue(vec![entry(1).track], "bob", None);
    assert_eq!(restarted.players[1].state().queue.back().unwrap().id, 50);
    restarted.players[1].skip();
    {
        let state = restarted.players[1].state();
        assert_eq!(state.history.len(), 1);
        let item = &state.history[0];
        assert_eq!(item.entry.id, 41);
        assert_eq!(item.started_at_ms, Some(1000));
        assert_eq!(item.played_ms, 1800);
        assert_eq!(item.position_ms, 12000);
        assert_eq!(item.outcome, HistoryOutcome::Skipped);
    }
}

#[test]
fn legacy_version_one_restores_position_without_fabricating_a_play() {
    let directory = tempfile::tempdir().unwrap();
    let original = bot(directory.path(), false);
    let legacy = br#"{
        "version":1,
        "players":[{
            "name":"music","paused":true,"volume":43,
            "ducking":{"enabled":true,"level":30,"hold_ms":800,"sensitivity":50},
            "current":{
                "entry":{
                    "id":9,"library":{"name":"second","url":"http://127.0.0.1/second"},
                    "added_by":"ada",
                    "track":{"library":999,"id":"legacy","title":"Legacy song",
                        "artist":null,"album":null,"album_id":null,"artist_id":null,
                        "duration":60,"track":null,"suffix":"flac","content_type":"audio/flac","cover_art":null}
                },
                "position_ms":12000
            },
            "queue":[],"next_id":1
        }],
        "channels":[]
    }"#;
    write(&directory.path().join(FILE), legacy).unwrap();
    restore(&original).unwrap();
    {
        let state = original.players[0].state();
        let playing = state.current.as_ref().unwrap();
        assert_eq!(playing.elapsed(), Duration::from_secs(12));
        assert_eq!(playing.entry.track.library, 1);
        assert_eq!(playing.source.as_ref().unwrap().name, "second");
        assert_eq!(playing.started_at_ms, None);
        assert_eq!(playing.played_ms, 0);
        assert!(state.history.is_empty());
        assert_eq!(state.next_id, 10);
        assert_eq!(state.next_history_id, 1);
    }
    original.players[0].skip();
    save(&original).unwrap();
    let restarted = bot(directory.path(), false);
    restore(&restarted).unwrap();
    let state = restarted.players[0].state();
    assert!(state.current.is_none());
    assert!(
        state.history.is_empty(),
        "a saved position is not proof of transmission"
    );
    assert_eq!(state.next_history_id, 1);
}

#[test]
fn historical_metadata_survives_reordered_and_removed_libraries() {
    let directory = tempfile::tempdir().unwrap();
    let original = bot(directory.path(), false);
    let source = LibraryIdentity::new(&original.config.libraries[1]);
    original.players[0].restore(Restored {
        paused: true,
        volume: 40,
        ducking: Ducking::default(),
        current: Some(RestoredCurrent {
            entry: entry(41),
            position: Duration::from_secs(7),
            source: Some(source.clone()),
            started_at_ms: Some(100),
            played_ms: 240,
        }),
        queue: vec![entry(42)],
        next_id: 43,
        history: VecDeque::new(),
        next_history_id: 1,
    });
    original.players[0].skip();
    save(&original).unwrap();
    let reordered = bot(directory.path(), true);
    restore(&reordered).unwrap();
    {
        let state = reordered.players[1].state();
        let item = &state.history[0];
        assert_eq!(
            item.entry.track.library, 1,
            "history retains original track metadata"
        );
        assert_eq!(item.source.as_ref(), Some(&source));
        assert_eq!(LibraryIdentity::new(&reordered.config.libraries[0]), source);
        assert_ne!(
            LibraryIdentity::new(&reordered.config.libraries[item.entry.track.library]),
            source
        );
        assert_eq!(
            state.queue[0].track.library, 0,
            "only queued tracks are remapped"
        );
    }
    save(&reordered).unwrap();
    let mut removed = bot(directory.path(), false);
    removed.config.libraries.remove(1);
    Arc::get_mut(&mut removed.libraries).unwrap().remove(1);
    restore(&removed).unwrap();
    {
        let state = removed.players[0].state();
        assert!(
            state.queue.is_empty(),
            "removed-library queued tracks still cannot play"
        );
        let item = &state.history[0];
        assert_eq!(item.source.as_ref(), Some(&source));
        assert_eq!(item.entry.id, 41);
        assert_eq!(item.entry.added_by, "ada");
        assert_eq!(item.entry.track.title, "A song");
        assert_eq!(item.entry.track.suffix.as_deref(), Some("flac"));
        assert_eq!(item.played_ms, 240);
        assert_eq!(item.position_ms, 7000);
        assert_eq!(item.started_at_ms, Some(100));
        assert_eq!(state.next_history_id, 2);
    }
    save(&removed).unwrap();
    let returned = bot(directory.path(), true);
    restore(&returned).unwrap();
    let state = returned.players[1].state();
    assert_eq!(state.history.len(), 1);
    assert_eq!(state.history[0].source.as_ref(), Some(&source));
    assert_eq!(state.history[0].entry.track.title, "A song");
}

#[test]
fn restart_bounds_saved_history_and_recovers_ending_order_ids() {
    let directory = tempfile::tempdir().unwrap();
    let original = bot(directory.path(), false);
    let source = LibraryIdentity::new(&original.config.libraries[1]);
    let mut saved = capture(&original);
    let player = &mut saved.players[0];
    player.history = (1..=(MAX_HISTORY + 5) as u64)
        .map(|id| HistoryItem {
            id,
            entry: entry(9000 + id),
            source: Some(source.clone()),
            started_at_ms: Some(id as i64),
            ended_at_ms: id as i64 + 100,
            played_ms: 20,
            position_ms: 3000,
            outcome: HistoryOutcome::Failed,
            error: Some("decoder failed".into()),
        })
        .collect();
    player.next_history_id = 2;
    player.current = Some(SavedCurrent {
        entry: SavedEntry {
            id: 41,
            library: source,
            added_by: "ada".into(),
            track: entry(41).track,
        },
        position_ms: 12000,
        started_at_ms: Some(500),
        played_ms: 160,
    });
    write(
        &directory.path().join(FILE),
        &serde_json::to_vec(&saved).unwrap(),
    )
    .unwrap();
    let restarted = bot(directory.path(), false);
    restore(&restarted).unwrap();
    {
        let state = restarted.players[0].state();
        assert_eq!(state.history.len(), MAX_HISTORY);
        assert_eq!(state.history.front().unwrap().id, (MAX_HISTORY + 5) as u64);
        assert_eq!(state.history.back().unwrap().id, 6);
        assert_eq!(state.next_history_id, (MAX_HISTORY + 6) as u64);
        assert_eq!(
            state.next_id, 42,
            "history IDs never become queue entry IDs"
        );
    }
    restarted.players[0].skip();
    save(&restarted).unwrap();
    let again = bot(directory.path(), false);
    restore(&again).unwrap();
    let state = again.players[0].state();
    assert_eq!(state.history.len(), MAX_HISTORY);
    assert_eq!(state.history[0].id, (MAX_HISTORY + 6) as u64);
    assert_eq!(state.history[0].entry.id, 41);
    assert_eq!(state.history[0].started_at_ms, Some(500));
    assert_eq!(state.history[0].played_ms, 160);
    assert_eq!(state.history[0].outcome, HistoryOutcome::Skipped);
    assert_eq!(state.next_history_id, (MAX_HISTORY + 7) as u64);
}

#[test]
fn a_removed_current_library_archives_the_ongoing_interval_once() {
    let directory = tempfile::tempdir().unwrap();
    let original = bot(directory.path(), false);
    let source = LibraryIdentity::new(&original.config.libraries[1]);
    original.players[0].restore(Restored {
        paused: true,
        volume: 40,
        ducking: Ducking::default(),
        current: Some(RestoredCurrent {
            entry: entry(41),
            position: Duration::from_secs(6),
            source: Some(source.clone()),
            started_at_ms: Some(1000),
            played_ms: 160,
        }),
        queue: Vec::new(),
        next_id: 42,
        history: VecDeque::new(),
        next_history_id: 1,
    });
    save(&original).unwrap();
    let mut removed = bot(directory.path(), false);
    removed.config.libraries.remove(1);
    Arc::get_mut(&mut removed.libraries).unwrap().remove(1);
    restore(&removed).unwrap();
    {
        let state = removed.players[0].state();
        assert!(
            state.current.is_none(),
            "an old index must not reach a different service"
        );
        assert_eq!(state.history.len(), 1);
        let item = &state.history[0];
        assert_eq!(item.source.as_ref(), Some(&source));
        assert_eq!(item.started_at_ms, Some(1000));
        assert_eq!(item.played_ms, 160);
        assert_eq!(item.position_ms, 6000);
        assert_eq!(item.outcome, HistoryOutcome::Failed);
        assert_eq!(
            item.error.as_deref(),
            Some("A song: the song's library is no longer configured")
        );
        assert_eq!(state.next_history_id, 2);
    }
    save(&removed).unwrap();
    restore(&removed).unwrap();
    assert_eq!(
        removed.players[0].state().history.len(),
        1,
        "a second restart does not archive again"
    );
}

#[test]
fn corrupt_state_is_left_for_recovery() {
    let directory = tempfile::tempdir().unwrap();
    let bot = bot(directory.path(), false);
    let path = directory.path().join(FILE);
    std::fs::write(&path, b"unfinished JSON").unwrap();
    assert!(restore(&bot).is_err());
    assert_eq!(std::fs::read(path).unwrap(), b"unfinished JSON");
}

#[cfg(unix)]
#[test]
fn legacy_queue_state_is_private_on_restore_and_atomic_replacement() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let original = bot(directory.path(), false);
    let path = directory.path().join(FILE);
    save(&original).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    restore(&original).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let temporary = path.with_extension("json.new");
    std::fs::write(&temporary, b"old secret").unwrap();
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o644)).unwrap();
    save(&original).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!temporary.exists());
    restore(&bot(directory.path(), false)).unwrap();
}

#[cfg(unix)]
#[test]
fn failed_atomic_replacement_preserves_old_state_and_private_temporary() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(FILE);
    std::fs::create_dir(&path).unwrap();
    let preserved = path.join("old");
    std::fs::write(&preserved, b"recoverable").unwrap();
    assert!(write(&path, b"candidate").is_err());
    assert_eq!(std::fs::read(preserved).unwrap(), b"recoverable");
    let temporary = path.with_extension("json.new");
    assert_eq!(
        std::fs::metadata(temporary).unwrap().permissions().mode() & 0o777,
        0o600
    );
}
