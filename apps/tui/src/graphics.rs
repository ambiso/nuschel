// SPDX-License-Identifier: EUPL-1.2
//! Terminal pictures from worker pixels. No image codecs or formula evaluator
//! run here. The cache and queue are bounded; layout only requests work.
use base64::Engine as _;
use nuschl_core::{ContentBlock, MessageDocument};
use nuschl_ipc::math::{MathImage, MathRequest, MathWorker};
use ratatui::layout::Size;
use ratatui_image::{
    picker::{Picker, ProtocolType},
    sliced::SlicedProtocol,
};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::mpsc;

const CACHE: usize = 32;
const QUEUE: usize = 4;
const MAX_PIXELS: usize = 512 * 1024;
const MAX_CACHE_BYTES: usize = 16 * 1024 * 1024;
const MAX_ROWS: u16 = 16;
const IDLE: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Source {
    Image {
        width: u32,
        height: u32,
        rgba: String,
    },
    Math {
        tex: String,
        display: bool,
    },
}

impl Source {
    fn bytes(&self) -> usize {
        match self {
            Self::Image { rgba, .. } => rgba.len(),
            Self::Math { tex, .. } => tex.len(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    source: Source,
    columns: u16,
}

enum Entry {
    Pending,
    Ready(Arc<SlicedProtocol>),
    Failed,
}
struct Cached {
    entry: Entry,
    used: u64,
    bytes: usize,
}
struct Job {
    key: Key,
    picker: Picker,
}
struct Reply {
    key: Key,
    image: Option<SlicedProtocol>,
    bytes: usize,
}

#[derive(Default)]
pub struct Graphics {
    picker: Option<Picker>,
    entries: HashMap<Key, Cached>,
    clock: u64,
    jobs: Option<mpsc::Sender<Job>>,
    replies: Option<mpsc::Receiver<Reply>>,
}

impl Graphics {
    /// Called before crossterm starts reading stdin, so capability replies do
    /// not become keystrokes. Unknown/half-block terminals keep source text.
    pub fn configure(&mut self, picker: Picker) {
        if picker.protocol_type() == ProtocolType::Halfblocks {
            return;
        }
        let font = picker.font_size();
        if font.width == 0 || font.height == 0 || font.width > 128 || font.height > 256 {
            return;
        }
        self.picker = Some(picker);
    }

    /// Integrate completed work on the UI thread. No blocked writes or tasks
    /// survive shutdown: dropping both channels ends the worker service.
    pub fn poll(&mut self) {
        while let Some(reply) = self.replies.as_mut().and_then(|rx| rx.try_recv().ok()) {
            if let Some(entry) = self.entries.get_mut(&reply.key) {
                entry.entry = reply
                    .image
                    .map_or(Entry::Failed, |image| Entry::Ready(Arc::new(image)));
                entry.bytes = reply.bytes;
            }
        }
        self.evict();
    }

    fn evict(&mut self) {
        while self.entries.len() >= CACHE
            || self
                .entries
                .values()
                .map(|entry| entry.bytes)
                .sum::<usize>()
                > MAX_CACHE_BYTES
        {
            let oldest = self
                .entries
                .iter()
                .filter(|(_, entry)| !matches!(entry.entry, Entry::Pending))
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone());
            let Some(key) = oldest else {
                break;
            };
            self.entries.remove(&key);
        }
    }

    fn lookup(
        &mut self,
        source: Source,
        columns: u16,
        request: bool,
    ) -> Option<Arc<SlicedProtocol>> {
        let picker = self.picker.as_ref()?.clone();
        let columns = columns.min(120);
        if columns == 0 || source.bytes() > MAX_PIXELS * 4 * 4 / 3 + 8 {
            return None;
        }
        self.clock = self.clock.saturating_add(1);
        let key = Key { source, columns };
        if let Some(cached) = self.entries.get_mut(&key) {
            cached.used = self.clock;
            return match &cached.entry {
                Entry::Ready(image) => Some(image.clone()),
                _ => None,
            };
        }
        if !request {
            return None;
        }
        self.evict();
        if self.entries.len() >= CACHE {
            return None;
        }
        let jobs = self.jobs.get_or_insert_with(|| {
            let (tx, rx) = mpsc::channel(QUEUE);
            let (replies, answers) = mpsc::channel(QUEUE);
            self.replies = Some(answers);
            tokio::spawn(serve(rx, replies));
            tx
        });
        if jobs
            .try_send(Job {
                key: key.clone(),
                picker,
            })
            .is_err()
        {
            return None;
        }
        self.entries.insert(
            key.clone(),
            Cached {
                entry: Entry::Pending,
                used: self.clock,
                bytes: key.source.bytes(),
            },
        );
        None
    }

    /// Formulas remain available as source in the message; their rendered
    /// versions (including inline formulas) follow it, rather than changing
    /// terminal text baselines. At most 16 pictures are requested per message.
    pub fn pictures(
        &mut self,
        document: &MessageDocument,
        columns: u16,
        request: bool,
    ) -> Vec<Arc<SlicedProtocol>> {
        if self.picker.is_none() {
            return Vec::new();
        }
        let mut sources = Vec::new();
        for block in &document.blocks {
            match block {
                ContentBlock::Image {
                    width,
                    height,
                    rgba,
                    ..
                } if valid_size(*width, *height, 4).is_some() => sources.push(Source::Image {
                    width: *width,
                    height: *height,
                    rgba: rgba.clone(),
                }),
                ContentBlock::Math { tex, .. } if tex.len() <= nuschl_ipc::math::MAX_TEX_BYTES => {
                    sources.push(Source::Math {
                        tex: tex.clone(),
                        display: true,
                    })
                }
                ContentBlock::Text { spans, .. } => {
                    for span in spans.iter().take(32) {
                        if let Some(tex) = &span.math
                            && tex.len() <= nuschl_ipc::math::MAX_TEX_BYTES
                        {
                            sources.push(Source::Math {
                                tex: tex.clone(),
                                display: false,
                            });
                        }
                        if sources.len() >= 16 {
                            break;
                        }
                    }
                }
                _ => {}
            }
            if sources.len() >= 16 {
                break;
            }
        }
        sources
            .into_iter()
            .take(16)
            .filter_map(|source| self.lookup(source, columns, request))
            .collect()
    }
}

fn valid_size(width: u32, height: u32, channels: usize) -> Option<usize> {
    let pixels = usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?;
    (width > 0 && height > 0 && width <= 4096 && height <= 2048 && pixels <= MAX_PIXELS)
        .then_some(pixels.checked_mul(channels)?)
}

fn rgba(width: u32, height: u32, encoded: &str) -> Option<image::RgbaImage> {
    let length = valid_size(width, height, 4)?;
    if encoded.len() > length.div_ceil(3) * 4 {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    if bytes.len() != length {
        return None;
    }
    image::RgbaImage::from_raw(width, height, bytes)
}

fn formula(image: MathImage, background: [u8; 3]) -> Option<image::RgbaImage> {
    valid_size(image.width, image.height, 4)?;
    let coverage = image.decode()?;
    let foreground = if background.iter().map(|&c| u16::from(c)).sum::<u16>() > 384 {
        24_u16
    } else {
        230_u16
    };
    // Sixel has binary alpha. Composite the antialiased mask against the
    // queried terminal background before encoding, keeping every grey edge.
    let pixels = coverage
        .into_iter()
        .flat_map(|alpha| {
            let alpha = u16::from(alpha);
            let [r, g, b] = background.map(|channel| {
                ((foreground * alpha + u16::from(channel) * (255 - alpha)) / 255) as u8
            });
            [r, g, b, 255]
        })
        .collect();
    image::RgbaImage::from_raw(image.width, image.height, pixels)
}

async fn serve(mut jobs: mpsc::Receiver<Job>, replies: mpsc::Sender<Reply>) {
    let executable = nuschl_session::workers::sibling("nuschl-math-worker");
    let mut worker = executable.ok().map(MathWorker::new);
    loop {
        let job = match tokio::time::timeout(IDLE, jobs.recv()).await {
            Ok(Some(job)) => job,
            Ok(None) => break,
            Err(_) => {
                if let Some(worker) = &mut worker {
                    worker.stop().await;
                }
                continue;
            }
        };
        let pixels = match &job.key.source {
            Source::Image {
                width,
                height,
                rgba: encoded,
            } => rgba(*width, *height, encoded),
            Source::Math { tex, display } => match &mut worker {
                Some(worker) => worker
                    .render(&MathRequest {
                        tex: tex.clone(),
                        display: *display,
                        size: f32::from(job.picker.font_size().height).clamp(4., 128.),
                    })
                    .await
                    .ok()
                    .and_then(|image| {
                        let background = job
                            .picker
                            .capabilities()
                            .iter()
                            .find_map(|capability| match capability {
                                ratatui_image::picker::Capability::Background(r, g, b) => {
                                    Some([*r, *g, *b])
                                }
                                _ => None,
                            })
                            .unwrap_or([0, 0, 0]);
                        formula(image, background)
                    }),
                None => None,
            },
        };
        let key = job.key;
        let input_bytes = key.source.bytes();
        let columns = key.columns;
        let (image, bytes) = if let Some(pixels) = pixels {
            let bytes = pixels
                .as_raw()
                .len()
                .saturating_mul(6)
                .saturating_add(input_bytes);
            let image = tokio::task::spawn_blocking(move || {
                SlicedProtocol::new(
                    &job.picker,
                    image::DynamicImage::ImageRgba8(pixels),
                    Some(Size::new(columns, MAX_ROWS)),
                )
                .ok()
            })
            .await
            .ok()
            .flatten();
            (image, bytes)
        } else {
            (None, input_bytes)
        };
        if replies.send(Reply { key, image, bytes }).await.is_err() {
            break;
        }
    }
    if let Some(worker) = &mut worker {
        worker.stop().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raw_pixels_require_exact_bounded_dimensions() {
        let bytes = base64::engine::general_purpose::STANDARD.encode([0; 8]);
        assert!(rgba(2, 1, &bytes).is_some());
        assert!(rgba(1, 1, &bytes).is_none());
        assert!(rgba(0, 1, "").is_none());
        assert!(rgba(u32::MAX, u32::MAX, &bytes).is_none());
        assert!(rgba(4096, 2048, "").is_none());
        assert!(rgba(1, 1, "\x1b]52;evil").is_none());
    }
}
