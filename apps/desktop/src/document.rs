// SPDX-License-Identifier: EUPL-1.2
//! Worker documents converted once into renderer-ready values. Images arrive as
//! raw RGBA from the sandboxed content worker; the UI never parses image formats.
use base64::Engine as _;
use gpui_kit::RenderImage;
use nuschel_core::{CodeRun, ContentBlock, MessageDocument};
use std::sync::Arc;

#[derive(Clone)]
pub enum Block {
    Text(String),
    Code {
        language: Option<String>,
        runs: Vec<CodeRun>,
    },
    Image {
        image: Arc<RenderImage>,
        width: u32,
        height: u32,
        /// The original `data:` URI (viewer and Save as), when known.
        source: Option<Arc<str>>,
    },
}

#[derive(Clone, Default)]
pub struct Document {
    pub blocks: Vec<Block>,
}

/// Build a GPUI image from worker RGBA pixels (GPUI expects BGRA).
pub fn render_image(width: u32, height: u32, rgba: &str) -> Option<Arc<RenderImage>> {
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        return None;
    }
    let mut pixels = base64::engine::general_purpose::STANDARD
        .decode(rgba)
        .ok()?;
    if pixels.len() != (width as usize) * (height as usize) * 4 {
        return None;
    }
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(width, height, pixels)?;
    Some(crate::images::track(Arc::new(RenderImage::new(vec![
        image::Frame::new(buffer),
    ]))))
}

impl From<MessageDocument> for Document {
    fn from(document: MessageDocument) -> Self {
        Self {
            blocks: document
                .blocks
                .into_iter()
                .map(|block| match block {
                    ContentBlock::Text { text } => Block::Text(text),
                    ContentBlock::Code { language, runs } => Block::Code { language, runs },
                    ContentBlock::Image {
                        width,
                        height,
                        rgba,
                        source,
                    } => match render_image(width, height, &rgba) {
                        Some(image) => Block::Image {
                            image,
                            width,
                            height,
                            source: source.map(Arc::from),
                        },
                        None => Block::Text("[image]".into()),
                    },
                })
                .collect(),
        }
    }
}

/// The file inside an inline `data:image/...;base64,` URI: its bytes and the
/// extension for its type. Only base64 (with Mumble's percent-encoding) is
/// decoded here; the image format itself is never parsed in this process.
pub fn data_uri_file(uri: &str) -> Option<(Vec<u8>, &'static str)> {
    let rest = uri.trim().strip_prefix("data:image/")?;
    let (kind, payload) = rest.split_once(";base64,")?;
    let extension = match kind.to_ascii_lowercase().as_str() {
        "png" => "png",
        "jpeg" | "jpg" => "jpg",
        "gif" => "gif",
        _ => return None,
    };
    let mut text = String::with_capacity(payload.len());
    let mut chars = payload.chars().filter(|c| !c.is_whitespace());
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            text.push(u8::from_str_radix(&hex, 16).ok()? as char);
        } else {
            text.push(c);
        }
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(text)
        .ok()?;
    Some((bytes, extension))
}

impl Document {
    pub fn first_image(&self) -> Option<Arc<RenderImage>> {
        self.blocks.iter().find_map(|block| match block {
            Block::Image { image, .. } => Some(image.clone()),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Block, Document, render_image};
    use base64::Engine as _;
    use nuschel_core::{ContentBlock, MessageDocument};

    #[test]
    fn saved_images_keep_their_bytes_and_type() {
        // Mumble percent-encodes the base64 text: [0xfb, 0xff, 0x00] is "+/8A".
        let (bytes, extension) = super::data_uri_file("data:image/PNG;base64,%2B%2F8A").unwrap();
        assert_eq!(extension, "png");
        assert_eq!(bytes, [0xfb, 0xff, 0x00]);
        assert!(super::data_uri_file("data:image/svg+xml;base64,PHN2Zz4=").is_none());
        assert!(super::data_uri_file("https://example.invalid/a.png").is_none());
    }

    #[test]
    fn converts_worker_images_and_rejects_inconsistent_sizes() {
        let rgba = base64::engine::general_purpose::STANDARD.encode([1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(render_image(2, 1, &rgba).is_some());
        assert!(render_image(3, 1, &rgba).is_none());
        assert!(render_image(2, 1, "not base64!").is_none());
        let document = Document::from(MessageDocument {
            blocks: vec![ContentBlock::Image {
                width: 9,
                height: 9,
                rgba,
                source: None,
            }],
        });
        assert!(matches!(document.blocks[0], Block::Text(ref text) if text == "[image]"));
    }
}
