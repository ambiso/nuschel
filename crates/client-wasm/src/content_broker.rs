// SPDX-License-Identifier: EUPL-1.2
//! Browser SDK content admission. This module never parses HTML, syntax or images.
//! Replies are inert derivatives; original source and context stay with the SDK.

use nuschl_core::{ContentBlock, MAX_CONTENT_BYTES, MessageDocument, TextRole};
use serde::Deserialize;

pub const MAX_REPLY_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_PIXEL_BYTES: usize = 640 * 1024;
pub const MAX_DOCUMENT_CACHE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PENDING_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_PENDING_JOBS: usize = 128;
pub const MAX_POSTED_JOBS: usize = 4;
const INVALID: &str = "Invalid or oversized content worker reply.";

pub use nuschl_client::timeline::SourceFormat as SourceMode;

/// UI RPC contracts. `context` is issued by the SDK snapshot, never an index
/// inferred by the UI. Raw selected file bytes remain outside this module.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageBegin {
    pub context: String,
    pub caption: String,
    pub attachment_revision: String,
    pub draft_revision: String,
}

#[derive(Debug, serde::Serialize)]
pub struct ImageBudget {
    pub ticket: String,
    pub max_input_bytes: usize,
    /// Remaining HTML budget after the SDK's captured caption and separator.
    pub max_html_bytes: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageSend {
    pub ticket: String,
    pub attachment_revision: String,
    pub draft_revision: String,
    pub html: String,
    pub thumbnail: ContentBlock,
}

/// Local admission to the original transport, not a Mumble delivery receipt.
#[derive(Debug, serde::Serialize)]
pub struct ImageAdmission {
    pub ticket: String,
    pub attachment_revision: String,
    pub draft_revision: String,
    pub state: ImageAdmissionState,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageAdmissionState {
    SentToTransport,
}

/// Fixed data-image protocol, not an HTML parser. No URL, attributes, alternate
/// tags or worker-selected source is accepted. Base64 unwrap is bounded basic
/// protocol logic; image parsing/encoding remains in the credential-free worker.
pub fn validate_image_html(html: &str) -> Result<(), &'static str> {
    if html.len() > MAX_CONTENT_BYTES {
        return Err(INVALID);
    }
    let payload = html
        .strip_prefix("<img src=\"data:image/png;base64,")
        .map(|payload| (payload, true))
        .or_else(|| {
            html.strip_prefix("<img src=\"data:image/jpeg;base64,")
                .map(|payload| (payload, false))
        })
        .ok_or(INVALID)?;
    let (payload, png) = payload;
    let payload = payload.strip_suffix("\" />").ok_or(INVALID)?;
    let mut base64 = String::with_capacity(payload.len());
    let mut bytes = payload.bytes();
    while let Some(byte) = bytes.next() {
        match byte {
            b'\n' => {}
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => base64.push(char::from(byte)),
            b'%' => {
                let pair = [bytes.next().ok_or(INVALID)?, bytes.next().ok_or(INVALID)?];
                base64.push(match &pair {
                    b"2B" => '+',
                    b"2F" => '/',
                    b"3D" => '=',
                    _ => return Err(INVALID),
                });
            }
            _ => return Err(INVALID),
        }
    }
    if base64.len() < 12 || !base64.len().is_multiple_of(4) {
        return Err(INVALID);
    }
    let padding = base64
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'=')
        .count();
    if padding > 2
        || !canonical_base64(&base64, base64.len() / 4 * 3 - padding)
        || !(if png {
            base64.starts_with("iVBORw0KGgo")
        } else {
            base64.starts_with("/9j/")
        })
    {
        return Err(INVALID);
    }
    Ok(())
}

pub fn validate_thumbnail(thumbnail: &ContentBlock) -> Result<(), &'static str> {
    if !matches!(thumbnail, ContentBlock::Image { .. }) {
        return Err(INVALID);
    }
    validate_document(&MessageDocument {
        blocks: vec![thumbnail.clone()],
    })?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DocumentWire {
    blocks: Vec<ContentBlock>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenderWire {
    marked_text: String,
    plain_text: String,
    document: DocumentWire,
}

pub struct ValidatedRender {
    pub document: MessageDocument,
    pub plain_text: String,
    /// Conservative logical charge, not a measurement of browser process RSS:
    /// encoded strings, object counts and decoded/canvas pixel copies all count.
    pub retained_bytes: usize,
}

pub fn validate_render(raw: &str) -> Result<ValidatedRender, &'static str> {
    // Bound before serde constructs any strings or arrays from the reply.
    if raw.len() > MAX_REPLY_BYTES {
        return Err(INVALID);
    }
    let reply: RenderWire = serde_json::from_str(raw).map_err(|_| INVALID)?;
    if reply.marked_text.len() > MAX_CONTENT_BYTES || reply.plain_text.len() > MAX_CONTENT_BYTES {
        return Err(INVALID);
    }
    let document = MessageDocument {
        blocks: reply.document.blocks,
    };
    let budget = validate_document(&document)?;
    let retained_bytes = raw
        .len()
        .saturating_mul(2)
        .saturating_add(budget.pixels.saturating_mul(3))
        .saturating_add(document.blocks.len().saturating_mul(128))
        .saturating_add(budget.spans.saturating_mul(64))
        .saturating_add(budget.runs.saturating_mul(96));
    Ok(ValidatedRender {
        document,
        plain_text: reply.plain_text,
        retained_bytes,
    })
}

#[derive(Default)]
struct Budget {
    text: usize,
    spans: usize,
    runs: usize,
    images: usize,
    pixels: usize,
}

impl Budget {
    fn text(&mut self, text: &str) -> Result<(), &'static str> {
        self.text = self.text.saturating_add(text.len());
        if self.text > MAX_CONTENT_BYTES {
            return Err(INVALID);
        }
        Ok(())
    }
}

fn validate_document(document: &MessageDocument) -> Result<Budget, &'static str> {
    if !document.within_limits() {
        return Err(INVALID);
    }
    let mut budget = Budget::default();
    for block in &document.blocks {
        match block {
            ContentBlock::Break { .. } => {}
            ContentBlock::Text { text, spans, role } => {
                budget.text(text)?;
                match role {
                    TextRole::Heading(level) if !(1..=3).contains(level) => return Err(INVALID),
                    TextRole::Item { depth, marker }
                        if !(1..=4).contains(depth) || marker.len() > 32 =>
                    {
                        return Err(INVALID);
                    }
                    _ => {}
                }
                budget.spans = budget.spans.saturating_add(spans.len());
                if budget.spans > 2048 {
                    return Err(INVALID);
                }
                let mut end = 0;
                for span in spans {
                    if span.start < end
                        || span.end < span.start
                        || span.end > text.len()
                        || !text.is_char_boundary(span.start)
                        || !text.is_char_boundary(span.end)
                        || span
                            .link
                            .as_ref()
                            .is_some_and(|url| url.len() > 2048 || !nuschl_core::allowed_link(url))
                        || span.math.as_ref().is_some_and(|tex| tex.len() > 4096)
                    {
                        return Err(INVALID);
                    }
                    end = span.end;
                }
            }
            ContentBlock::Code { language, runs } => {
                if language
                    .as_ref()
                    .is_some_and(|language| language.len() > 256)
                {
                    return Err(INVALID);
                }
                budget.runs = budget.runs.saturating_add(runs.len());
                if budget.runs > 32768 {
                    return Err(INVALID);
                }
                for run in runs {
                    budget.text(&run.text)?;
                }
            }
            ContentBlock::Image {
                width,
                height,
                rgba,
                source,
            } => {
                if source.is_some() || *width == 0 || *height == 0 || *width > 360 || *height > 270
                {
                    return Err(INVALID);
                }
                let bytes = (*width as usize)
                    .checked_mul(*height as usize)
                    .and_then(|pixels| pixels.checked_mul(4))
                    .ok_or(INVALID)?;
                budget.images += 1;
                budget.pixels = budget.pixels.checked_add(bytes).ok_or(INVALID)?;
                if budget.images > 8
                    || budget.pixels > MAX_PIXEL_BYTES
                    || !canonical_base64(rgba, bytes)
                {
                    return Err(INVALID);
                }
            }
            ContentBlock::Math { source, tex } => {
                budget.text(source)?;
                if tex.len() > 4096 {
                    return Err(INVALID);
                }
            }
        }
    }
    Ok(budget)
}

/// Shape and canonical padding only, without invoking an image/base64 decoder.
fn canonical_base64(text: &str, bytes: usize) -> bool {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let value = |byte| alphabet.iter().position(|candidate| *candidate == byte);
    let padding = (3 - bytes % 3) % 3;
    let encoded = text.as_bytes();
    if encoded.len() != bytes.div_ceil(3) * 4 {
        return false;
    }
    for (index, byte) in encoded.iter().copied().enumerate() {
        if index >= encoded.len() - padding {
            if byte != b'=' {
                return false;
            }
        } else if value(byte).is_none() {
            return false;
        }
    }
    padding == 0
        || value(encoded[encoded.len() - padding - 1])
            .is_some_and(|last| last & if padding == 2 { 15 } else { 3 } == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reply(blocks: serde_json::Value) -> String {
        json!({"marked_text":"original derivative","plain_text":"plain derivative","document":{"blocks":blocks}}).to_string()
    }

    #[test]
    fn real_shapes_are_inert_and_source_uris_never_cross_this_boundary() {
        let raw = reply(json!([
            {"kind":"text","text":"🦀 bold","spans":[{"start":5,"end":9,"style":{"bold":true},"link":"https://example.invalid/x"}]},
            {"kind":"break","count":2},
            {"kind":"image","width":1,"height":1,"rgba":"/wAA/w=="}
        ]));
        let accepted = validate_render(&raw).unwrap();
        assert_eq!(accepted.document.blocks.len(), 3);
        assert!(accepted.retained_bytes > raw.len() * 2);
        let hostile = raw.replace(
            "\"rgba\"",
            "\"source\":\"https://example.invalid/tracker\",\"rgba\"",
        );
        assert!(validate_render(&hostile).is_err());
    }

    #[test]
    fn aggregate_and_untrusted_shape_failures_are_rejected() {
        for blocks in [
            json!([{"kind":"break","count":4294967295u32}]),
            json!([{"kind":"break","count":131072},{"kind":"text","text":"x"},{"kind":"break","count":131073}]),
            json!([{"kind":"text","text":"🦀","spans":[{"start":1,"end":4}]}]),
            json!([{"kind":"text","text":"x","spans":[{"start":0,"end":1,"link":"javascript:alert(1)"}]}]),
            json!([{"kind":"image","width":1,"height":1,"rgba":"/wAA/x=="}]),
            json!([{"kind":"image","width":4294967295u32,"height":4294967295u32,"rgba":""}]),
            json!([{"kind":"text","text":"x","fetch":"https://example.invalid"}]),
        ] {
            assert!(validate_render(&reply(blocks)).is_err());
        }
        assert!(
            validate_render(&reply(
                json!([{"kind":"text","text":"x".repeat(MAX_CONTENT_BYTES+1)}])
            ))
            .is_err()
        );
        assert!(validate_render(&" ".repeat(MAX_REPLY_BYTES + 1)).is_err());
        let repeated = reply(json!([{"kind":"text","text":"x"}])).replace(
            "\"plain_text\":",
            "\"plain_text\":\"first\",\"plain_text\":",
        );
        assert!(validate_render(&repeated).is_err());
    }
}
