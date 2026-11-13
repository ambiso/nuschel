// SPDX-License-Identifier: EUPL-1.2
//! One owned, serial reporter for all logical queues. Finished playback stays
//! in the player's bounded history until its cursor has consumed it.
use crate::{
    Bot,
    player::{Player, State},
    subsonic::{API_TIMEOUT, Library, LibraryIdentity, Track},
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::watch, task::JoinHandle};

const MAX_THRESHOLD_MS: u64 = 240_000;
/// A first successful frame does not always coincide with a UI notification
/// (for example after an encoding error). Poll only the small cursor headers.
const CHECK_INTERVAL: Duration = Duration::from_secs(1);

fn qualifies(track: &Track, started_at_ms: Option<i64>, played_ms: u64) -> bool {
    let threshold = track
        .duration
        .filter(|seconds| *seconds > 0)
        .map_or(MAX_THRESHOLD_MS, |seconds| {
            (u64::from(seconds) * 500).min(MAX_THRESHOLD_MS)
        });
    started_at_ms.is_some() && played_ms > 0 && played_ms >= threshold
}

#[derive(Debug, PartialEq, Eq)]
struct Report {
    library: usize,
    id: String,
    time: i64,
    submission: bool,
}

fn source_library(
    track: &Track,
    source: Option<&LibraryIdentity>,
    identities: &[LibraryIdentity],
) -> Option<usize> {
    let source = source?;
    if identities.get(track.library) == Some(source) {
        Some(track.library)
    } else {
        identities.iter().position(|identity| identity == source)
    }
}

impl Report {
    fn new(
        track: &Track,
        source: Option<&LibraryIdentity>,
        identities: &[LibraryIdentity],
        time: i64,
        submission: bool,
    ) -> Option<Self> {
        // The captured source, not a possibly stale saved index, owns a play.
        // Missing sources must never silently use another library's account.
        let library = source_library(track, source, identities)?;
        Some(Self {
            library,
            id: track.id.clone(),
            time,
            submission,
        })
    }
}

#[derive(Clone, Copy)]
struct Cursor {
    finished: u64,
    now_playing: Option<u64>,
    prefer_finished: bool,
}

impl Cursor {
    fn capture(state: &State) -> Self {
        Self {
            // Never backfill restored/pre-feature finished history. Capture
            // before spawning audio, not when the reporting task first polls.
            finished: state.history.front().map_or(0, |item| item.id),
            // A resumed current entry may safely announce itself again.
            now_playing: None,
            prefer_finished: false,
        }
    }

    fn current(&mut self, state: &State, identities: &[LibraryIdentity]) -> Option<Report> {
        let playing = state.current.as_ref()?;
        let time = playing.started_at_ms?;
        if playing.played_ms == 0 || self.now_playing == Some(playing.entry.id) {
            return None;
        }
        // Consume before doing I/O: failures are not retried because the
        // protocol has no idempotency token, including across pause/seek.
        self.now_playing = Some(playing.entry.id);
        Report::new(
            &playing.entry.track,
            playing.source.as_ref(),
            identities,
            time,
            false,
        )
    }

    fn finished(
        &mut self,
        state: &State,
        identities: &[LibraryIdentity],
        through: u64,
    ) -> Option<Report> {
        if self.finished >= through
            || state
                .history
                .front()
                .is_none_or(|item| item.id <= self.finished)
        {
            return None;
        }
        // Newest-first monotonic ending IDs make the no-work path O(1) and
        // finding the next oldest pending record O(log(history.len())).
        let newer = state.history.partition_point(|item| item.id > through);
        let pending = state
            .history
            .partition_point(|item| item.id > self.finished);
        for item in state.history.range(newer..pending).rev() {
            self.finished = item.id;
            if !qualifies(&item.entry.track, item.started_at_ms, item.played_ms) {
                continue;
            }
            let Some(time) = item.started_at_ms else {
                continue;
            };
            if let Some(report) = Report::new(
                &item.entry.track,
                item.source.as_ref(),
                identities,
                time,
                true,
            ) {
                return Some(report);
            }
        }
        None
    }

    fn next(&mut self, state: &State, identities: &[LibraryIdentity]) -> Option<Report> {
        let report = if self.prefer_finished {
            self.finished(state, identities, u64::MAX)
                .or_else(|| self.current(state, identities))
        } else {
            self.current(state, identities)
                .or_else(|| self.finished(state, identities, u64::MAX))
        };
        if let Some(report) = &report {
            // Neither a long history backlog nor rapidly replacing current
            // songs can starve the other kind of report in this queue.
            self.prefer_finished = !report.submission;
        }
        report
    }
}

struct Stop {
    deadline: Instant,
    through: Vec<u64>,
}

impl Stop {
    fn capture(players: &[Arc<Player>], grace: Duration) -> Self {
        Self {
            deadline: Instant::now() + grace,
            through: players
                .iter()
                .map(|player| player.state().history.front().map_or(0, |item| item.id))
                .collect(),
        }
    }
}

struct Reporter {
    players: Arc<[Arc<Player>]>,
    libraries: Arc<Vec<Library>>,
    identities: Vec<LibraryIdentity>,
    cursors: Vec<Cursor>,
    next_player: usize,
    selected: Option<(usize, Cursor)>,
    changes: watch::Receiver<u64>,
}

impl Reporter {
    fn capture(
        players: &[Arc<Player>],
        libraries: Arc<Vec<Library>>,
        changes: watch::Receiver<u64>,
    ) -> Self {
        let cursors = players
            .iter()
            .map(|player| Cursor::capture(&player.state()))
            .collect();
        let identities = libraries.iter().map(Library::identity).collect();
        Self {
            players: players.to_vec().into(),
            libraries,
            identities,
            cursors,
            next_player: 0,
            selected: None,
            changes,
        }
    }
    fn next(&mut self) -> Option<Report> {
        self.select(None)
    }

    fn select(&mut self, through: Option<&[u64]>) -> Option<Report> {
        self.selected = None;
        // One request per visit, round-robin across logical players, not
        // Mumble subscriptions: shared channels do not duplicate plays.
        for _ in 0..self.players.len() {
            let index = self.next_player;
            self.next_player = (index + 1) % self.players.len();
            let state = self.players[index].state();
            let before = self.cursors[index];
            let report = if let Some(through) = through {
                self.cursors[index].finished(&state, &self.identities, through[index])
            } else {
                self.cursors[index].next(&state, &self.identities)
            };
            if report.is_some() {
                self.selected = Some((index, before));
                return report;
            }
        }
        None
    }

    fn undelivered(&self, through: &[u64]) -> usize {
        self.players
            .iter()
            .enumerate()
            .map(|(index, player)| {
                player
                    .state()
                    .history
                    .iter()
                    .filter(|item| {
                        item.id > self.cursors[index].finished
                            && item.id <= through[index]
                            && qualifies(&item.entry.track, item.started_at_ms, item.played_ms)
                            && source_library(
                                &item.entry.track,
                                item.source.as_ref(),
                                &self.identities,
                            )
                            .is_some()
                    })
                    .count()
            })
            .sum()
    }
    async fn run(mut self, mut stop: watch::Receiver<Option<Stop>>) {
        let mut interval = tokio::time::interval(CHECK_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            self.changes.borrow_and_update();
            let report = {
                let stopping = stop.borrow();
                if let Some(stopping) = stopping.as_ref() {
                    if Instant::now() >= stopping.deadline {
                        let count = self.undelivered(&stopping.through);
                        if count > 0 {
                            tracing::warn!(
                                count,
                                "shutdown grace expired with undelivered playback reports"
                            );
                        }
                        return;
                    }
                    // Drain only already-finished records captured when the
                    // owner stopped us. Later playback cannot prolong shutdown.
                    let report = self.select(Some(&stopping.through));
                    if report.is_none() {
                        return;
                    }
                    report
                } else {
                    self.next()
                }
            };
            if let Some(report) = report {
                let libraries = self.libraries.clone();
                let cancellation = stop.clone();
                let (index, _) = self.selected.expect("selected report has a cursor");
                let history_id = self.cursors[index].finished;
                // Always await the actual blocking call. Async cancellation
                // must not detach live HTTP work and admit another request.
                let result = tokio::task::spawn_blocking(move || {
                    // Read cancellation when the blocking worker actually
                    // starts, so time spent queued cannot extend the grace.
                    let deadline = {
                        let stopping = cancellation.borrow();
                        if let Some(stopping) = stopping.as_ref() {
                            if Instant::now() >= stopping.deadline
                                || !report.submission
                                || history_id > stopping.through[index]
                            {
                                return false;
                            }
                            Some(stopping.deadline)
                        } else {
                            None
                        }
                    };
                    let library = &libraries[report.library];
                    if let Err(error) =
                        library.scrobble(&report.id, report.time, report.submission, deadline)
                    {
                        tracing::warn!(
                            library = %library.name,
                            "playback reporting failed: {}", error.public_message()
                        );
                    }
                    true
                })
                .await;
                match result {
                    Ok(false) => {
                        // A queued request cancelled before its actual call
                        // is undelivered, not attempted. Keep its cursor pending.
                        if let Some((index, before)) = self.selected.take() {
                            self.cursors[index] = before;
                        }
                    }
                    Ok(true) => {}
                    Err(_) => tracing::warn!("playback reporting worker failed"),
                }
                continue;
            }
            tokio::select! {
                biased;
                changed = stop.changed() => {
                    if changed.is_err() { return; }
                }
                changed = self.changes.changed() => {
                    if changed.is_err() { return; }
                }
                _ = interval.tick() => {}
            }
        }
    }
}

/// Keep this handle for the whole runtime and call `shutdown().await` on every
/// exit path. Stop snapshots finished-ID cutoffs synchronously, joins the sole
/// actual HTTP call, and drains within a total 30-second transport-bounded grace.
#[must_use = "the playback reporter must be owned and shut down"]
pub struct Handle {
    stop: watch::Sender<Option<Stop>>,
    task: JoinHandle<()>,
    players: Arc<[Arc<Player>]>,
}

impl Handle {
    fn launch(reporter: Reporter) -> Self {
        let (stop, receiver) = watch::channel(None);
        Self {
            stop,
            players: reporter.players.clone(),
            task: tokio::spawn(reporter.run(receiver)),
        }
    }
    pub fn shutdown(self) -> impl std::future::Future<Output = ()> {
        let Self {
            stop,
            task,
            players,
        } = self;
        let _ = stop.send(Some(Stop::capture(&players, API_TIMEOUT)));
        async move {
            if task.await.is_err() {
                tracing::warn!("playback reporter failed");
            }
        }
    }
}

/// Call synchronously after persistence restore and before spawning any audio
/// tasks. Cursor capture is deliberately outside the spawned future.
pub fn start(bot: &Bot) -> Handle {
    Handle::launch(Reporter::capture(
        &bot.players,
        bot.libraries.clone(),
        bot.changes.subscribe(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Credentials,
        player::{Changes, Ducking, Entry, HistoryOutcome, Restored, RestoredCurrent},
    };

    fn libraries(url: &str) -> Arc<Vec<Library>> {
        let config = serde_json::from_value(serde_json::json!({
            "name": "Test library", "url": url,
        }))
        .unwrap();
        Arc::new(vec![Library::new(
            0,
            &config,
            Credentials::ApiKey("private-key".into()),
        )])
    }

    fn player(changes: &Changes, name: &str) -> Arc<Player> {
        Arc::new(Player::new(
            name.into(),
            50,
            Ducking::default(),
            changes.clone(),
        ))
    }

    fn track(duration: Option<u32>) -> Track {
        Track {
            library: 0,
            id: "same-song".into(),
            title: "Song".into(),
            artist: None,
            album: None,
            album_id: None,
            artist_id: None,
            duration,
            track: None,
            suffix: None,
            content_type: None,
            cover_art: None,
        }
    }

    fn current(
        player: &Player,
        libraries: &[Library],
        entry: u64,
        time: Option<i64>,
        played_ms: u64,
    ) {
        let (history, next_history_id) = {
            let state = player.state();
            (state.history.clone(), state.next_history_id)
        };
        player.restore(Restored {
            paused: false,
            volume: 50,
            ducking: Ducking::default(),
            current: Some(RestoredCurrent {
                entry: Entry {
                    id: entry,
                    track: track(Some(6)),
                    added_by: "listener".into(),
                },
                position: Duration::from_millis(played_ms),
                source: Some(libraries[0].identity()),
                started_at_ms: time,
                played_ms,
            }),
            queue: Vec::new(),
            next_id: entry + 1,
            history,
            next_history_id,
        });
    }

    #[test]
    fn eligibility_uses_transmitted_audio_at_the_half_or_four_minute_boundary() {
        for (duration, below, at) in [
            (Some(1), 499, 500),
            (Some(6), 2999, 3000),
            (Some(480), 239_999, 240_000),
            (Some(u32::MAX), 239_999, 240_000),
            (Some(0), 239_999, 240_000),
            (None, 239_999, 240_000),
        ] {
            let track = track(duration);
            assert!(!qualifies(&track, Some(123), below), "{duration:?}");
            assert!(qualifies(&track, Some(123), at), "{duration:?}");
            assert!(!qualifies(&track, None, at), "unstarted {duration:?}");
            assert!(!qualifies(&track, Some(123), 0), "no audio {duration:?}");
        }
    }

    #[test]
    fn pause_seek_and_a_long_wall_clock_gap_do_not_submit_short_playback() {
        let changes = Changes::default();
        let player = player(&changes, "queue");
        let libraries = libraries("http://127.0.0.1:1");
        let mut reporter = Reporter::capture(
            std::slice::from_ref(&player),
            libraries.clone(),
            changes.subscribe(),
        );
        current(&player, &libraries, 1, Some(1000), 1200);
        assert!(!reporter.next().unwrap().submission);
        player.set_paused(true);
        player.seek(Duration::from_secs(6));
        player.set_paused(false);
        assert!(reporter.next().is_none(), "pause/seek do not reannounce");
        player.skip();
        let mut state = player.state();
        state.history[0].ended_at_ms = 60_000;
        assert_eq!(state.history[0].played_ms, 1200);
        assert_eq!(state.history[0].position_ms, 6000);
        assert_eq!(
            state.history[0].ended_at_ms - state.history[0].started_at_ms.unwrap(),
            59_000
        );
        drop(state);
        assert!(
            reporter.next().is_none(),
            "seek and paused time are not audio"
        );
        assert_eq!(
            reporter.cursors[0].finished, 1,
            "ineligible records are consumed"
        );
    }

    #[test]
    fn shared_channels_report_once_and_repeated_song_ids_are_distinct_plays() {
        let changes = Changes::default();
        let player = player(&changes, "shared queue");
        let libraries = libraries("http://127.0.0.1:1");
        let _first_channel = player.subscribe(0, 1, u32::MAX);
        let _second_channel = player.subscribe(0, 2, u32::MAX);
        let mut reporter = Reporter::capture(
            std::slice::from_ref(&player),
            libraries.clone(),
            changes.subscribe(),
        );
        for (entry, time) in [(1, 1000), (2, 9000)] {
            current(&player, &libraries, entry, Some(time), 3000);
            let expected = Report {
                library: 0,
                id: "same-song".into(),
                time,
                submission: false,
            };
            assert_eq!(reporter.next(), Some(expected));
            changes.notify();
            player.set_paused(true);
            player.seek(Duration::from_secs(2));
            player.set_paused(false);
            assert!(reporter.next().is_none());
            player.skip();
            assert_eq!(
                reporter.next(),
                Some(Report {
                    library: 0,
                    id: "same-song".into(),
                    time,
                    submission: true,
                })
            );
            assert!(
                reporter.next().is_none(),
                "shared destinations do not duplicate history"
            );
        }
        assert_eq!(player.state().history.len(), 2);
    }

    #[test]
    fn synchronous_capture_skips_legacy_history_but_catches_a_first_play_finishing_before_poll() {
        let changes = Changes::default();
        let player = player(&changes, "queue");
        let libraries = libraries("http://127.0.0.1:1");
        current(&player, &libraries, 1, Some(1000), 3000);
        player.skip();
        current(&player, &libraries, 2, None, 0);
        let mut reporter = Reporter::capture(
            std::slice::from_ref(&player),
            libraries.clone(),
            changes.subscribe(),
        );
        // Audio may start and end before the spawned reporter is first polled.
        current(&player, &libraries, 2, Some(9000), 3000);
        player.skip();
        assert_eq!(
            reporter.next(),
            Some(Report {
                library: 0,
                id: "same-song".into(),
                time: 9000,
                submission: true,
            })
        );
        assert!(
            reporter.next().is_none(),
            "restored finished history is not backfilled"
        );
    }

    #[test]
    fn resumed_current_announces_and_submits_only_once_after_startup() {
        let changes = Changes::default();
        let player = player(&changes, "queue");
        let libraries = libraries("http://127.0.0.1:1");
        current(&player, &libraries, 7, Some(1234), 3000);
        let mut reporter = Reporter::capture(
            std::slice::from_ref(&player),
            libraries,
            changes.subscribe(),
        );
        let now_playing = reporter.next().unwrap();
        assert!(!now_playing.submission);
        assert_eq!(now_playing.time, 1234);
        assert!(reporter.next().is_none());
        player.skip();
        let submitted = reporter.next().unwrap();
        assert!(submitted.submission);
        assert_eq!(submitted.time, 1234);
        assert!(reporter.next().is_none());
    }

    #[test]
    fn round_robin_and_alternating_kinds_prevent_backlog_starvation() {
        let changes = Changes::default();
        let first = player(&changes, "first");
        let second = player(&changes, "second");
        let libraries = libraries("http://127.0.0.1:1");
        let mut reporter = Reporter::capture(
            &[first.clone(), second.clone()],
            libraries.clone(),
            changes.subscribe(),
        );
        for entry in 1..=2 {
            current(&first, &libraries, entry, Some(entry as i64 * 1000), 3000);
            first.skip();
        }
        current(&first, &libraries, 3, Some(3000), 3000);
        current(&second, &libraries, 1, Some(10_000), 3000);
        assert_eq!(reporter.next().unwrap().time, 3000);
        assert_eq!(
            reporter.next().unwrap().time,
            10_000,
            "other queue is not starved"
        );
        let oldest = reporter.next().unwrap();
        assert!(oldest.submission);
        assert_eq!(oldest.time, 1000);
        first.skip();
        current(&first, &libraries, 4, Some(4000), 3000);
        let newest = reporter.next().unwrap();
        assert!(
            !newest.submission,
            "history cannot starve a new current entry"
        );
        assert_eq!(newest.time, 4000);
        let next_history = reporter.next().unwrap();
        assert!(
            next_history.submission,
            "new current entries cannot starve history"
        );
        assert_eq!(next_history.time, 2000);
    }

    #[test]
    fn captured_source_never_reports_to_a_replacement_library() {
        let changes = Changes::default();
        let player = player(&changes, "queue");
        let original = libraries("http://127.0.0.1:1/original");
        let replacement = libraries("http://127.0.0.1:1/replacement");
        let mut reporter = Reporter::capture(
            std::slice::from_ref(&player),
            replacement,
            changes.subscribe(),
        );
        current(&player, &original, 1, Some(1000), 3000);
        assert!(reporter.next().is_none());
        player.skip();
        assert!(reporter.next().is_none());
        assert_eq!(reporter.cursors[0].finished, 1);
    }

    #[test]
    fn shutdown_cutoff_does_not_consume_newer_finished_plays() {
        let changes = Changes::default();
        let player = player(&changes, "queue");
        let libraries = libraries("http://127.0.0.1:1");
        let mut reporter = Reporter::capture(
            std::slice::from_ref(&player),
            libraries.clone(),
            changes.subscribe(),
        );
        current(&player, &libraries, 1, Some(1000), 3000);
        player.skip();
        let stop = Stop::capture(std::slice::from_ref(&player), API_TIMEOUT);
        current(&player, &libraries, 2, Some(9000), 3000);
        player.skip();
        assert_eq!(reporter.undelivered(&stop.through), 1);
        let pending = reporter.select(Some(&stop.through)).unwrap();
        assert!(pending.submission);
        assert_eq!(pending.time, 1000);
        assert!(reporter.select(Some(&stop.through)).is_none());
        assert_eq!(reporter.cursors[0].finished, 1);
        assert_eq!(reporter.undelivered(&stop.through), 0);
        assert_eq!(
            reporter.next().unwrap().time,
            9000,
            "later history remains unconsumed"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_joins_actual_call_drains_snapshot_and_reporting_errors_are_not_retried() {
        use crate::subsonic::tests::Server;
        use std::{
            io::Write,
            sync::{
                atomic::{AtomicUsize, Ordering},
                mpsc,
            },
        };

        let (entered, mut entries) = tokio::sync::mpsc::channel(2);
        let (release, gate) = mpsc::channel();
        let attempts = Arc::new(AtomicUsize::new(0));
        let started = attempts.clone();
        let server = Server::new(move |request, socket| {
            let index = started.fetch_add(1, Ordering::SeqCst);
            assert!(request.starts_with("GET /rest/scrobble?"));
            assert!(request.contains(if index == 0 {
                "submission=false"
            } else {
                "submission=true"
            }));
            entered.blocking_send(index).unwrap();
            gate.recv_timeout(Duration::from_secs(2)).unwrap();
            if index == 0 {
                socket.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            } else {
                let body = br#"{"subsonic-response":{"status":"ok"}}"#;
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                socket.write_all(body).unwrap();
            }
        });
        let changes = Changes::default();
        let player = player(&changes, "queue");
        let libraries = libraries(&server.url);
        let reporter = Reporter::capture(
            std::slice::from_ref(&player),
            libraries.clone(),
            changes.subscribe(),
        );
        current(&player, &libraries, 1, Some(1000), 3000);
        let handle = Handle::launch(reporter);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), entries.recv())
                .await
                .unwrap(),
            Some(0)
        );
        // Playback locks remain usable while the first actual HTTP call waits.
        player.skip();
        let stopping = handle.shutdown();
        // This finishes after the synchronous stop snapshot and is not admitted.
        current(&player, &libraries, 2, Some(9000), 3000);
        player.skip();
        let joined = tokio::spawn(stopping);
        tokio::task::yield_now().await;
        assert!(!joined.is_finished(), "stop must await the actual call");
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        release.send(()).unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), entries.recv())
                .await
                .unwrap(),
            Some(1)
        );
        assert!(
            !joined.is_finished(),
            "stop also owns the pending finished report"
        );
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), joined)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "no retries or later-event admission"
        );
    }

    #[test]
    fn now_playing_waits_for_actual_audio_not_just_a_selected_entry_or_position() {
        let changes = Changes::default();
        let player = player(&changes, "queue");
        let libraries = libraries("http://127.0.0.1:1");
        let mut reporter = Reporter::capture(
            std::slice::from_ref(&player),
            libraries.clone(),
            changes.subscribe(),
        );
        current(&player, &libraries, 1, None, 0);
        player.seek(Duration::from_secs(6));
        assert!(reporter.next().is_none());
        player.state().current.as_mut().unwrap().started_at_ms = Some(1234);
        assert!(
            reporter.next().is_none(),
            "a timestamp alone is not transmitted audio"
        );
        player.state().current.as_mut().unwrap().played_ms = 20;
        let started = reporter.next().unwrap();
        assert!(!started.submission);
        assert_eq!(started.time, 1234);
        assert!(reporter.next().is_none());
        player.skip();
        assert!(
            reporter.next().is_none(),
            "one frame cannot submit a six-second song"
        );
    }

    #[test]
    fn completed_skipped_and_failed_plays_use_the_same_actual_audio_threshold() {
        let changes = Changes::default();
        let player = player(&changes, "queue");
        let libraries = libraries("http://127.0.0.1:1");
        let mut reporter = Reporter::capture(
            std::slice::from_ref(&player),
            libraries.clone(),
            changes.subscribe(),
        );
        let mut entry = 1;
        for outcome in [
            HistoryOutcome::Completed,
            HistoryOutcome::Skipped,
            HistoryOutcome::Failed,
        ] {
            for played_ms in [2980, 3000] {
                current(
                    &player,
                    &libraries,
                    entry,
                    Some(entry as i64 * 1000),
                    played_ms,
                );
                player.skip();
                player.state().history[0].outcome = outcome;
                let report = reporter.next();
                assert_eq!(report.is_some(), played_ms == 3000, "{outcome:?}");
                if let Some(report) = report {
                    assert!(report.submission);
                }
                assert!(reporter.next().is_none());
                entry += 1;
            }
        }
    }
}
