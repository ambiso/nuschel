// SPDX-License-Identifier: EUPL-1.2
//! Portable domain types. This crate must not depend on a UI, native FFI, or I/O.

use serde::{Deserialize, Serialize};

/// Parsing limits are separate from UI collapsing. Storage retains the original body.
pub const MAX_CONTENT_BYTES: usize = 64 * 1024;

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
    },
    Code {
        language: Option<String>,
        runs: Vec<CodeRun>,
    },
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
