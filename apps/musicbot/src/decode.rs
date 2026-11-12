// SPDX-License-Identifier: EUPL-1.2
//! Credential-bearing network pump. Format parsing, DSP and PCM generation run
//! only in the confined decode child; this module transports opaque bytes.
use crate::{
    codec::{self, Request, Response, Role},
    codec_host,
    subsonic::{Library, Track},
};
use std::{
    io::Read,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    io::AsyncWriteExt,
    sync::{Notify, mpsc},
    task::JoinHandle,
};

pub const RATE: u32 = codec::RATE;
pub const CHANNELS: usize = codec::CHANNELS;
pub const FRAME: usize = codec::FRAME;
// Five seconds, not a whole-track accumulation. Plus two 32 KiB input chunks,
// one bounded protocol reply, and the child's bounded demux/resampler storage.
const AHEAD: usize = 250;
pub type Frame = Result<Vec<f32>, String>;

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

pub fn start(libraries: Arc<Vec<Library>>, track: Track, start: Start) -> Decoding {
    let (sender, frames) = mpsc::channel(AHEAD);
    let cancelled = Arc::new(AtomicBool::new(false));
    let wake = Arc::new(Notify::new());
    let flag = cancelled.clone();
    let signal = wake.clone();
    let worker = tokio::spawn(async move {
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
                transport(stream, start.frames, &sender, flag.clone(), signal).await
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
        worker: Some(worker),
    }
}

fn pump(
    mut input: impl Read,
    chunks: mpsc::Sender<Vec<u8>>,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    let mut bytes = [0; 32 * 1024];
    while !cancelled.load(Ordering::Relaxed) {
        let count = match input.read(&mut bytes) {
            Ok(0) => return Ok(()),
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err("the music library audio download failed".into()),
        };
        if cancelled.load(Ordering::Relaxed)
            || chunks.blocking_send(bytes[..count].to_vec()).is_err()
        {
            return Ok(());
        }
    }
    Ok(())
}

async fn transport(
    stream: crate::subsonic::Stream,
    skip_frames: u64,
    sender: &mpsc::Sender<Frame>,
    cancelled: Arc<AtomicBool>,
    wake: Arc<Notify>,
) -> Result<(), String> {
    let mut process = codec_host::spawn(Role::Decode).await?;
    let mut input = process.input.take().ok_or("decoder input missing")?;
    let mut output = process.output.take().ok_or("decoder output missing")?;
    let request = Request::Decode {
        mime: stream.mime,
        extension: stream.extension,
        skip_frames,
    };
    let (chunks, mut incoming) = mpsc::channel::<Vec<u8>>(2);
    let pump_flag = cancelled.clone();
    let pumping = tokio::task::spawn_blocking(move || pump(stream.reader, chunks, &pump_flag));
    let mut writing = tokio::spawn(async move {
        codec_host::write_request(&mut input, 1, &request).await?;
        while let Some(bytes) = incoming.recv().await {
            input
                .write_all(&bytes)
                .await
                .map_err(|_| "decoder stream write failed")?;
        }
        input
            .shutdown()
            .await
            .map_err(|_| "decoder stream close failed")
    });
    let result = loop {
        if cancelled.load(Ordering::Relaxed) {
            break Ok(());
        }
        let response = tokio::select! {
            _ = wake.notified() => { if cancelled.load(Ordering::Relaxed) { break Ok(()); } else { continue; } }
            _ = sender.closed() => { break Ok(()); }
            response = tokio::time::timeout(codec_host::DECODE_TIMEOUT, codec_host::read_response(&mut output, 1)) => response,
        };
        match response {
            Ok(Ok(Response::Pcm { samples })) if codec::valid_pcm(&samples) => {
                // Consumer backpressure does not count as a decoder stall.
                tokio::select! {
                    _ = wake.notified() => { if cancelled.load(Ordering::Relaxed) { break Ok(()); } }
                    result = sender.send(Ok(samples)) => if result.is_err() { break Ok(()); },
                }
            }
            Ok(Ok(Response::Done)) => break Ok(()),
            Ok(Ok(Response::Error { message })) => break Err(message),
            Ok(Err(error)) => break Err(error),
            Err(_) => break Err("music decoder did not produce audio within 35 seconds".into()),
            _ => break Err("music decoder returned an invalid response".into()),
        }
    };
    // Closing the bounded chunk receiver releases a blocked producer; a read
    // already submitted to ureq must still finish before this slot is reusable.
    writing.abort();
    let _ = (&mut writing).await;
    loop {
        match process.retire().await {
            Ok(()) => break,
            Err(_) => {
                // Never release the actual process permit on a failed kill/reap.
                tracing::error!("cannot confirm music decoder exit; retaining its slot");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
    let downloaded = pumping
        .await
        .map_err(|_| "music download task failed".to_owned())?;
    if cancelled.load(Ordering::Relaxed) {
        Ok(())
    } else {
        result.and(downloaded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn opaque_pump_is_bounded_and_cancellation_stops_before_read() {
        let (sender, mut receiver) = mpsc::channel(2);
        let cancelled = AtomicBool::new(true);
        struct NoRead;
        impl Read for NoRead {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                panic!("read after cancellation")
            }
        }
        pump(NoRead, sender, &cancelled).unwrap();
        assert!(receiver.try_recv().is_err());
        let (sender, mut receiver) = mpsc::channel(2);
        pump(
            Cursor::new(vec![42; 40_000]),
            sender,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(receiver.try_recv().unwrap().len(), 32 * 1024);
        assert_eq!(receiver.try_recv().unwrap().len(), 40_000 - 32 * 1024);
        assert!(receiver.try_recv().is_err());
    }
}
