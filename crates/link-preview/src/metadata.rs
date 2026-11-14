// SPDX-License-Identifier: EUPL-1.2
//! What Open Graph extraction hands back: a bounded, inert value with no
//! parser behind it. The HTML itself is tokenized only in the content worker
//! (or the content WASM worker) by `nuschl_content::preview`; this crate
//! never parses a page.
use crate::model::{MAX_ALT_BYTES, MAX_DESCRIPTION_BYTES, MAX_TITLE_BYTES, MAX_URL_BYTES};
use crate::text::is_normalized;

/// The largest document extraction accepts, as the fetch caps it.
pub const MAX_DOCUMENT_BYTES: usize = 256 * 1024;

/// What the HTTP `Content-Type` charset parameter said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportCharset {
    Utf8,
    /// No charset parameter: a `<meta>` declaration may name one.
    Unspecified,
    /// Any other label: no preview (only UTF-8 is decoded).
    Other,
}

/// Normalised metadata within the carried model's limits. `image` is a raw,
/// page-controlled reference that the supervisor resolves against the
/// fetched document URL with `policy::Target::join`, which applies the full
/// target policy again.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtractedMetadata {
    pub title: String,
    pub description: String,
    pub image: Option<String>,
    pub image_alt: String,
}

impl ExtractedMetadata {
    /// Whether a value (for example a worker reply) keeps this type's
    /// bounds: normalised strings within the limits, an image reference
    /// without controls and at most a URL long, alternative text only with
    /// an image, and something to show.
    pub fn is_bounded(&self) -> bool {
        is_normalized(&self.title, MAX_TITLE_BYTES)
            && is_normalized(&self.description, MAX_DESCRIPTION_BYTES)
            && is_normalized(&self.image_alt, MAX_ALT_BYTES)
            && self.image.as_ref().is_none_or(|image| {
                !image.is_empty()
                    && image.len() <= MAX_URL_BYTES
                    && !image.chars().any(crate::text::refused)
            })
            && (self.image.is_some() || self.image_alt.is_empty())
            && !(self.title.is_empty() && self.description.is_empty() && self.image.is_none())
    }
}

/// Why a preview is unavailable, without free text from the page or the
/// worker. Extraction returns the first three.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// Neither a title, a description nor an image was found.
    NoMetadata,
    /// A byte-order mark, the transport or the document declares an
    /// encoding other than UTF-8.
    Encoding,
    /// The document exceeds [`MAX_DOCUMENT_BYTES`].
    TooLarge,
    MediaType,
    Network,
    Proxy,
    Address,
    Timeout,
    Malformed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_bounded_inert_values_pass() {
        let good = ExtractedMetadata {
            title: "A title".into(),
            description: String::new(),
            image: Some("/a.png".into()),
            image_alt: "Alt".into(),
        };
        assert!(good.is_bounded());
        for bad in [
            ExtractedMetadata::default(),
            ExtractedMetadata {
                title: " padded".into(),
                ..good.clone()
            },
            ExtractedMetadata {
                title: "t".repeat(MAX_TITLE_BYTES + 1),
                ..good.clone()
            },
            ExtractedMetadata {
                description: "a\u{202e}b".into(),
                ..good.clone()
            },
            ExtractedMetadata {
                image: Some("/a\u{7}".into()),
                ..good.clone()
            },
            ExtractedMetadata {
                image: Some(format!("/{}", "a".repeat(MAX_URL_BYTES))),
                ..good.clone()
            },
            ExtractedMetadata {
                image: Some(String::new()),
                ..good.clone()
            },
            ExtractedMetadata {
                image: None,
                ..good.clone()
            },
        ] {
            assert!(!bad.is_bounded(), "{bad:?}");
        }
    }
}
