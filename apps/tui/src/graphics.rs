// SPDX-License-Identifier: EUPL-1.2
//! Terminal pictures from worker pixels. No image codecs or formula evaluator
//! run here. The cache and queue are bounded; layout only requests work.
use base64::Engine as _;
use nuschl_core::{ContentBlock, MessageDocument};
use nuschl_ipc::math::{MathImage, MathRequest, MathWorker};
use ratatui::layout::Size;
use ratatui_image::{
    picker::{Picker, ProtocolType},
    protocol::Protocol,
    sliced::SlicedProtocol,
};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::mpsc;

const CACHE: usize = 32;
const QUEUE: usize = 4;
const MAX_PIXELS: usize = 512 * 1024;
const MAX_CACHE_BYTES: usize = 16 * 1024 * 1024;
const MAX_ROWS: u16 = 16;
const MAX_ENCODED: usize = 8 * 1024 * 1024;
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

enum SourceRef<'a> {
    Image {
        width: u32,
        height: u32,
        rgba: &'a str,
    },
    Math {
        tex: &'a str,
        display: bool,
    },
}

impl SourceRef<'_> {
    fn matches(&self, source: &Source) -> bool {
        match (self, source) {
            (
                Self::Image {
                    width,
                    height,
                    rgba,
                },
                Source::Image {
                    width: w,
                    height: h,
                    rgba: data,
                },
            ) => width == w && height == h && *rgba == data,
            (
                Self::Math { tex, display },
                Source::Math {
                    tex: text,
                    display: d,
                },
            ) => *tex == text && display == d,
            _ => false,
        }
    }
    fn owned(self) -> Source {
        match self {
            Self::Image {
                width,
                height,
                rgba,
            } => Source::Image {
                width,
                height,
                rgba: rgba.into(),
            },
            Self::Math { tex, display } => Source::Math {
                tex: tex.into(),
                display,
            },
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
    task: Option<tokio::task::JoinHandle<()>>,
    deleted: Vec<u32>,
}

impl Graphics {
    /// Called before crossterm starts reading stdin, so capability replies do
    /// not become keystrokes. Half-blocks use one raster column per terminal
    /// column and two raster rows per terminal row, keeping formulas readable.
    pub fn configure(&mut self, picker: Picker) {
        let picker = if picker.protocol_type() == ProtocolType::Halfblocks {
            Picker::from_fontsize((1, 2).into())
        } else {
            picker
        };
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
            if let Some(Cached {
                entry: Entry::Ready(image),
                ..
            }) = self.entries.remove(&key)
                && let SlicedProtocol::Kitty(kitty) = image.as_ref()
            {
                self.deleted.push(kitty.image_id());
            }
        }
    }

    fn lookup(
        &mut self,
        source: SourceRef<'_>,
        columns: u16,
        request: bool,
    ) -> Option<Arc<SlicedProtocol>> {
        let picker = self.picker.as_ref()?.clone();
        let columns = columns.min(120);
        if columns == 0 {
            return None;
        }
        self.clock = self.clock.saturating_add(1);
        if let Some((_, cached)) = self
            .entries
            .iter_mut()
            .find(|(key, _)| key.columns == columns && source.matches(&key.source))
        {
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
        let key = Key {
            source: source.owned(),
            columns,
        };
        let reserved = key.source.bytes().saturating_mul(2);
        if self
            .entries
            .values()
            .map(|entry| entry.bytes)
            .sum::<usize>()
            .saturating_add(reserved)
            > MAX_CACHE_BYTES
        {
            return None;
        }
        let jobs = self.jobs.get_or_insert_with(|| {
            let (tx, rx) = mpsc::channel(QUEUE);
            let (replies, answers) = mpsc::channel(QUEUE);
            self.replies = Some(answers);
            self.task = Some(tokio::spawn(serve(rx, replies)));
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
                bytes: reserved,
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
                } if valid_size(*width, *height, 4).is_some()
                    && rgba.len() <= MAX_PIXELS * 4 * 4 / 3 + 8 =>
                {
                    sources.push(SourceRef::Image {
                        width: *width,
                        height: *height,
                        rgba,
                    })
                }
                ContentBlock::Math { tex, .. } if tex.len() <= nuschl_ipc::math::MAX_TEX_BYTES => {
                    sources.push(SourceRef::Math { tex, display: true })
                }
                ContentBlock::Text { spans, .. } => {
                    for span in spans.iter().take(32) {
                        if let Some(tex) = &span.math
                            && tex.len() <= nuschl_ipc::math::MAX_TEX_BYTES
                        {
                            sources.push(SourceRef::Math {
                                tex,
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

    /// Delete only images allocated by this app. Kitty uppercase I releases
    /// both the placement and stored pixels. No global-terminal delete.
    pub fn flush_deleted(&mut self) -> std::io::Result<()> {
        use std::io::Write as _;
        let mut output = std::io::stdout().lock();
        for id in self.deleted.drain(..) {
            if self.picker.as_ref().is_some_and(Picker::tmux_detected) {
                write!(
                    output,
                    "\x1bPtmux;\x1b\x1b_Ga=d,d=I,i={id},q=2\x1b\x1b\\\x1b\\"
                )?;
            } else {
                write!(output, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\")?;
            }
        }
        output.flush()
    }

    pub fn shutdown(&mut self) {
        for (_, cached) in self.entries.drain() {
            if let Entry::Ready(image) = cached.entry
                && let SlicedProtocol::Kitty(kitty) = image.as_ref()
            {
                self.deleted.push(kitty.image_id());
            }
        }
        self.jobs = None;
        self.replies = None;
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for Graphics {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
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

fn encoded_len(image: &SlicedProtocol) -> usize {
    match image {
        SlicedProtocol::Kitty(kitty) => kitty.encoded_len(),
        SlicedProtocol::Sixel(sixel) => sixel.borrow_owner().data.len(),
        SlicedProtocol::Halfblocks(_) => {
            usize::from(image.size().width) * usize::from(image.size().height) * 64
        }
        SlicedProtocol::Sliced(rows) => rows
            .iter()
            .map(|row| match row {
                Protocol::Kitty(kitty) => kitty.encoded_len(),
                Protocol::Sixel(sixel) => sixel.data.len(),
                Protocol::ITerm2(iterm) => iterm.data.len(),
                Protocol::Halfblocks(_) => MAX_ENCODED + 1,
            })
            .sum(),
    }
}

fn encode(
    picker: &Picker,
    pixels: image::RgbaImage,
    columns: u16,
) -> Option<(SlicedProtocol, usize)> {
    let pixels = image::DynamicImage::ImageRgba8(pixels);
    let size = ratatui_image::Resize::Fit(None).size_for(
        &pixels,
        picker.font_size(),
        Size::new(columns, MAX_ROWS),
    );
    let font = picker.font_size();
    valid_size(
        u32::from(size.width) * u32::from(font.width),
        u32::from(size.height) * u32::from(font.height),
        4,
    )?;
    let image = SlicedProtocol::new(picker, pixels, Some(size)).ok()?;
    let bytes = encoded_len(&image);
    (bytes <= MAX_ENCODED).then_some((image, bytes.saturating_mul(2)))
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
                        size: if job.picker.protocol_type() == ProtocolType::Halfblocks {
                            12.
                        } else {
                            f32::from(job.picker.font_size().height).clamp(4., 128.)
                        },
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
            let encoded = tokio::task::spawn_blocking(move || encode(&job.picker, pixels, columns))
                .await
                .ok()
                .flatten();
            match encoded {
                Some((image, bytes)) => (Some(image), bytes.saturating_add(input_bytes)),
                None => (None, input_bytes),
            }
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
