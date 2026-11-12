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

impl MessageDocument {
    /// Whether a worker's document is within every budget its consumers rely
    /// on: at most [`MAX_BLOCKS`] blocks other than breaks, and its breaks
    /// within theirs ([`MessageDocument::breaks_valid`]). One that is not is
    /// malformed and refused before anything converts, copies or lays it out
    /// (the desktop's and the browser's boundaries alike).
    pub fn within_limits(&self) -> bool {
        let blocks = self.blocks.iter().filter(|block| !block.is_break()).count();
        blocks <= MAX_BLOCKS && self.breaks_valid()
    }

    /// Whether the document's [`ContentBlock::Break`]s are within their
    /// budget: at most [`MAX_BREAKS`], never two in a row, and all of them
    /// together at most [`MAX_BREAK_LINES`] line breaks, as many as the
    /// largest message can hold. A reply asking for more is malformed.
    pub fn breaks_valid(&self) -> bool {
        let mut breaks = 0;
        let mut lines: usize = 0;
        let mut previous = false;
        for block in &self.blocks {
            let ContentBlock::Break { count } = block else {
                previous = false;
                continue;
            };
            breaks += 1;
            lines = lines.saturating_add(*count as usize);
            if previous || breaks > MAX_BREAKS || lines > MAX_BREAK_LINES {
                return false;
            }
            previous = true;
        }
        true
    }
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
    /// The line breaks typed between the neighbouring blocks, or before the
    /// first or after the last: `count - 1` blank lines between two blocks,
    /// `count` blank lines at the start or end, and 0 keeps a display formula
    /// on the line of the text beside it (next to an image it is one line
    /// break: images are never inline). Without one, neighbouring blocks are
    /// one line break apart and nothing precedes the first or follows the
    /// last, so documents built without breaks keep their layout. At most one
    /// stands between two other blocks ([`MAX_BREAKS`]), and all counts
    /// together are at most the source's line breaks ([`MAX_BREAK_LINES`]).
    Break { count: u32 },
}

/// Blocks of every other kind per document (the content worker's limit).
pub const MAX_BLOCKS: usize = 512;
/// [`ContentBlock::Break`]s per document: at most one between two other
/// blocks and one at each end; they have their own budget, so the limit on
/// other blocks stays as it is.
pub const MAX_BREAKS: usize = MAX_BLOCKS + 1;
/// Line breaks all [`ContentBlock::Break`]s of a document count together: no
/// more than a message of [`MAX_CONTENT_BYTES`] can hold (every one of them
/// is a typed line break).
pub const MAX_BREAK_LINES: usize = MAX_CONTENT_BYTES;

impl ContentBlock {
    /// Whether this is a [`ContentBlock::Break`], which holds no content.
    pub fn is_break(&self) -> bool {
        matches!(self, ContentBlock::Break { .. })
    }

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
    fn breaks_have_their_own_shape_and_budget() {
        let document = MessageDocument {
            blocks: vec![
                ContentBlock::Break { count: 2 },
                ContentBlock::text("a"),
                ContentBlock::Break { count: 0 },
                ContentBlock::Math {
                    source: "$$x$$".into(),
                    tex: "x".into(),
                },
            ],
        };
        let json = serde_json::to_string(&document).unwrap();
        assert!(
            json.starts_with(r#"{"blocks":[{"kind":"break","count":2},"#),
            "{json}"
        );
        assert_eq!(
            serde_json::from_str::<MessageDocument>(&json).unwrap(),
            document
        );
        assert!(document.breaks_valid());
        assert!(
            serde_json::from_str::<MessageDocument>(
                r#"{"blocks":[{"kind":"break","count":1,"extra":1}]}"#
            )
            .is_err()
        );
        // Two in a row, too many, or more line breaks than a message holds.
        let breaks = |blocks: Vec<ContentBlock>| MessageDocument { blocks }.breaks_valid();
        let one = || ContentBlock::Break { count: 1 };
        assert!(!breaks(vec![one(), one()]));
        assert!(!breaks(vec![ContentBlock::Break {
            count: MAX_CONTENT_BYTES as u32 + 1
        }]));
        let mut many = Vec::new();
        for _ in 0..=MAX_BREAKS {
            many.extend([one(), ContentBlock::text("x")]);
        }
        assert!(!breaks(many.clone()));
        many.truncate(2 * MAX_BREAKS);
        assert!(breaks(many));
    }

    /// A reply may not ask for more line breaks, all its breaks together,
    /// than a message can hold: 512 one-letter blocks with a whole message's
    /// line breaks between each two asked for 133,955,584 of them.
    #[test]
    fn breaks_together_stay_within_a_message() {
        let mut blocks = Vec::new();
        for index in 0..MAX_BLOCKS {
            if index > 0 {
                blocks.push(ContentBlock::Break {
                    count: MAX_CONTENT_BYTES as u32,
                });
            }
            blocks.push(ContentBlock::text("x"));
        }
        assert!(!MessageDocument { blocks }.breaks_valid());
        // A whole message's line breaks, spread over the gaps, are fine; one
        // more is not.
        let spread = |extra: u32| MessageDocument {
            blocks: vec![
                ContentBlock::Break {
                    count: MAX_CONTENT_BYTES as u32 / 2,
                },
                ContentBlock::text("x"),
                ContentBlock::Break {
                    count: MAX_CONTENT_BYTES as u32 / 2 + extra,
                },
            ],
        };
        assert!(spread(0).breaks_valid());
        assert!(!spread(1).breaks_valid());
        assert!(
            !MessageDocument {
                blocks: vec![ContentBlock::Break { count: u32::MAX }],
            }
            .breaks_valid()
        );
    }

    /// The desktop's and the browser's boundaries refuse a document of more
    /// blocks than the worker makes (`within_limits`); breaks do not count.
    #[test]
    fn documents_have_one_budget_for_their_blocks() {
        let document = |count: usize| MessageDocument {
            blocks: (0..count)
                .flat_map(|_| [ContentBlock::text("x"), ContentBlock::Break { count: 2 }])
                .collect(),
        };
        assert!(document(MAX_BLOCKS).within_limits());
        assert!(!document(MAX_BLOCKS + 1).within_limits());
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
