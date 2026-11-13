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
    sync::{Notify, mpsc, oneshot},
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
    worker: JoinHandle<Result<(), String>>,
    terminal_receiver: oneshot::Receiver<Result<(), String>>,
    terminal: Option<Result<(), String>>,
    final_result: Option<Result<(), String>>,
    retiring: bool,
    supervisor_failed: bool,
    reported: bool,
}
impl Decoding {
    /// Cancellation completes without releasing this slot. The same owner
    /// retains the child and blocking HTTP read until poll_outcome confirms exit.
    pub fn request_cancel(&mut self) {
        self.retiring = true;
        self.cancelled.store(true, Ordering::Relaxed);
        self.wake.notify_one();
        self.frames.close();
        while self.frames.try_recv().is_ok() {}
    }

    pub fn retiring(&self) -> bool {
        self.retiring
    }

    /// Only a normal supervisor return confirms both reaping and read completion.
    /// A panic is a lost owner, not permission to start another child.
    pub fn retired(&self) -> bool {
        self.final_result.is_some()
    }

    /// Poll without waiting on OS teardown. Terminal errors follow buffered PCM;
    /// a provisional Done cannot become success before the final network result.
    pub async fn poll_outcome(&mut self, held_frame: bool) -> Option<Result<(), String>> {
        if self.terminal.is_none() {
            match self.terminal_receiver.try_recv() {
                Ok(result) => self.terminal = Some(result),
                Err(oneshot::error::TryRecvError::Empty) => {}
                Err(oneshot::error::TryRecvError::Closed) => {
                    self.terminal =
                        Some(Err("music decoder supervisor stopped unexpectedly".into()));
                }
            }
        }
        if self.final_result.is_none() && !self.supervisor_failed && self.worker.is_finished() {
            match (&mut self.worker).await {
                Ok(result) => self.final_result = Some(result),
                Err(error) => {
                    tracing::error!("decoder supervisor failed: {error}");
                    self.supervisor_failed = true;
                    self.terminal =
                        Some(Err("music decoder supervisor stopped unexpectedly".into()));
                }
            }
        }
        if self.reported || held_frame || !self.frames.is_empty() {
            return None;
        }
        let outcome = self.final_result.clone().or_else(|| {
            self.terminal
                .as_ref()
                .filter(|result| result.is_err())
                .cloned()
        });
        if outcome.is_some() {
            self.reported = true;
        }
        outcome
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
    let (terminal_sender, terminal_receiver) = oneshot::channel();
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
        match stream {
            Ok(Ok(stream)) if !flag.load(Ordering::Relaxed) => {
                let media = Media {
                    reader: stream.reader,
                    mime: stream.mime,
                    extension: stream.extension,
                };
                codec::decode_notifying(
                    &worker,
                    media,
                    start.frames,
                    &sender,
                    flag.clone(),
                    signal,
                    codec::DecodeCompletion {
                        limits: codec::DecodeLimits::DEFAULT,
                        terminal: Some(terminal_sender),
                    },
                )
                .await
            }
            other => {
                let result = match other {
                    Ok(Ok(_)) => Ok(()),
                    Ok(Err(error)) => Err(error),
                    Err(_) => Err("music library download task failed".into()),
                };
                let _ = terminal_sender.send(result.clone());
                result
            }
        }
    });
    Decoding {
        entry: start.entry,
        generation: start.generation,
        frames,
        cancelled,
        wake,
        worker: supervisor,
        terminal_receiver,
        terminal: None,
        final_result: None,
        retiring: false,
        supervisor_failed: false,
        reported: false,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) struct Controlled {
        pub decoding: Decoding,
        pub sender: mpsc::Sender<Frame>,
        pub terminal: oneshot::Sender<Result<(), String>>,
        pub finish: oneshot::Sender<Result<(), String>>,
    }

    pub(crate) fn controlled() -> Controlled {
        let (sender, frames) = mpsc::channel(AHEAD);
        let retained_sender = sender.clone();
        let (terminal_sender, terminal_receiver) = oneshot::channel();
        let (finish, retirement) = oneshot::channel();
        let worker = tokio::spawn(async move {
            let _frames = retained_sender;
            retirement.await.unwrap()
        });
        Controlled {
            decoding: Decoding {
                entry: 7,
                generation: 3,
                frames,
                cancelled: Arc::new(AtomicBool::new(false)),
                wake: Arc::new(Notify::new()),
                worker,
                terminal_receiver,
                terminal: None,
                final_result: None,
                retiring: false,
                supervisor_failed: false,
                reported: false,
            },
            sender,
            terminal: terminal_sender,
            finish,
        }
    }

    #[tokio::test]
    async fn terminal_error_does_not_need_pcm_capacity_or_release_the_slot() {
        let Controlled {
            mut decoding,
            sender,
            terminal,
            finish,
        } = controlled();
        for index in 0..AHEAD {
            sender.try_send(Ok(vec![index as f32])).unwrap();
        }
        terminal.send(Err("decoder timed out".into())).unwrap();
        assert_eq!(decoding.poll_outcome(false).await, None);
        assert_eq!(decoding.terminal, Some(Err("decoder timed out".into())));
        assert!(!decoding.retired());
        for index in 0..AHEAD {
            assert_eq!(decoding.frames.try_recv().unwrap().unwrap(), [index as f32]);
        }
        assert_eq!(
            decoding.poll_outcome(false).await,
            Some(Err("decoder timed out".into()))
        );
        decoding.request_cancel();
        for _ in 0..20 {
            assert_eq!(decoding.poll_outcome(false).await, None);
            assert!(decoding.retiring() && !decoding.retired());
            assert_eq!((decoding.entry, decoding.generation), (7, 3));
        }
        finish.send(Ok(())).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !decoding.retired() {
                assert_eq!(decoding.poll_outcome(false).await, None);
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn provisional_done_cannot_hide_a_late_network_failure() {
        let Controlled {
            mut decoding,
            sender: _sender,
            terminal,
            finish,
        } = controlled();
        terminal.send(Ok(())).unwrap();
        assert_eq!(decoding.poll_outcome(false).await, None);
        assert!(!decoding.retired());
        finish
            .send(Err("network read failed after decoder Done".into()))
            .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Some(result) = decoding.poll_outcome(false).await {
                    break result;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(result, Err("network read failed after decoder Done".into()));
        assert!(decoding.retired());
    }

    #[tokio::test]
    async fn panicked_supervisor_does_not_confirm_retirement() {
        let Controlled {
            mut decoding,
            sender: _sender,
            terminal,
            finish,
        } = controlled();
        terminal.send(Ok(())).unwrap();
        // The controlled supervisor panics when its completion input disappears.
        drop(finish);
        while !decoding.worker.is_finished() {
            tokio::task::yield_now().await;
        }
        assert!(decoding.poll_outcome(false).await.unwrap().is_err());
        decoding.request_cancel();
        assert!(!decoding.retired());
        assert!(decoding.supervisor_failed);
        assert_eq!(decoding.poll_outcome(false).await, None);
    }
}
