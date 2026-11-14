// SPDX-License-Identifier: EUPL-1.2
//! The math worker: LaTeX formulas from messages, rendered by Typst in a
//! sandboxed process into coverage masks the interface tints (docs/math.md).
//!
//! Unlike the one-shot content worker it stays up between formulas, since
//! Typst's start-up costs more than a formula. It starts on the first one
//! and the desktop stops it when idle. Every formula has a deadline; a worker
//! that misses it, dies or answers out of turn is killed, and the next
//! formula starts a fresh one.

use serde::{Deserialize, Serialize};

/// One formula: LaTeX math without its delimiters, inline or display, at
/// `size` device pixels per em (text size × scale factor).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MathRequest {
    pub tex: String,
    pub display: bool,
    pub size: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "status",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum MathResponse {
    Rendered(MathImage),
    /// Not rendered, and why, in words for a tooltip.
    Rejected(String),
}

/// A rendered formula: one byte of coverage per pixel, row by row, base64.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MathImage {
    pub width: u32,
    pub height: u32,
    /// The text baseline, in pixels from the top.
    pub baseline: u32,
    pub coverage: String,
}

/// The longest formula sent, in bytes (`nuschl_math::latex::MAX_SOURCE_BYTES`).
pub const MAX_TEX_BYTES: usize = 4096;
/// The largest image accepted back (`nuschl_math::MAX_WIDTH` and so on).
pub const MAX_WIDTH: u32 = 4096;
pub const MAX_HEIGHT: u32 = 1024;
pub const MAX_PIXELS: u32 = 2 * 1024 * 1024;

impl MathImage {
    /// The coverage bytes, if the image is within the limits and its data
    /// matches its size: the worker's reply is not trusted.
    pub fn decode(&self) -> Option<Vec<u8>> {
        use base64::Engine as _;
        let pixels = self.width.checked_mul(self.height)?;
        if self.width == 0
            || self.height == 0
            || self.width > MAX_WIDTH
            || self.height > MAX_HEIGHT
            || pixels > MAX_PIXELS
            || self.baseline > self.height
        {
            return None;
        }
        let coverage = base64::engine::general_purpose::STANDARD
            .decode(&self.coverage)
            .ok()?;
        (coverage.len() == pixels as usize).then_some(coverage)
    }
}

#[cfg(not(target_family = "wasm"))]
pub use supervisor::MathWorker;

#[cfg(not(target_family = "wasm"))]
mod supervisor {
    use std::{
        path::PathBuf,
        time::{Duration, Instant},
    };

    use nuschl_sandbox::{Role, Worker};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        process::ChildStdout,
        time::timeout,
    };

    use super::{MAX_TEX_BYTES, MathImage, MathRequest, MathResponse};
    use crate::{Envelope, MAX_FRAME_BYTES, PROTOCOL_VERSION, host::Error, write_frame};

    /// How long one formula may take, start-up included.
    const DEADLINE: Duration = Duration::from_secs(3);

    struct Process {
        worker: Worker,
        /// Formulas this process answered.
        served: u64,
    }

    /// The math worker, started when a formula needs it.
    pub struct MathWorker {
        executable: PathBuf,
        process: Option<Process>,
        request_id: u64,
        last_used: Instant,
    }

    impl MathWorker {
        pub fn new(executable: PathBuf) -> Self {
            Self {
                executable,
                process: None,
                request_id: 0,
                last_used: Instant::now(),
            }
        }

        pub fn running(&self) -> bool {
            self.process.is_some()
        }

        /// Time since the last formula.
        pub fn idle(&self) -> Duration {
            self.last_used.elapsed()
        }

        /// Render a formula. `Err(Error::Rejected)` is the worker's answer
        /// (a LaTeX or Typst error); other errors mean it failed and was killed.
        pub async fn render(&mut self, request: &MathRequest) -> Result<MathImage, Error> {
            if request.tex.len() > MAX_TEX_BYTES {
                return Err(Error::Rejected("too long".into()));
            }
            self.last_used = Instant::now();
            let reused = self
                .process
                .as_ref()
                .is_some_and(|process| process.served > 0);
            match self.attempt(request).await {
                // A long-lived worker may have died in between (its lifetime
                // CPU limit, say): one fresh attempt. A missed deadline is the
                // formula's fault and is not retried.
                Err(Error::Io(_) | Error::Frame(_) | Error::Json(_) | Error::InvalidResponse)
                    if reused =>
                {
                    self.attempt(request).await
                }
                result => result,
            }
        }

        /// Stop the worker; the next formula starts it again.
        pub async fn stop(&mut self) {
            if let Some(Process { mut worker, .. }) = self.process.take() {
                // Closing its input lets it exit; kill it if it does not.
                drop(worker.input);
                if timeout(Duration::from_secs(1), worker.child.wait())
                    .await
                    .is_err()
                {
                    worker.child.kill().await.ok();
                }
            }
        }

        async fn attempt(&mut self, request: &MathRequest) -> Result<MathImage, Error> {
            if self.process.is_none() {
                self.process = Some(self.start().await?);
            }
            self.request_id += 1;
            let id = self.request_id;
            let mut frame = Vec::new();
            write_frame(&mut frame, &Envelope::new(id, request))?;
            let Some(process) = self.process.as_mut() else {
                return Err(Error::InvalidResponse);
            };
            let exchange = async {
                process.worker.input.write_all(&frame).await?;
                process.worker.input.flush().await?;
                read_reply(&mut process.worker.output).await
            };
            let reply = match timeout(DEADLINE, exchange).await {
                Ok(Ok(reply)) if reply.request_id == id => reply,
                outcome => {
                    self.kill().await;
                    return Err(match outcome {
                        Err(_) => Error::Timeout,
                        Ok(Err(error)) => error,
                        Ok(Ok(_)) => Error::InvalidResponse,
                    });
                }
            };
            process.served += 1;
            match reply.payload {
                MathResponse::Rendered(image) if image.decode().is_some() => Ok(image),
                MathResponse::Rendered(_) => {
                    self.kill().await;
                    Err(Error::InvalidResponse)
                }
                MathResponse::Rejected(reason) => {
                    Err(Error::Rejected(reason.chars().take(200).collect()))
                }
            }
        }

        async fn start(&self) -> Result<Process, Error> {
            let command = nuschl_sandbox::command(&self.executable, Role::Math)?;
            Ok(Process {
                worker: nuschl_sandbox::spawn(command).await?,
                served: 0,
            })
        }

        async fn kill(&mut self) {
            if let Some(mut process) = self.process.take() {
                process.worker.child.kill().await.ok();
            }
        }
    }

    async fn read_reply(output: &mut ChildStdout) -> Result<Envelope<MathResponse>, Error> {
        let size = output.read_u32().await? as usize;
        if size > MAX_FRAME_BYTES {
            return Err(Error::Frame(crate::Error::TooLarge));
        }
        let mut bytes = vec![0; size];
        output.read_exact(&mut bytes).await?;
        let reply: Envelope<MathResponse> = serde_json::from_slice(&bytes)?;
        if reply.version != PROTOCOL_VERSION {
            return Err(Error::InvalidResponse);
        }
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::MathImage;
    use base64::Engine as _;

    #[test]
    fn replies_are_checked_against_their_size_and_the_limits() {
        let image = |width, height, baseline, bytes: usize| MathImage {
            width,
            height,
            baseline,
            coverage: base64::engine::general_purpose::STANDARD.encode(vec![7; bytes]),
        };
        assert_eq!(image(3, 2, 2, 6).decode(), Some(vec![7; 6]));
        assert_eq!(image(3, 2, 2, 5).decode(), None);
        assert_eq!(image(3, 2, 3, 6).decode(), None);
        assert_eq!(image(0, 2, 0, 0).decode(), None);
        assert_eq!(image(5000, 1, 0, 5000).decode(), None);
        assert_eq!(image(4096, 1024, 0, 4096 * 1024).decode(), None);
        let mut garbled = image(1, 1, 0, 1);
        garbled.coverage = "!!".into();
        assert_eq!(garbled.decode(), None);
    }
}
