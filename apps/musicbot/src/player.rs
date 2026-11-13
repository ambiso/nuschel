// SPDX-License-Identifier: EUPL-1.2
//! A queue and its playback. Each player encodes once and sends the same Opus
//! packets to every channel attached to it; with nobody attached it waits.
use crate::{
    decode::{self, CHANNELS, Decoding, FRAME},
    subsonic::{Library, LibraryIdentity, Track},
};
use nuschl_ipc::musicbot::{Client, Role};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{Notify, broadcast, mpsc::error::TryRecvError, watch},
    time::{Instant, sleep, sleep_until},
};

const TICK: Duration = Duration::from_millis(20);
/// Mumble's per-packet overhead (IP, UDP, encryption, voice header), rounded
/// up, at 50 packets per second.
const OVERHEAD_BPS: u32 = 64 * 8 * 50;
const MIN_BITRATE: u32 = 6_000;
/// After this many frames without audio from the decoder, talking ends.
const UNDERRUN_END: u32 = 10;
/// After this many frames without audio, viewers see the song buffering.
const UNDERRUN_SHOWN: u32 = 5;

/// Zero means this destination cannot support even the minimum Opus rate.
pub(crate) fn bitrate_for_bandwidth(configured: u32, bandwidth: u32) -> u32 {
    let available = bandwidth.saturating_sub(OVERHEAD_BPS);
    if available < MIN_BITRATE {
        0
    } else {
        available.min(configured.max(MIN_BITRATE))
    }
}
pub const MAX_QUEUE: usize = 5000;
pub const MAX_HISTORY: usize = 1000;
/// The longest a queue may stay quiet after the last word (the web page's limit).
pub const MAX_DUCK_HOLD: Duration = Duration::from_secs(10);
/// How much of the way to the target gain each frame goes: getting quieter
/// takes about 60 ms, getting louder again about half a second.
const DUCK_ATTACK: f32 = 0.5;
const DUCK_RELEASE: f32 = 0.08;

/// Lowering the music while people in the channel speak (voice activity,
/// not merely an open microphone: `vad.rs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ducking {
    pub enabled: bool,
    /// The music's volume while ducked, in percent of its normal volume.
    pub level: u8,
    /// How long the music stays quieter after the last speech.
    pub hold: Duration,
    /// How readily sound counts as speech, in percent.
    pub sensitivity: u8,
}

impl Default for Ducking {
    fn default() -> Self {
        Self {
            enabled: true,
            level: 30,
            hold: Duration::from_millis(800),
            sensitivity: 50,
        }
    }
}

/// An encoded 20 ms frame; `end` closes a transmission.
#[derive(Debug)]
pub struct Packet {
    pub opus: Vec<u8>,
    pub end: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub id: u64,
    pub track: Track,
    pub added_by: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HistoryOutcome {
    Completed,
    Skipped,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryItem {
    /// Monotonic ending-order ID, independent of the queue entry ID.
    pub id: u64,
    pub entry: Entry,
    pub source: Option<LibraryIdentity>,
    pub started_at_ms: Option<i64>,
    pub ended_at_ms: i64,
    /// Duration of audio packets successfully broadcast, not the track position.
    pub played_ms: u64,
    pub position_ms: u64,
    pub outcome: HistoryOutcome,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Playing {
    pub entry: Entry,
    /// The position, in frames.
    pub frames: u64,
    pub source: Option<LibraryIdentity>,
    pub started_at_ms: Option<i64>,
    pub played_ms: u64,
    /// Waiting for audio from the library.
    pub buffering: bool,
    /// Counts seeks, so that the decoder restarts at the new position.
    generation: u64,
}

impl Playing {
    pub fn elapsed(&self) -> Duration {
        Duration::from_millis(self.frames.saturating_mul(20))
    }

    fn record_audio(&mut self, now: impl FnOnce() -> i64) {
        if self.started_at_ms.is_none() {
            self.started_at_ms = Some(now());
        }
        self.played_ms = self.played_ms.saturating_add(20);
    }
}

#[derive(Debug)]
pub struct State {
    pub current: Option<Playing>,
    pub queue: VecDeque<Entry>,
    pub paused: bool,
    pub volume: u8,
    pub ducking: Ducking,
    /// Why the last song stopped early.
    pub error: Option<String>,
    pub next_id: u64,
    pub history: VecDeque<HistoryItem>,
    pub next_history_id: u64,
}

impl State {
    fn start_next(&mut self, source: impl FnOnce(&Track) -> Option<LibraryIdentity>) -> bool {
        if self.current.is_some() || self.paused {
            return false;
        }
        let Some(entry) = self.queue.pop_front() else {
            return false;
        };
        self.error = None;
        self.current = Some(Playing {
            source: source(&entry.track),
            entry,
            frames: 0,
            started_at_ms: None,
            played_ms: 0,
            buffering: true,
            generation: 0,
        });
        true
    }

    fn fail(
        &mut self,
        entry: u64,
        generation: u64,
        error: String,
        now: impl FnOnce() -> i64,
    ) -> bool {
        if !self.matches(entry, generation) {
            return false;
        }
        let title = &self.current.as_ref().unwrap().entry.track.title;
        let error = format!("{title}: {error}");
        self.error = Some(error.clone());
        self.finish_current(HistoryOutcome::Failed, Some(error), now);
        true
    }

    fn matches(&self, entry: u64, generation: u64) -> bool {
        self.current
            .as_ref()
            .is_some_and(|playing| playing.entry.id == entry && playing.generation == generation)
    }

    /// Completion belongs to the decoder generation, not merely its queue entry.
    fn finish(
        &mut self,
        entry: u64,
        generation: u64,
        outcome: HistoryOutcome,
        error: Option<String>,
        now: impl FnOnce() -> i64,
    ) -> bool {
        if !self.matches(entry, generation) {
            return false;
        }
        self.finish_current(outcome, error, now);
        true
    }

    fn finish_current(
        &mut self,
        outcome: HistoryOutcome,
        error: Option<String>,
        now: impl FnOnce() -> i64,
    ) {
        let Some(playing) = self.current.take() else {
            return;
        };
        if outcome != HistoryOutcome::Failed && playing.started_at_ms.is_none() {
            return;
        }
        let position_ms = playing.frames.saturating_mul(20);
        let id = self.next_history_id;
        self.next_history_id = self.next_history_id.saturating_add(1);
        self.history.push_front(HistoryItem {
            id,
            entry: playing.entry,
            source: playing.source,
            started_at_ms: playing.started_at_ms,
            ended_at_ms: now(),
            played_ms: playing.played_ms,
            position_ms,
            outcome,
            error,
        });
        self.history.truncate(MAX_HISTORY);
    }
}

/// State restored before playback starts. Entry IDs remain stable for open viewers.
pub struct Restored {
    pub paused: bool,
    pub volume: u8,
    pub ducking: Ducking,
    pub current: Option<RestoredCurrent>,
    pub queue: Vec<Entry>,
    pub next_id: u64,
    pub history: VecDeque<HistoryItem>,
    pub next_history_id: u64,
}

pub struct RestoredCurrent {
    pub entry: Entry,
    pub position: Duration,
    pub source: Option<LibraryIdentity>,
    pub started_at_ms: Option<i64>,
    pub played_ms: u64,
}

/// Tells the web page's viewers that something changed: a counter they watch.
#[derive(Clone)]
pub struct Changes(Arc<watch::Sender<u64>>);

impl Default for Changes {
    fn default() -> Self {
        Self(Arc::new(watch::Sender::new(0)))
    }
}

impl Changes {
    pub fn notify(&self) {
        self.0.send_modify(|count| *count = count.wrapping_add(1));
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.0.subscribe()
    }
}

pub struct Player {
    pub name: String,
    state: Mutex<State>,
    wake: Notify,
    packets: broadcast::Sender<Arc<Packet>>,
    /// Bandwidth limits of the attached connections, by channel.
    limits: Mutex<BTreeMap<usize, u32>>,
    /// When somebody in an attached channel last spoke.
    speech: Mutex<Option<Instant>>,
    changes: Changes,
}

/// A channel's attachment to a player: its packets, until dropped.
pub struct Subscription {
    pub player: usize,
    pub packets: broadcast::Receiver<Arc<Packet>>,
    owner: Arc<Player>,
    channel: usize,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        lock(&self.owner.limits).remove(&self.channel);
        // One listener fewer: the position may stop advancing.
        self.owner.changes.notify();
        self.owner.wake.notify_one();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn wall_clock_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis().min(i64::MAX as u128) as i64,
        Err(error) => -(error.duration().as_millis().min(i64::MAX as u128) as i64),
    }
}

impl Player {
    pub fn new(name: String, volume: u8, ducking: Ducking, changes: Changes) -> Self {
        Self {
            name,
            state: Mutex::new(State {
                current: None,
                queue: VecDeque::new(),
                paused: false,
                volume: volume.min(100),
                ducking,
                error: None,
                next_id: 1,
                history: VecDeque::new(),
                next_history_id: 1,
            }),
            wake: Notify::new(),
            // About a second; a slower connection skips ahead.
            packets: broadcast::channel(50).0,
            limits: Mutex::new(BTreeMap::new()),
            speech: Mutex::new(None),
            changes,
        }
    }

    /// Somebody in a channel that plays this queue speaks.
    pub fn heard_speech(&self) {
        *lock(&self.speech) = Some(Instant::now());
    }

    /// Whether the music is lowered for speech right now.
    pub fn ducked(&self, ducking: &Ducking) -> bool {
        ducking.enabled && lock(&self.speech).is_some_and(|spoken| spoken.elapsed() < ducking.hold)
    }

    pub fn set_ducking(&self, ducking: Ducking) {
        self.change(|state| {
            state.ducking = Ducking {
                level: ducking.level.min(100),
                hold: ducking.hold.min(MAX_DUCK_HOLD),
                sensitivity: ducking.sensitivity.min(100),
                ..ducking
            }
        });
    }

    pub fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn change<R>(&self, change: impl FnOnce(&mut State) -> R) -> R {
        let result = change(&mut self.state());
        self.wake.notify_one();
        self.changes.notify();
        result
    }

    /// Attaches channel `channel`; a zero budget prevents transmission.
    pub fn subscribe(
        self: &Arc<Self>,
        index: usize,
        channel: usize,
        max_bandwidth: u32,
    ) -> Subscription {
        lock(&self.limits).insert(channel, max_bandwidth);
        let subscription = Subscription {
            player: index,
            packets: self.packets.subscribe(),
            owner: self.clone(),
            channel,
        };
        self.wake.notify_one();
        self.changes.notify();
        subscription
    }

    pub(crate) fn update_bandwidth(&self, channel: usize, bandwidth: u32) {
        let changed = {
            let mut limits = lock(&self.limits);
            if let Some(limit) = limits.get_mut(&channel) {
                if *limit == bandwidth {
                    false
                } else {
                    *limit = bandwidth;
                    true
                }
            } else {
                false
            }
        };
        if changed {
            self.wake.notify_one();
            self.changes.notify();
        }
    }

    pub fn listeners(&self) -> usize {
        self.packets.receiver_count()
    }

    /// Adds songs in order at `index` (clamped; `None`: at the end); returns
    /// how many fit.
    pub fn enqueue(&self, tracks: Vec<Track>, added_by: &str, index: Option<usize>) -> usize {
        self.change(|state| {
            let room = MAX_QUEUE.saturating_sub(state.queue.len());
            let first = index.unwrap_or(usize::MAX).min(state.queue.len());
            let mut added = 0;
            for (at, track) in (first..).zip(tracks.into_iter().take(room)) {
                let id = state.next_id;
                state.next_id += 1;
                state.queue.insert(
                    at,
                    Entry {
                        id,
                        track,
                        added_by: added_by.to_owned(),
                    },
                );
                added += 1;
            }
            added
        })
    }

    pub fn remove(&self, entry: u64) {
        self.change(|state| state.queue.retain(|queued| queued.id != entry));
    }

    /// Moves an entry to `index` of the queue without it (clamped).
    pub fn move_to(&self, entry: u64, index: usize) {
        self.change(|state| {
            let Some(position) = state.queue.iter().position(|queued| queued.id == entry) else {
                return;
            };
            if let Some(moved) = state.queue.remove(position) {
                let index = index.min(state.queue.len());
                state.queue.insert(index, moved);
            }
        });
    }

    /// Continues the current song at `position` (clamped to its length).
    pub fn seek(&self, position: Duration) {
        self.change(|state| {
            if let Some(playing) = state.current.as_mut() {
                let length = playing
                    .entry
                    .track
                    .duration
                    .map_or(u64::MAX, |seconds| u64::from(seconds) * 50);
                playing.frames =
                    (position.as_millis() / TICK.as_millis()).min(u128::from(length)) as u64;
                playing.generation += 1;
                playing.buffering = true;
            }
        });
    }

    /// Restores a saved queue before it plays, retaining entry IDs and position.
    pub fn restore(&self, restored: Restored) {
        self.change(|state| {
            state.current = restored.current.map(|current| {
                let frames = current.position.as_millis() / TICK.as_millis();
                let length = current
                    .entry
                    .track
                    .duration
                    .map_or(u64::MAX, |seconds| u64::from(seconds) * 50);
                Playing {
                    entry: current.entry,
                    frames: frames.min(u128::from(length)) as u64,
                    source: current.source,
                    started_at_ms: current.started_at_ms,
                    played_ms: current.played_ms,
                    buffering: true,
                    generation: 0,
                }
            });
            state.queue = restored.queue.into_iter().take(MAX_QUEUE).collect();
            let largest = state
                .queue
                .iter()
                .map(|entry| entry.id)
                .chain(state.current.iter().map(|playing| playing.entry.id))
                .max()
                .unwrap_or(0);
            state.next_id = restored.next_id.max(largest.saturating_add(1));
            state.history = restored.history;
            state
                .history
                .make_contiguous()
                .sort_unstable_by_key(|item| std::cmp::Reverse(item.id));
            state.next_history_id = restored.next_history_id.max(1).max(
                state
                    .history
                    .front()
                    .map_or(1, |item| item.id.saturating_add(1)),
            );
            state.history.truncate(MAX_HISTORY);
            state.paused = restored.paused;
            state.volume = restored.volume.min(100);
            state.ducking = restored.ducking;
        });
    }

    /// Startup restoration cannot resume a current track from a removed library.
    pub(crate) fn fail_restored(&self, error: String) {
        self.change(|state| {
            if let Some(playing) = &state.current {
                state.fail(playing.entry.id, playing.generation, error, wall_clock_ms);
            }
        });
    }

    pub fn clear(&self) {
        self.change(|state| state.queue.clear());
    }

    /// Ends the current song; the next one starts unless paused.
    pub fn skip(&self) {
        self.change(|state| state.finish_current(HistoryOutcome::Skipped, None, wall_clock_ms));
    }

    pub fn set_paused(&self, paused: bool) {
        self.change(|state| state.paused = paused);
    }

    pub fn set_volume(&self, volume: u8) {
        self.change(|state| state.volume = volume.min(100));
    }

    /// The bitrate for the attached connections: the configured one, or less
    /// where a server allows less.
    fn bitrate(&self, configured: u32) -> u32 {
        lock(&self.limits)
            .values()
            .map(|limit| bitrate_for_bandwidth(configured, *limit))
            .fold(configured, u32::min)
    }

    /// The caller holds the current-state lock through sending and recording.
    fn broadcast_audio(
        &self,
        playing: &mut Playing,
        opus: &[u8],
        now: impl FnOnce() -> i64,
    ) -> bool {
        playing.frames = playing.frames.saturating_add(1);
        if self
            .packets
            .send(Arc::new(Packet {
                opus: opus.to_vec(),
                end: false,
            }))
            .is_err()
        {
            return false;
        }
        playing.record_audio(now);
        true
    }

    /// Plays the queue for as long as the program runs.
    pub async fn run(
        self: Arc<Self>,
        libraries: Arc<Vec<Library>>,
        configured_bitrate: u32,
        codec_worker: PathBuf,
    ) {
        let mut encoder = Client::new(Role::Encode, codec_worker.clone());
        // At most one raw frame survives an asynchronous encode. A pause or
        // temporarily absent listener cannot consume it without advancing time.
        let mut held: Option<Vec<f32>> = None;
        let mut decoding: Option<Decoding> = None;
        let mut talking = false;
        let mut last_bitrate = configured_bitrate;
        let mut reset_encoder = false;
        let mut underrun = 0;
        let mut deadline: Option<Instant> = None;
        // The gain of the last frame; changes are ramped so they do not click.
        let mut gain = 0.0f32;
        let mut was_ducked = false;
        loop {
            if reset_encoder {
                if let Err(error) = encoder.clear().await {
                    tracing::warn!("{}: encoder reset failed: {error}", self.name);
                    sleep(Duration::from_millis(500)).await;
                    continue;
                }
                reset_encoder = false;
            }
            let replace = {
                let mut state = self.state();
                if state.start_next(|track| libraries.get(track.library).map(Library::identity)) {
                    self.changes.notify();
                }
                match &state.current {
                    Some(playing) => !decoding.as_ref().is_some_and(|running| {
                        running.entry == playing.entry.id
                            && running.generation == playing.generation
                    }),
                    None => decoding.is_some(),
                }
            };
            if replace {
                held = None;
                if let Some(previous) = decoding.take() {
                    let finished = previous.frames.is_closed() && previous.frames.is_empty();
                    if talking && !finished {
                        self.end(&mut encoder, last_bitrate).await;
                        talking = false;
                    }
                    previous.cancel().await;
                }
                // A seek or skip may have arrived while the old read was stopping.
                let request = {
                    let mut state = self.state();
                    state.current.as_mut().map(|playing| {
                        if playing.source.is_none() {
                            playing.source = libraries
                                .get(playing.entry.track.library)
                                .map(Library::identity);
                        }
                        let start = decode::Start {
                            entry: playing.entry.id,
                            generation: playing.generation,
                            frames: playing.frames,
                        };
                        (playing.entry.track.clone(), start)
                    })
                };
                decoding = request.map(|(track, start)| {
                    decode::start(libraries.clone(), codec_worker.clone(), track, start)
                });
                underrun = 0;
                deadline = None;
            }
            let (paused, volume, ducking) = {
                let state = self.state();
                (state.paused, state.volume, state.ducking)
            };
            let Some(running) = decoding.as_mut().filter(|_| {
                !paused && self.listeners() > 0 && self.bitrate(configured_bitrate) > 0
            }) else {
                if talking {
                    self.end(&mut encoder, last_bitrate).await;
                    talking = false;
                }
                deadline = None;
                // Woken by changes; the timeout notices channels leaving.
                tokio::select! {
                    _ = self.wake.notified() => {}
                    _ = sleep(Duration::from_millis(500)) => {}
                }
                continue;
            };
            let next = deadline.get_or_insert_with(Instant::now);
            sleep_until(*next).await;
            *next += TICK;
            let now = Instant::now();
            if *next + TICK * 10 < now {
                // Far behind (a suspended machine): resume from now.
                *next = now;
            }
            let ducked = self.ducked(&ducking);
            if ducked != was_ducked {
                was_ducked = ducked;
                self.changes.notify();
            }
            let bitrate = self.bitrate(configured_bitrate);
            let receive = {
                let state = self.state();
                if state.paused
                    || self.listeners() == 0
                    || bitrate == 0
                    || !state.matches(running.entry, running.generation)
                {
                    continue;
                }
                held.take()
                    .map(|frame| Ok(Ok(frame)))
                    .unwrap_or_else(|| running.frames.try_recv())
            };
            match receive {
                Ok(Ok(frame)) => {
                    let target = target_gain(volume, &ducking, ducked);
                    let next = next_gain(gain, target);
                    let mut samples = frame.clone();
                    apply_gain(&mut samples, gain, next);
                    let encoded = encoder.encode(samples, bitrate).await;
                    // Revalidate after IPC, before any broadcast/accounting.
                    // Seek, skip, pause and destination changes remain immediate
                    // while the unique encoder owns its one outstanding request.
                    let mut state = self.state();
                    if !state.matches(running.entry, running.generation) {
                        reset_encoder = true;
                        continue;
                    }
                    if state.paused
                        || self.listeners() == 0
                        || self.bitrate(configured_bitrate) < bitrate
                    {
                        held = Some(frame);
                        reset_encoder = true;
                        continue;
                    }
                    match encoded {
                        Ok(opus) => {
                            last_bitrate = bitrate;
                            gain = next;
                            let playing = state.current.as_mut().unwrap();
                            talking |= self.broadcast_audio(playing, &opus, wall_clock_ms);
                            if std::mem::take(&mut playing.buffering) {
                                self.changes.notify();
                            }
                        }
                        Err(error) => {
                            state.fail(running.entry, running.generation, error, wall_clock_ms);
                            self.changes.notify();
                        }
                    }
                    underrun = 0;
                }
                Ok(Err(error)) => {
                    let mut state = self.state();
                    if state.fail(running.entry, running.generation, error, wall_clock_ms) {
                        self.changes.notify();
                    }
                }
                Err(TryRecvError::Empty) => {
                    underrun += 1;
                    if underrun == UNDERRUN_END && talking {
                        self.end(&mut encoder, last_bitrate).await;
                        talking = false;
                    }
                    let mut state = self.state();
                    if state.matches(running.entry, running.generation)
                        && underrun >= UNDERRUN_SHOWN
                        && let Some(playing) = state.current.as_mut()
                        && !std::mem::replace(&mut playing.buffering, true)
                    {
                        self.changes.notify();
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    let mut state = self.state();
                    if state.finish(
                        running.entry,
                        running.generation,
                        HistoryOutcome::Completed,
                        None,
                        wall_clock_ms,
                    ) {
                        self.changes.notify();
                    }
                }
            }
        }
    }

    /// A frame of silence that closes the transmission, so clients stop
    /// showing the bot as talking.
    async fn end(&self, encoder: &mut Client, bitrate: u32) {
        if let Ok(opus) = encoder.encode(vec![0.; FRAME * CHANNELS], bitrate).await {
            let _ = self.packets.send(Arc::new(Packet { opus, end: true }));
        }
    }
}

/// The music's gain for a volume in percent (squared, so the slider follows
/// loudness rather than amplitude), lowered while ducked.
fn target_gain(volume: u8, ducking: &Ducking, ducked: bool) -> f32 {
    let gain = (f32::from(volume) / 100.0).powi(2);
    if ducked {
        gain * f32::from(ducking.level) / 100.0
    } else {
        gain
    }
}

/// One frame's step towards `target`: quickly down, slowly up.
fn next_gain(current: f32, target: f32) -> f32 {
    let rate = if target < current {
        DUCK_ATTACK
    } else {
        DUCK_RELEASE
    };
    let next = current + (target - current) * rate;
    // Close enough is there; otherwise the last steps never end.
    if (target - next).abs() < 1e-4 {
        target
    } else {
        next
    }
}

/// Scales a frame by a gain that moves linearly from `from` to `to`.
fn apply_gain(frame: &mut [f32], from: f32, to: f32) {
    let frames = (frame.len() / CHANNELS).max(1) as f32;
    for (index, samples) in frame.as_chunks_mut::<CHANNELS>().0.iter_mut().enumerate() {
        let gain = from + (to - from) * (index + 1) as f32 / frames;
        for sample in samples {
            *sample = (*sample * gain).clamp(-1.0, 1.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str) -> Track {
        Track {
            library: 0,
            id: id.into(),
            title: id.into(),
            artist: None,
            album: None,
            album_id: None,
            artist_id: None,
            duration: None,
            track: None,
            suffix: None,
            content_type: None,
            cover_art: None,
        }
    }

    fn queued(player: &Player) -> Vec<String> {
        player
            .state()
            .queue
            .iter()
            .map(|entry| entry.track.id.clone())
            .collect()
    }

    fn player() -> Player {
        Player::new("test".into(), 50, Ducking::default(), Changes::default())
    }

    fn start_next(player: &Player) -> (u64, u64) {
        let mut state = player.state();
        assert!(state.start_next(|_| None));
        let playing = state.current.as_ref().unwrap();
        (playing.entry.id, playing.generation)
    }

    fn audio(player: &Player, entry: u64, generation: u64, now: impl FnOnce() -> i64) -> bool {
        let mut state = player.state();
        if state.paused || !state.matches(entry, generation) {
            return false;
        }
        player.broadcast_audio(state.current.as_mut().unwrap(), &[1, 2, 3], now)
    }

    #[test]
    fn history_counts_successful_packets_once_for_a_shared_queue() {
        let player = Arc::new(player());
        player.enqueue(vec![track("a")], "ada", None);
        let (entry, generation) = start_next(&player);
        assert!(!audio(&player, entry, generation, || panic!(
            "no receiver, no start clock"
        )));
        assert_eq!(player.state().current.as_ref().unwrap().started_at_ms, None);
        let mut first = player.subscribe(0, 1, u32::MAX);
        let mut second = player.subscribe(0, 2, u32::MAX);
        assert!(audio(&player, entry, generation, || 1000));
        assert!(audio(&player, entry, generation, || panic!(
            "read clock only at start"
        )));
        assert_eq!(first.packets.try_recv().unwrap().opus, [1, 2, 3]);
        assert_eq!(second.packets.try_recv().unwrap().opus, [1, 2, 3]);
        player.set_paused(true);
        assert!(!audio(&player, entry, generation, || panic!("paused")));
        player.set_paused(false);
        player.seek(Duration::from_secs(4));
        assert!(!audio(&player, entry, generation, || panic!(
            "stale seek audio"
        )));
        assert!(audio(&player, entry, generation + 1, || panic!(
            "seek retains start"
        )));
        let mut state = player.state();
        assert!(state.finish(
            entry,
            generation + 1,
            HistoryOutcome::Completed,
            None,
            || 5000
        ));
        assert!(!state.finish(
            entry,
            generation + 1,
            HistoryOutcome::Completed,
            None,
            || panic!("duplicate EOF")
        ));
        assert_eq!(state.history.len(), 1);
        let item = &state.history[0];
        assert_eq!(item.started_at_ms, Some(1000));
        assert_eq!(item.ended_at_ms, 5000);
        assert_eq!(
            item.played_ms, 60,
            "not listener count, pauses or sought position"
        );
        assert_eq!(item.position_ms, 4020);
        assert_eq!(item.outcome, HistoryOutcome::Completed);
        assert_eq!(state.next_history_id, 2);
    }

    #[test]
    fn stale_seek_completion_and_failure_cannot_end_the_current_play() {
        let player = Arc::new(player());
        let _listener = player.subscribe(0, 0, u32::MAX);
        player.enqueue(vec![track("a"), track("b")], "ada", None);
        let (entry, generation) = start_next(&player);
        assert!(audio(&player, entry, generation, || 10));
        player.seek(Duration::from_secs(2));
        {
            let mut state = player.state();
            assert!(!state.finish(
                entry,
                generation,
                HistoryOutcome::Completed,
                None,
                || panic!("stale EOF")
            ));
            assert!(
                !state.fail(entry, generation, "old decoder".into(), || panic!(
                    "stale error"
                ))
            );
            assert!(!state.finish(
                entry + 1,
                generation + 1,
                HistoryOutcome::Completed,
                None,
                || panic!("wrong entry")
            ));
            assert!(state.history.is_empty());
            assert!(state.error.is_none());
            assert!(state.fail(entry, generation + 1, "decode failed".into(), || 20));
            assert!(!state.finish(
                entry,
                generation + 1,
                HistoryOutcome::Completed,
                None,
                || panic!("error already archived")
            ));
        }
        let (next, next_generation) = start_next(&player);
        let state = player.state();
        assert_eq!(next, entry + 1);
        assert_eq!(next_generation, 0);
        assert!(
            state.error.is_none(),
            "a new song clears only the live error"
        );
        assert_eq!(state.history[0].error.as_deref(), Some("a: decode failed"));
        assert_eq!(state.history[0].outcome, HistoryOutcome::Failed);
        assert_eq!(state.history[0].played_ms, 20);
        assert_eq!(state.history[0].position_ms, 2000);
    }

    #[test]
    fn a_restored_play_resumes_its_original_start_and_packet_duration() {
        let player = Arc::new(player());
        let source = LibraryIdentity {
            name: "original library".into(),
            url: "http://library.example".into(),
        };
        player.restore(Restored {
            paused: true,
            volume: 50,
            ducking: Ducking::default(),
            current: Some(RestoredCurrent {
                entry: Entry {
                    id: 41,
                    track: track("a"),
                    added_by: "ada".into(),
                },
                position: Duration::from_secs(5),
                source: Some(source.clone()),
                started_at_ms: Some(1000),
                played_ms: 80,
            }),
            queue: Vec::new(),
            next_id: 42,
            history: VecDeque::new(),
            next_history_id: 1,
        });
        let _listener = player.subscribe(0, 0, u32::MAX);
        assert!(!audio(&player, 41, 0, || panic!("restored pause")));
        player.set_paused(false);
        assert!(audio(&player, 41, 0, || panic!(
            "restart must not reset first audio"
        )));
        player.seek(Duration::from_secs(2));
        assert!(audio(&player, 41, 1, || panic!(
            "seek must not reset first audio"
        )));
        let mut state = player.state();
        assert!(state.finish(41, 1, HistoryOutcome::Completed, None, || 9000));
        assert_eq!(state.history.len(), 1);
        let item = &state.history[0];
        assert_eq!(item.started_at_ms, Some(1000));
        assert_eq!(
            item.played_ms, 120,
            "saved audio plus only two newly broadcast packets"
        );
        assert_eq!(item.position_ms, 2020);
        assert_eq!(item.source.as_ref(), Some(&source));
        assert_eq!(item.entry.id, 41);
        assert_eq!(state.next_history_id, 2);
    }

    #[test]
    fn queued_removals_and_unplayed_skips_do_not_create_history() {
        let player = Arc::new(player());
        player.enqueue(
            vec![track("removed"), track("skipped"), track("cleared")],
            "ada",
            None,
        );
        let removed = player.state().queue[0].id;
        player.remove(removed);
        start_next(&player);
        player.skip();
        player.clear();
        player.skip();
        player.enqueue(vec![track("empty")], "ada", None);
        let (empty, generation) = start_next(&player);
        player.seek(Duration::from_secs(4));
        assert!(player.state().finish(
            empty,
            generation + 1,
            HistoryOutcome::Completed,
            None,
            || panic!("never-started EOF is not a play"),
        ));
        {
            let state = player.state();
            assert!(state.history.is_empty());
            assert_eq!(state.next_history_id, 1);
        }
        player.enqueue(vec![track("failed")], "ada", None);
        let (entry, generation) = start_next(&player);
        assert!(
            player
                .state()
                .fail(entry, generation, "no audio".into(), || 100)
        );
        {
            let state = player.state();
            let failed = &state.history[0];
            assert_eq!(failed.started_at_ms, None);
            assert_eq!(failed.played_ms, 0);
            assert_eq!(failed.outcome, HistoryOutcome::Failed);
        }
        let _listener = player.subscribe(0, 0, u32::MAX);
        player.enqueue(vec![track("played")], "ada", None);
        let (entry, generation) = start_next(&player);
        assert!(audio(&player, entry, generation, || 200));
        player.skip();
        player.skip();
        let state = player.state();
        assert_eq!(state.history.len(), 2);
        assert_eq!(state.history[0].outcome, HistoryOutcome::Skipped);
        assert_eq!(state.history[0].played_ms, 20);
        assert_eq!(state.history[0].id, 2);
        assert_eq!(state.next_history_id, 3);
    }

    #[test]
    fn retained_history_is_newest_first_with_monotonic_ending_ids() {
        let player = player();
        for n in 0..MAX_HISTORY + 3 {
            player.enqueue(vec![track(&n.to_string())], "ada", None);
            let (entry, generation) = start_next(&player);
            assert!(
                player
                    .state()
                    .fail(entry, generation, "failed".into(), || n as i64)
            );
        }
        let state = player.state();
        assert_eq!(state.history.len(), MAX_HISTORY);
        assert_eq!(state.history.front().unwrap().id, (MAX_HISTORY + 3) as u64);
        assert_eq!(state.history.back().unwrap().id, 4);
        assert_eq!(state.next_history_id, (MAX_HISTORY + 4) as u64);
        assert!(
            state
                .history
                .iter()
                .zip(state.history.iter().skip(1))
                .all(|(newer, older)| newer.id == older.id + 1)
        );
    }

    #[test]
    fn a_racing_skip_waits_for_the_transmitted_packet_to_be_recorded() {
        let player = Arc::new(player());
        let mut listener = player.subscribe(0, 0, u32::MAX);
        player.enqueue(vec![track("a")], "ada", None);
        let (entry, generation) = start_next(&player);
        let mut state = player.state();
        let (ready, started) = std::sync::mpsc::channel();
        let skipping = player.clone();
        let thread = std::thread::spawn(move || {
            ready.send(()).unwrap();
            skipping.skip();
        });
        started.recv().unwrap();
        assert!(player.broadcast_audio(state.current.as_mut().unwrap(), &[1], || 100));
        assert!(
            state.history.is_empty(),
            "skip cannot finish while sending holds the state lock"
        );
        drop(state);
        thread.join().unwrap();
        assert_eq!(listener.packets.try_recv().unwrap().opus, [1]);
        assert!(!audio(&player, entry, generation, || panic!(
            "stale skip audio"
        )));
        assert!(matches!(
            listener.packets.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        let state = player.state();
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].played_ms, 20);
        assert_eq!(state.history[0].outcome, HistoryOutcome::Skipped);
    }

    #[test]
    fn moving_places_the_entry_at_the_index_of_the_queue_without_it() {
        let player = player();
        player.enqueue(
            vec![track("a"), track("b"), track("c"), track("d")],
            "ada",
            None,
        );
        let ids: Vec<u64> = player.state().queue.iter().map(|entry| entry.id).collect();
        // Dragging "a" below "c": index 2 of [b, c, d].
        player.move_to(ids[0], 2);
        assert_eq!(queued(&player), ["b", "c", "a", "d"]);
        player.move_to(ids[3], 0);
        assert_eq!(queued(&player), ["d", "b", "c", "a"]);
        player.move_to(ids[1], 99);
        assert_eq!(queued(&player), ["d", "c", "a", "b"]);
        player.move_to(12345, 0);
        player.remove(ids[0]);
        assert_eq!(queued(&player), ["d", "c", "b"]);
    }

    #[test]
    fn songs_are_inserted_in_order_where_dropped() {
        let player = player();
        player.enqueue(vec![track("a"), track("b")], "ada", None);
        player.enqueue(vec![track("one"), track("two")], "ada", Some(1));
        assert_eq!(queued(&player), ["a", "one", "two", "b"]);
        player.enqueue(vec![track("first")], "ada", Some(0));
        player.enqueue(vec![track("last")], "ada", Some(99));
        assert_eq!(queued(&player), ["first", "a", "one", "two", "b", "last"]);
    }

    #[test]
    fn queue_is_bounded() {
        let player = player();
        let many = (0..MAX_QUEUE + 3).map(|n| track(&n.to_string())).collect();
        assert_eq!(player.enqueue(many, "ada", None), MAX_QUEUE);
        assert_eq!(player.enqueue(vec![track("x")], "ada", Some(0)), 0);
    }

    #[test]
    fn seeking_restarts_decoding_at_the_position_within_the_song() {
        let player = player();
        let mut song = track("a");
        song.duration = Some(10);
        player.restore(Restored {
            paused: false,
            volume: 50,
            ducking: Ducking::default(),
            current: Some(RestoredCurrent {
                entry: Entry {
                    id: 1,
                    track: song,
                    added_by: "ada".into(),
                },
                position: Duration::ZERO,
                source: None,
                started_at_ms: None,
                played_ms: 0,
            }),
            queue: Vec::new(),
            next_id: 2,
            history: VecDeque::new(),
            next_history_id: 1,
        });
        let generation = player.state().current.as_ref().unwrap().generation;
        player.seek(Duration::from_millis(4_010));
        let state = player.state();
        let playing = state.current.as_ref().unwrap();
        assert_eq!(playing.elapsed(), Duration::from_secs(4));
        assert_eq!(playing.generation, generation + 1);
        drop(state);
        player.seek(Duration::from_secs(60));
        assert_eq!(
            player.state().current.as_ref().unwrap().elapsed(),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn changes_reach_viewers() {
        let changes = Changes::default();
        let mut watching = changes.subscribe();
        let player = Arc::new(Player::new("test".into(), 50, Ducking::default(), changes));
        player.set_volume(70);
        assert!(watching.has_changed().unwrap());
        watching.borrow_and_update();
        let attached = player.subscribe(0, 0, u32::MAX);
        assert!(watching.has_changed().unwrap());
        watching.borrow_and_update();
        drop(attached);
        assert!(
            watching.has_changed().unwrap(),
            "a listener leaving is a change"
        );
    }

    #[test]
    fn bitrate_follows_the_strictest_server() {
        let player = Arc::new(player());
        assert_eq!(player.bitrate(96_000), 96_000);
        let generous = player.subscribe(0, 0, 558_000);
        let strict = player.subscribe(0, 1, 72_000);
        assert_eq!(player.bitrate(96_000), 72_000 - OVERHEAD_BPS);
        player.update_bandwidth(1, 32_000);
        assert_eq!(player.bitrate(96_000), 6_400);
        player.update_bandwidth(1, OVERHEAD_BPS + MIN_BITRATE - 1);
        assert_eq!(player.bitrate(96_000), 0);
        player.update_bandwidth(1, OVERHEAD_BPS + MIN_BITRATE);
        assert_eq!(player.bitrate(96_000), MIN_BITRATE);
        drop(strict);
        assert_eq!(player.bitrate(96_000), 96_000);
        player.update_bandwidth(0, 0);
        assert_eq!(player.bitrate(96_000), 0);
        player.update_bandwidth(0, 558_000);
        assert_eq!(player.bitrate(96_000), 96_000);
        drop(generous);
        assert_eq!(player.listeners(), 0);
    }

    #[test]
    fn bandwidth_never_raises_the_codec_rate_past_the_server_budget() {
        for (bandwidth, expected) in [
            (0, 0),
            (OVERHEAD_BPS, 0),
            (OVERHEAD_BPS + MIN_BITRATE - 1, 0),
            (OVERHEAD_BPS + MIN_BITRATE, MIN_BITRATE),
            (32_000, 6_400),
            (72_000, 46_400),
            (121_600, 96_000),
        ] {
            let rate = bitrate_for_bandwidth(96_000, bandwidth);
            assert_eq!(rate, expected);
            if rate > 0 {
                assert!(rate + OVERHEAD_BPS <= bandwidth);
            }
        }
    }

    #[test]
    fn ducking_lowers_quickly_and_recovers_slowly() {
        let ducking = Ducking::default();
        let normal = target_gain(100, &ducking, false);
        let ducked = target_gain(100, &ducking, true);
        assert_eq!((normal, ducked), (1.0, 0.3));
        assert_eq!(
            target_gain(
                100,
                &Ducking {
                    level: 0,
                    ..ducking
                },
                true
            ),
            0.0
        );
        // Down to within 5 % of the ducked level in five frames (100 ms).
        let mut gain = normal;
        for _ in 0..5 {
            gain = next_gain(gain, ducked);
        }
        assert!(gain - ducked < 0.05 * (normal - ducked), "{gain}");
        // Back up takes longer than 200 ms but arrives.
        for _ in 0..10 {
            gain = next_gain(gain, normal);
        }
        assert!(gain < 0.9, "{gain}");
        for _ in 0..200 {
            gain = next_gain(gain, normal);
        }
        assert_eq!(gain, normal);
    }

    #[test]
    fn a_gain_change_is_ramped_within_the_frame() {
        let mut frame = vec![1.0; FRAME * CHANNELS];
        apply_gain(&mut frame, 1.0, 0.0);
        // Both channels of a frame get the same gain; it falls monotonically.
        assert_eq!(frame[0], frame[1]);
        assert!(
            frame
                .chunks(2)
                .zip(frame.chunks(2).skip(1))
                .all(|(a, b)| b[0] <= a[0])
        );
        assert_eq!(frame[FRAME * CHANNELS - 1], 0.0);
        assert!(frame[0] > 0.99);
    }

    #[tokio::test(start_paused = true)]
    async fn speech_ducks_for_the_hold_time_only() {
        let player = player();
        let ducking = player.state().ducking;
        assert!(!player.ducked(&ducking));
        player.heard_speech();
        assert!(player.ducked(&ducking));
        assert!(!player.ducked(&Ducking {
            enabled: false,
            ..ducking
        }));
        tokio::time::advance(ducking.hold).await;
        assert!(!player.ducked(&ducking));
    }
}
