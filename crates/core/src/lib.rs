// SPDX-License-Identifier: EUPL-1.2
//! Portable domain types. This crate must not depend on a UI, native FFI, or I/O.

use serde::{Deserialize, Serialize};

pub mod admin;
pub mod audio;
pub mod bookmark;
pub mod model;
pub mod network;
pub mod preview;
pub mod search;
pub mod share;
pub mod transfer;

pub use search::{SearchCursor, SearchPage};

/// Parsing limits are separate from UI collapsing. Storage retains the original body.
/// 256 KiB covers Mumble's default 128 KiB image messages with room for markup.
pub const MAX_CONTENT_BYTES: usize = 256 * 1024;

/// Inline images travel inside message text as `EMBED data:image/...;base64,... EMBED`.
/// The marker is stripped from all other text before embeds are inserted.
pub const EMBED: char = '\u{FFFC}';

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatMessage {
    /// A trusted server identity assigned by the connection adapter, not a remote display name.
    pub server: String,
    pub channel: String,
    pub author: String,
    /// Local observation time. Legacy Mumble does not supply an authoritative timestamp.
    pub observed_at_ms: i64,
    pub body: String,
}

/// A storage ID is meaningful only within its database and server/channel scope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryEntry {
    pub id: i64,
    pub message: ChatMessage,
    /// On a Nuschl server: the message's sequence number in its conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<i64>,
    /// When the author last edited it (Nuschl servers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_ms: Option<i64>,
    /// When it was deleted: a tombstone, whose text is gone (Nuschl servers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_ms: Option<i64>,
}

/// A Nuschl server's message by its per-conversation sequence number, for
/// the local cache (`message.channel` is the local storage key). A record
/// with `deleted_ms` is a tombstone: its body is stored empty whatever it says.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerRecord {
    pub message: ChatMessage,
    pub seq: i64,
    #[serde(default)]
    pub edited_ms: Option<i64>,
    #[serde(default)]
    pub deleted_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryPage {
    /// Chronological display order, including repeated messages with identical timestamps.
    pub messages: Vec<HistoryEntry>,
    /// Exclusive cursor for the next older page; None means the beginning was
    /// reached (a page read forwards, after a message, leaves it None).
    pub next_before: Option<i64>,
    /// Exclusive cursor for the next newer page of a page read around or after
    /// a message; None means the newest stored message is included (a page
    /// read backwards leaves it None).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_after: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageDocument {
    pub blocks: Vec<ContentBlock>,
}

/// Renderers receive text and styles, never executable HTML or instructions to fetch URLs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentBlock {
    Text {
        text: String,
        /// Formatting over parts of the text, sorted by start and disjoint.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        spans: Vec<Span>,
        #[serde(default, skip_serializing_if = "TextRole::is_paragraph")]
        role: TextRole,
    },
    Code {
        language: Option<String>,
        runs: Vec<CodeRun>,
    },
    /// Decoded, downscaled pixels from the content worker: RGBA8, base64 encoded.
    Image {
        width: u32,
        height: u32,
        rgba: String,
        /// The `data:` URI the image came from, for a larger view (decoded again
        /// by the worker) and for saving the original file.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    /// A display formula (`$$…$$` or `\[…\]`): `tex` is the LaTeX between
    /// the delimiters, which the math worker renders; `source` is the
    /// formula as written, for copying, screen readers and when it cannot
    /// be shown.
    Math { source: String, tex: String },
}

impl ContentBlock {
    /// A plain paragraph.
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text {
            text: text.into(),
            spans: Vec::new(),
            role: TextRole::Paragraph,
        }
    }
}

/// What a text block is.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TextRole {
    #[default]
    Paragraph,
    /// A heading of level 1 to 3.
    Heading(u8),
    /// A list item, nested from depth 1, with its bullet or number.
    Item {
        depth: u8,
        marker: String,
    },
    Quote,
    /// A horizontal rule; its text is empty.
    Rule,
}

impl TextRole {
    pub fn is_paragraph(&self) -> bool {
        *self == TextRole::Paragraph
    }
}

/// Formatting over the bytes `start..end` of a text block.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    #[serde(default, skip_serializing_if = "Style::is_plain")]
    pub style: Style,
    /// The http, https or mumble URL this part links to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// Inline LaTeX math (`$…$` or `\(…\)`): the LaTeX between the
    /// delimiters. The span's text is the formula as written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub math: Option<String>,
}

/// Inline formatting from legacy Mumble rich text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    /// Inline code: monospace.
    pub code: bool,
    /// The sender's colour; renderers adjust it to stay legible.
    pub color: Option<[u8; 3]>,
}

impl Style {
    pub fn is_plain(&self) -> bool {
        *self == Style::default()
    }
}

/// The link schemes messages may use; anything else stays plain text.
pub fn allowed_link(url: &str) -> bool {
    url.split_once("://").is_some_and(|(scheme, rest)| {
        !rest.is_empty()
            && ["http", "https", "mumble"]
                .iter()
                .any(|allowed| scheme.eq_ignore_ascii_case(allowed))
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeRun {
    pub text: String,
    pub foreground: [u8; 4],
    pub bold: bool,
    pub italic: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreviewPolicy {
    pub max_lines: usize,
    pub max_chars: usize,
}

impl Default for PreviewPolicy {
    fn default() -> Self {
        Self {
            max_lines: 12,
            max_chars: 2_000,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MessagePreview<'a> {
    pub text: &'a str,
    /// A UI renders a keyboard-accessible "Show more" control when true.
    pub has_more: bool,
}

impl PreviewPolicy {
    /// Preserve the original bytes and Unicode boundaries; expansion never edits the body.
    pub fn preview(self, text: &str, expanded: bool) -> MessagePreview<'_> {
        if !expanded {
            let mut lines = 1;
            for (characters, (index, character)) in text.char_indices().enumerate() {
                if characters >= self.max_chars || (character == '\n' && lines >= self.max_lines) {
                    return MessagePreview {
                        text: &text[..index],
                        has_more: true,
                    };
                }
                if character == '\n' {
                    lines += 1;
                }
            }
        }
        MessagePreview {
            text,
            has_more: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_preserves_newlines_and_expansion_restores_the_original() {
        let body = "first\n\nthird\nfourth";
        let policy = PreviewPolicy {
            max_lines: 3,
            max_chars: 100,
        };
        assert_eq!(
            policy.preview(body, false),
            MessagePreview {
                text: "first\n\nthird",
                has_more: true
            }
        );
        assert_eq!(policy.preview(body, true).text, body);
    }

    #[test]
    fn long_single_lines_collapse_on_unicode_boundaries() {
        let policy = PreviewPolicy {
            max_lines: 12,
            max_chars: 2,
        };
        assert_eq!(
            policy.preview("🦀éλ", false),
            MessagePreview {
                text: "🦀é",
                has_more: true
            }
        );
        assert!(!policy.preview("🦀é", false).has_more);
    }
}
