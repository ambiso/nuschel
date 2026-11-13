// SPDX-License-Identifier: EUPL-1.2
//! Credential-bearing network pump. Format parsing, DSP and PCM generation run
//! only in the confined decode child (nuschl-musicbot-codec-worker, through
//! `nuschl_ipc::musicbot`); this module transports opaque bytes.
use crate::subsonic::{Library, Track};
use nuschl_ipc::musicbot::{self as codec, Media};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    sync::{Notify, mpsc},
    task::JoinHandle,
};

pub use codec::Frame;
pub const CHANNELS: usize = codec::CHANNELS;
pub const FRAME: usize = codec::FRAME;
// Five seconds, not a whole-track accumulation. Plus two 32 KiB input chunks,
// one bounded protocol reply, and the child's bounded demux/resampler storage.
const AHEAD: usize = 250;

pub struct Decoding {
    pub entry: u64,
    pub generation: u64,
    pub frames: mpsc::Receiver<Frame>,
    cancelled: Arc<AtomicBool>,
    wake: Arc<Notify>,
    worker: Option<JoinHandle<()>>,
}
impl Decoding {
    /// Replacement waits for actual child exit AND the pending HTTP read.
    pub async fn cancel(mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.wake.notify_one();
        self.frames.close();
        if let Some(worker) = self.worker.take()
            && let Err(error) = worker.await
        {
            tracing::error!("decoder supervisor failed: {error}");
        }
    }
}
impl Drop for Decoding {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.wake.notify_one();
        self.frames.close();
    }
}
pub struct Start {
    pub entry: u64,
    pub generation: u64,
    pub frames: u64,
}

pub fn start(
    libraries: Arc<Vec<Library>>,
    worker: PathBuf,
    track: Track,
    start: Start,
) -> Decoding {
    let (sender, frames) = mpsc::channel(AHEAD);
    let cancelled = Arc::new(AtomicBool::new(false));
    let wake = Arc::new(Notify::new());
    let flag = cancelled.clone();
    let signal = wake.clone();
    let supervisor = tokio::spawn(async move {
        // ureq has a 30-second per-read/metadata deadline. Even after cancellation
        // keep ownership of this actual operation until its thread returns.
        let stream = tokio::task::spawn_blocking(move || {
            libraries
                .get(track.library)
                .ok_or_else(|| "the song's library is no longer configured".to_owned())?
                .stream(&track)
                .map_err(|error| error.public_message())
        })
        .await;
        let result = match stream {
            Ok(Ok(stream)) if !flag.load(Ordering::Relaxed) => {
                let media = Media {
                    reader: stream.reader,
                    mime: stream.mime,
                    extension: stream.extension,
                };
                codec::decode(
                    &worker,
                    media,
                    start.frames,
                    &sender,
                    flag.clone(),
                    signal,
                    &codec::DecodeLimits::DEFAULT,
                )
                .await
            }
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => Err(error),
            Err(_) => Err("music library download task failed".into()),
        };
        if !flag.load(Ordering::Relaxed)
            && let Err(error) = result
        {
            // Ordered after all admitted audio, never overtaking buffered PCM.
            let _ = sender.send(Err(error)).await;
        }
    });
    Decoding {
        entry: start.entry,
        generation: start.generation,
        frames,
        cancelled,
        wake,
        worker: Some(supervisor),
    }
}
