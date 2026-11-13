// SPDX-License-Identifier: EUPL-1.2
//! Offline preview operations in the content worker. No URL is a fetch command.
use base64::Engine as _;
use nuschl_core::preview::{
    EmbeddedImage, MAX_ALT_BYTES, MAX_DESCRIPTION_BYTES, MAX_IMAGE_BYTES, MAX_IMAGE_HEIGHT,
    MAX_IMAGE_WIDTH, MAX_TITLE_BYTES, MAX_URL_BYTES,
};
use serde::{Deserialize, Serialize};

pub const SCHEMA: u16 = 1;
pub const MAX_DOCUMENT_BYTES: usize = 256 * 1024;
pub const MAX_SOURCE_IMAGE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_RGBA_BYTES: usize = MAX_IMAGE_WIDTH as usize * MAX_IMAGE_HEIGHT as usize * 4;
const JSON_OVERHEAD_BYTES: usize = 4096;
pub const MAX_REQUEST_FRAME_BYTES: usize =
    MAX_SOURCE_IMAGE_BYTES.div_ceil(3) * 4 + MAX_ALT_BYTES * 6 + JSON_OVERHEAD_BYTES;
pub const MAX_RESPONSE_FRAME_BYTES: usize = MAX_RGBA_BYTES.div_ceil(3) * 4
    + MAX_IMAGE_BYTES.div_ceil(3) * 4
    + MAX_ALT_BYTES * 6
    + JSON_OVERHEAD_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Charset {
    Utf8,
    Unspecified,
    Other,
}

impl Charset {
    /// Classify a bounded, already-parsed HTTP charset label. Header parameter
    /// parsing itself remains in the isolated HTTP response parser.
    pub fn from_label(label: Option<&str>) -> Self {
        let Some(label) = label else {
            return Self::Unspecified;
        };
        if label.len() > 64 {
            return Self::Other;
        }
        let label = label.trim_matches(|c: char| c.is_ascii_whitespace());
        if [
            "unicode-1-1-utf-8",
            "unicode11utf8",
            "unicode20utf8",
            "utf-8",
            "utf8",
            "x-unicode20utf8",
        ]
        .iter()
        .any(|known| label.eq_ignore_ascii_case(known))
        {
            Self::Utf8
        } else {
            Self::Other
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Document {
        schema: u16,
        body: String,
        charset: Charset,
        truncated: bool,
    },
    PrepareImage {
        schema: u16,
        image: String,
        alt: String,
    },
    RenderImage {
        schema: u16,
        image: EmbeddedImage,
    },
}

impl Request {
    /// Admission before base64 allocation. Exact decoded length is checked too.
    pub fn is_bounded(&self) -> bool {
        match self {
            Self::Document { schema, body, .. } => {
                *schema == SCHEMA && body.len() <= MAX_DOCUMENT_BYTES.div_ceil(3) * 4
            }
            Self::PrepareImage { schema, image, alt } => {
                *schema == SCHEMA
                    && image.len() <= MAX_SOURCE_IMAGE_BYTES.div_ceil(3) * 4
                    && alt.len() <= MAX_ALT_BYTES
            }
            Self::RenderImage { schema, image } => *schema == SCHEMA && bounded_image(image),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    pub title: String,
    pub description: String,
    /// Untrusted reference only: parent joins and checks its target policy again.
    pub image: Option<String>,
    pub image_alt: String,
}

impl Metadata {
    pub fn is_bounded(&self) -> bool {
        normalized(&self.title, MAX_TITLE_BYTES)
            && normalized(&self.description, MAX_DESCRIPTION_BYTES)
            && normalized(&self.image_alt, MAX_ALT_BYTES)
            && self.image.as_ref().is_none_or(|s| {
                !s.is_empty() && s.len() <= MAX_URL_BYTES && !s.chars().any(refused)
            })
            && (self.image.is_some() || self.image_alt.is_empty())
            && !(self.title.is_empty() && self.description.is_empty() && self.image.is_none())
    }
}

/// Source-free pixels, not an image resource or fetch instruction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pixels {
    pub width: u32,
    pub height: u32,
    pub rgba: String,
}

impl Pixels {
    pub fn decode(&self) -> Option<Vec<u8>> {
        let expected =
            usize::try_from(self.width.checked_mul(self.height)?.checked_mul(4)?).ok()?;
        if self.width == 0
            || self.height == 0
            || self.width > MAX_IMAGE_WIDTH
            || self.height > MAX_IMAGE_HEIGHT
            || expected > MAX_RGBA_BYTES
            || self.rgba.len() != expected.div_ceil(3) * 4
        {
            return None;
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&self.rgba)
            .ok()?;
        (bytes.len() == expected).then_some(bytes)
    }

    pub fn is_bounded(&self) -> bool {
        self.decode().is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unavailable {
    NoMetadata,
    Encoding,
    TooLarge,
    Malformed,
    Image,
    Version,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    Metadata {
        schema: u16,
        metadata: Metadata,
    },
    Image {
        schema: u16,
        image: EmbeddedImage,
        pixels: Pixels,
    },
    Pixels {
        schema: u16,
        pixels: Pixels,
    },
    Unavailable {
        schema: u16,
        reason: Unavailable,
    },
}

impl Response {
    /// Bound and sanitize the reply before using its fields. Encoded-image
    /// headers additionally pass the preview Card check; only workers decode.
    pub fn is_bounded(&self) -> bool {
        match self {
            Self::Metadata { schema, metadata } => *schema == SCHEMA && metadata.is_bounded(),
            Self::Image {
                schema,
                image,
                pixels,
            } => {
                *schema == SCHEMA
                    && bounded_image(image)
                    && image.width == pixels.width
                    && image.height == pixels.height
                    && pixels.is_bounded()
            }
            Self::Pixels { schema, pixels } => *schema == SCHEMA && pixels.is_bounded(),
            Self::Unavailable { schema, .. } => *schema == SCHEMA,
        }
    }
}

fn bounded_image(image: &EmbeddedImage) -> bool {
    !image.data.is_empty()
        && image.data.len() <= MAX_IMAGE_BYTES
        && image.width > 0
        && image.height > 0
        && image.width <= MAX_IMAGE_WIDTH
        && image.height <= MAX_IMAGE_HEIGHT
        && normalized(&image.alt, MAX_ALT_BYTES)
}

fn refused(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}' | '\u{fffe}' | '\u{ffff}')
}

fn normalized(text: &str, limit: usize) -> bool {
    if text.len() > limit || text.starts_with(' ') || text.ends_with(' ') {
        return false;
    }
    let mut space = false;
    for c in text.chars() {
        if refused(c) || (c.is_whitespace() && c != ' ') || (space && c == ' ') {
            return false;
        }
        space = c == ' ';
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_is_inert_bounded_and_image_alt_is_not_orphaned() {
        assert_eq!(Charset::from_label(None), Charset::Unspecified);
        assert_eq!(Charset::from_label(Some(" UTF8 ")), Charset::Utf8);
        assert_eq!(Charset::from_label(Some("windows-1252")), Charset::Other);
        let mut data = Metadata {
            title: "t".repeat(MAX_TITLE_BYTES),
            description: String::new(),
            image: None,
            image_alt: String::new(),
        };
        assert!(data.is_bounded());
        data.title.push('t');
        assert!(!data.is_bounded());
        data.title = "two  spaces".into();
        assert!(!data.is_bounded());
        data.title = "ok".into();
        data.image_alt = "orphan".into();
        assert!(!data.is_bounded());
        data.image = Some("/image.png".into());
        assert!(data.is_bounded());
        data.image = Some("/image\n.png".into());
        assert!(!data.is_bounded());
    }

    #[test]
    fn pixels_check_dimensions_and_exact_base64_before_decoding() {
        let mut pixels = Pixels {
            width: 1,
            height: 1,
            rgba: "yFAo/w==".into(),
        };
        assert_eq!(pixels.decode(), Some(vec![200, 80, 40, 255]));
        pixels.width = MAX_IMAGE_WIDTH + 1;
        assert!(!pixels.is_bounded());
        pixels.width = 1;
        pixels.rgba = "AAAA".into();
        assert!(!pixels.is_bounded());
        pixels.rgba = "!!!!!!!!".into();
        assert!(!pixels.is_bounded());
    }

    #[test]
    fn maximal_pixel_reply_and_nested_tags_fit_the_frame_budget() {
        let pixels = Pixels {
            width: MAX_IMAGE_WIDTH,
            height: MAX_IMAGE_HEIGHT,
            rgba: base64::engine::general_purpose::STANDARD.encode(vec![0; MAX_RGBA_BYTES]),
        };
        let image = EmbeddedImage {
            mime: nuschl_core::preview::ImageMime::Png,
            data: vec![0; MAX_IMAGE_BYTES],
            width: pixels.width,
            height: pixels.height,
            alt: "\"".repeat(MAX_ALT_BYTES),
        };
        let response = Response::Image {
            schema: SCHEMA,
            image,
            pixels,
        };
        assert!(response.is_bounded());
        let bytes = serde_json::to_vec(&crate::Envelope::new(
            1,
            crate::ContentResponse::Preview(response),
        ))
        .unwrap();
        assert!(bytes.len() <= MAX_RESPONSE_FRAME_BYTES);
        let request = Request::Document {
            schema: SCHEMA,
            body: String::new(),
            charset: Charset::Utf8,
            truncated: false,
        };
        let bytes = serde_json::to_vec(&crate::Envelope::new(
            1,
            crate::ContentRequest::Preview(request),
        ))
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["payload"]["operation"],
            "document"
        );
    }
}
