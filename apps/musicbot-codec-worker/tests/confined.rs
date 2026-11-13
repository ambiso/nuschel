// SPDX-License-Identifier: EUPL-1.2
//! The bot's supervisor (`nuschl_ipc::musicbot`) against this crate's real
//! worker executable, confined by the production Linux launcher (bubblewrap
//! and prlimit on PATH). Faults are real: throttled, endless or oversized
//! input, and a worker stopped with SIGSTOP. The tests run one at a time, and
//! each checks that no confined worker outlives its supervisor.
#![cfg(target_os = "linux")]
use nuschl_ipc::musicbot::{self, DecodeLimits, Ducker, Media, RESPONSE_TIMEOUT};
use std::{
    io::{self, Cursor, Read},
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Notify, mpsc};

const WORKER: &str = env!("CARGO_BIN_EXE_nuschl-musicbot-codec-worker");
const MIB: usize = 1024 * 1024;
static ONE_AT_A_TIME: Mutex<()> = Mutex::const_new(());

/// Host PIDs of confined workers in `role`.
fn workers(role: &str) -> Vec<u32> {
    let expected = [
        b"/worker".as_slice(),
        b"--codec-worker",
        role.as_bytes(),
        b"",
    ];
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let pid = entry.file_name().to_str()?.parse().ok()?;
            let command = std::fs::read(entry.path().join("cmdline")).ok()?;
            command.split(|byte| *byte == 0).eq(expected).then_some(pid)
        })
        .collect()
}

/// The supervisor reaps its launcher; the confined worker follows it.
async fn none_left(role: &str) {
    let started = Instant::now();
    while !workers(role).is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "a confined {role} worker outlived its supervisor"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn limits() -> DecodeLimits {
    DecodeLimits {
        inactivity: Duration::from_secs(5),
        before_audio: Duration::from_secs(60),
        ..DecodeLimits::DEFAULT
    }
}

/// Pauses before every read, as a slow library would.
struct Throttled<R> {
    inner: R,
    pause: Duration,
}

impl<R: Read> Read for Throttled<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        std::thread::sleep(self.pause);
        self.inner.read(bytes)
    }
}

/// A 48 kHz stereo 16-bit WAV file of `frames` 20 ms frames of a sine.
fn wav(frames: usize) -> Vec<u8> {
    let samples = frames * musicbot::FRAME;
    let mut data = Vec::with_capacity(samples * 4);
    for n in 0..samples {
        let left = (n as f32 / musicbot::RATE as f32 * 440.0 * std::f32::consts::TAU).sin() * 0.5;
        let sample = ((left * f32::from(i16::MAX)) as i16).to_le_bytes();
        data.extend_from_slice(&sample);
        data.extend_from_slice(&sample);
    }
    let mut file = Vec::new();
    file.extend_from_slice(b"RIFF");
    file.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    file.extend_from_slice(b"WAVEfmt ");
    file.extend_from_slice(&16u32.to_le_bytes());
    file.extend_from_slice(&1u16.to_le_bytes());
    file.extend_from_slice(&2u16.to_le_bytes());
    file.extend_from_slice(&musicbot::RATE.to_le_bytes());
    file.extend_from_slice(&(musicbot::RATE * 4).to_le_bytes());
    file.extend_from_slice(&4u16.to_le_bytes());
    file.extend_from_slice(&16u16.to_le_bytes());
    file.extend_from_slice(b"data");
    file.extend_from_slice(&(data.len() as u32).to_le_bytes());
    file.extend_from_slice(&data);
    file
}

/// One decode through the supervisor: its result and every frame it passed on.
async fn decode(
    reader: impl Read + Send + 'static,
    extension: &str,
    skip_frames: u64,
    limits: DecodeLimits,
) -> (Result<(), String>, Vec<Vec<f32>>) {
    let (sender, mut receiver) = mpsc::channel::<musicbot::Frame>(64);
    let collecting = tokio::spawn(async move {
        let mut frames = Vec::new();
        while let Some(frame) = receiver.recv().await {
            frames.push(frame.expect("the supervisor returns errors, never sends them"));
        }
        frames
    });
    let media = Media {
        reader,
        mime: None,
        extension: Some(extension.into()),
    };
    let result = musicbot::decode(
        Path::new(WORKER),
        media,
        skip_frames,
        &sender,
        Arc::new(AtomicBool::new(false)),
        Arc::new(Notify::new()),
        &limits,
    )
    .await;
    drop(sender);
    (result, collecting.await.unwrap())
}

fn bits(frame: &[f32]) -> Vec<u32> {
    frame.iter().map(|sample| sample.to_bits()).collect()
}

#[tokio::test]
async fn progress_keeps_a_slow_seek_alive_and_it_plays_the_unskipped_frames() {
    let _one = ONE_AT_A_TIME.lock().await;
    let song = wav(1500);
    // Unthrottled, the input ends while replies are still being read: that
    // must never drop a partly read reply (it once desynchronised the stream).
    let (result, whole) = decode(Cursor::new(song.clone()), "wav", 0, limits()).await;
    result.unwrap();
    assert_eq!(whole.len(), 1500);
    // About 1.3 MB/s: each 250-frame step (960 kB) takes ~0.75 s, the whole
    // 1000-frame skip ~3 s, twice the 1.5 s allowed without any reply.
    let slow = Throttled {
        inner: Cursor::new(song),
        pause: Duration::from_millis(25),
    };
    let started = Instant::now();
    let limits = DecodeLimits {
        inactivity: Duration::from_millis(1500),
        ..limits()
    };
    let (result, sought) = decode(slow, "wav", 1000, limits).await;
    result.unwrap();
    assert!(started.elapsed() > Duration::from_millis(1500));
    assert_eq!(sought.len(), 500);
    assert!(
        whole[1000..]
            .iter()
            .zip(&sought)
            .all(|(a, b)| bits(a) == bits(b)),
        "the seek did not continue exactly where an unskipped decode is"
    );
    none_left("decode").await;
}

#[tokio::test]
async fn an_mp4_read_whole_reports_progress_until_the_workers_own_verdict() {
    let _one = ONE_AT_A_TIME.lock().await;
    // ~3 MB/s of junk named .m4a: 4 MiB take longer than the 1 s allowed
    // without a reply, so only accepted input progress gets it to the end.
    let junk = Throttled {
        inner: Cursor::new(vec![0; 4 * MIB + 1]),
        pause: Duration::from_millis(10),
    };
    let limits = DecodeLimits {
        inactivity: Duration::from_secs(1),
        ..limits()
    };
    let started = Instant::now();
    let (result, frames) = decode(junk, "m4a", 0, limits).await;
    assert_eq!(
        result.unwrap_err(),
        "the audio format is invalid or unsupported"
    );
    assert!(started.elapsed() > Duration::from_secs(1));
    assert!(frames.is_empty());
    none_left("decode").await;
}

#[tokio::test]
async fn input_over_the_limits_fails_explicitly_instead_of_being_cut() {
    let _one = ONE_AT_A_TIME.lock().await;
    let limits = DecodeLimits {
        mp4_bytes: 2 * MIB as u64,
        seek_input_bytes: MIB as u64,
        ..limits()
    };
    let (result, frames) = decode(Cursor::new(vec![0; 3 * MIB]), "m4a", 0, limits).await;
    assert_eq!(result.unwrap_err(), "the file is larger than 2 MiB");
    assert!(frames.is_empty());
    none_left("decode").await;
    // A seek that would read 3.8 MB of song first.
    let (result, frames) = decode(Cursor::new(wav(1500)), "wav", 1000, limits).await;
    assert_eq!(
        result.unwrap_err(),
        "playback would read more than 1 MiB before it starts"
    );
    assert!(frames.is_empty());
    none_left("decode").await;
}

#[tokio::test]
async fn progress_never_extends_the_time_before_audio() {
    let _one = ONE_AT_A_TIME.lock().await;
    // Endless junk named .m4a keeps input progress coming every ~0.3 s.
    let endless = Throttled {
        inner: io::repeat(0),
        pause: Duration::from_millis(10),
    };
    let limits = DecodeLimits {
        inactivity: Duration::from_secs(1),
        before_audio: Duration::from_secs(2),
        ..limits()
    };
    let started = Instant::now();
    let (result, frames) = decode(endless, "m4a", 0, limits).await;
    let elapsed = started.elapsed();
    assert_eq!(
        result.unwrap_err(),
        "music decoder did not start playing within 2 seconds"
    );
    assert!(elapsed >= Duration::from_secs(2) && elapsed < Duration::from_secs(5));
    assert!(frames.is_empty());
    none_left("decode").await;
}

#[tokio::test]
async fn a_stopped_worker_withholding_progress_fails_and_is_reaped() {
    let _one = ONE_AT_A_TIME.lock().await;
    let endless = Throttled {
        inner: io::repeat(0),
        pause: Duration::from_millis(10),
    };
    let limits = DecodeLimits {
        inactivity: Duration::from_secs(1),
        ..limits()
    };
    let decoding = tokio::spawn(decode(endless, "m4a", 0, limits));
    let started = Instant::now();
    let pid = loop {
        if let [pid] = workers("decode")[..] {
            break pid;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "no worker started"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let stopped = std::process::Command::new("kill")
        .args(["-STOP", &pid.to_string()])
        .status()
        .unwrap();
    assert!(stopped.success());
    let stopped_at = Instant::now();
    let (result, frames) = decoding.await.unwrap();
    assert_eq!(
        result.unwrap_err(),
        "music decoder made no progress within 1 seconds"
    );
    assert!(stopped_at.elapsed() < Duration::from_secs(4));
    assert!(frames.is_empty());
    none_left("decode").await;
}

#[tokio::test]
async fn a_seek_beyond_the_limit_starts_no_worker() {
    let _one = ONE_AT_A_TIME.lock().await;
    let (result, frames) = decode(
        Cursor::new(wav(3)),
        "wav",
        DecodeLimits::DEFAULT.seek_frames + 1,
        limits(),
    )
    .await;
    assert_eq!(
        result.unwrap_err(),
        "playback cannot start more than 180 minutes into a song"
    );
    assert!(frames.is_empty());
    assert!(workers("decode").is_empty());
}

fn signal(name: &str, pid: u32) {
    let sent = std::process::Command::new("kill")
        .args([format!("-{name}"), pid.to_string()])
        .status()
        .unwrap();
    assert!(sent.success(), "kill -{name} {pid}");
}

/// The one confined duck worker that exists now.
fn duck_worker() -> u32 {
    let [pid] = workers("duck")[..] else {
        panic!("expected exactly one duck worker");
    };
    pid
}

/// Stops the worker, then kills it while the next request is in flight.
fn die_mid_request(pid: u32) -> tokio::task::JoinHandle<()> {
    signal("STOP", pid);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        signal("KILL", pid);
    })
}

#[tokio::test]
async fn detector_failures_cost_packets_never_the_connection() {
    let _one = ONE_AT_A_TIME.lock().await;
    let silence = nuschl_core::audio::SILENT_OPUS;
    let threshold = musicbot::speech_threshold(50);
    let mut ducker = Ducker::new(PathBuf::from(WORKER));
    assert!(!ducker.hear(1, &silence, threshold).await.unwrap());
    assert!(ducker.running());
    let first = duck_worker();

    // A worker that dies while answering sender 7: one recovered failure,
    // its exit confirmed, no error for the connection.
    let killing = die_mid_request(first);
    let started = Instant::now();
    assert!(!ducker.hear(7, &silence, threshold).await.unwrap());
    killing.await.unwrap();
    assert!(
        started.elapsed() < RESPONSE_TIMEOUT,
        "an exit is not a timeout"
    );
    assert!(!ducker.running() && !ducker.suspended());
    none_left("duck").await;

    // The poison sender is never retried: nothing starts.
    for _ in 0..5 {
        assert!(!ducker.hear(7, &silence, threshold).await.unwrap());
        assert!(!ducker.running());
    }
    assert!(workers("duck").is_empty());

    // Other talkers get a fresh worker.
    assert!(!ducker.hear(8, &silence, threshold).await.unwrap());
    let second = duck_worker();
    assert_ne!(second, first);

    // A stalled worker: the request deadline, then a separately awaited reap.
    signal("STOP", second);
    let started = Instant::now();
    assert!(!ducker.hear(9, &silence, threshold).await.unwrap());
    let elapsed = started.elapsed();
    assert!(elapsed >= RESPONSE_TIMEOUT && elapsed < RESPONSE_TIMEOUT * 3);
    assert!(!ducker.running() && !ducker.suspended());
    none_left("duck").await;

    // The third recovered failure suspends ducking for this session.
    assert!(!ducker.hear(12, &silence, threshold).await.unwrap());
    let killing = die_mid_request(duck_worker());
    assert!(!ducker.hear(10, &silence, threshold).await.unwrap());
    killing.await.unwrap();
    assert!(ducker.suspended() && ducker.notice().is_some());
    none_left("duck").await;
    for sender in [11, 12, 13] {
        assert!(!ducker.hear(sender, &silence, threshold).await.unwrap());
        ducker.forget(sender).await.unwrap();
    }
    assert!(!ducker.running() && workers("duck").is_empty());

    // Only a genuinely new session resets failures and the quarantine.
    ducker.new_session().await.unwrap();
    assert!(!ducker.suspended() && ducker.notice().is_none());
    assert!(!ducker.hear(7, &silence, threshold).await.unwrap());
    assert!(ducker.running());
    ducker.disable().await.unwrap();
    none_left("duck").await;
}

#[tokio::test]
async fn disabling_retires_the_detector_once_and_later_packets_send_nothing() {
    let _one = ONE_AT_A_TIME.lock().await;
    let silence = nuschl_core::audio::SILENT_OPUS;
    let mut ducker = Ducker::new(PathBuf::from(WORKER));
    assert!(
        !ducker
            .hear(1, &silence, musicbot::speech_threshold(50))
            .await
            .unwrap()
    );
    duck_worker();
    ducker.disable().await.unwrap();
    assert!(!ducker.running());
    none_left("duck").await;
    // Ducking stays off: each later packet only asks to disable again.
    for _ in 0..50 {
        ducker.disable().await.unwrap();
    }
    assert!(!ducker.running() && workers("duck").is_empty());
}
