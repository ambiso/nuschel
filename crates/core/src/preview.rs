// SPDX-License-Identifier: EUPL-1.2
//! Typed, carried data. A preview is not an instruction for a receiver to fetch.
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use url::Url;

pub const VERSION: u16 = 1;
pub const MAX_URL_BYTES: usize = 2048;
pub const MAX_TITLE_BYTES: usize = 512;
pub const MAX_DESCRIPTION_BYTES: usize = 2048;
pub const MAX_ALT_BYTES: usize = 512;
pub const MAX_IMAGE_BYTES: usize = 32 * 1024;
pub const MAX_IMAGE_WIDTH: u32 = 640;
pub const MAX_IMAGE_HEIGHT: u32 = 360;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderId {
    OpenGraphV1,
}

/// Adding another provider requires implementing its bounded extraction and
/// policy path; merely receiving its identifier never enables a new fetcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub html_metadata: bool,
    pub image: bool,
    pub follows_preview_relation: bool,
}

impl ProviderId {
    pub const fn capabilities(self) -> Capabilities {
        match self {
            Self::OpenGraphV1 => Capabilities {
                html_metadata: true,
                image: true,
                follows_preview_relation: false,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageMime {
    Png,
    Jpeg,
}

impl ImageMime {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddedImage {
    pub mime: ImageMime,
    /// Canonical encoded image bytes, not pixels and never a URL. Only the
    /// content worker may decode these into renderable pixels.
    #[serde(with = "image_bytes")]
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub alt: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preview {
    pub version: u16,
    pub provider: ProviderId,
    /// The actual sender-entered URL. An og:url tag cannot replace it.
    pub url: String,
    pub title: String,
    pub description: String,
    pub image: Option<EmbeddedImage>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Version,
    Metadata,
    Url,
    Image,
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Version => "unsupported preview version",
            Self::Metadata => "invalid preview metadata",
            Self::Url => "invalid preview URL or mismatched sender candidate",
            Self::Image => "preview image is empty, oversized, or invalid",
        })
    }
}
impl std::error::Error for Error {}

/// Syntax only: receiving metadata never authorizes a network request. Sender
/// fetch policy separately checks addresses, resolution, ports and proxy routing.
pub fn canonical_url(input: &str) -> Result<String, Error> {
    if input.is_empty()
        || input.len() > MAX_URL_BYTES
        || input.trim() != input
        || input.chars().any(char::is_control)
    {
        return Err(Error::Url);
    }
    let mut url = Url::parse(input).map_err(|_| Error::Url)?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host().is_none()
    {
        return Err(Error::Url);
    }
    if let Some(url::Host::Domain(host)) = url.host() {
        let host = host.trim_end_matches('.').to_owned();
        url.set_host(Some(&host)).map_err(|_| Error::Url)?;
    }
    Ok(url.into())
}

impl Preview {
    pub fn validate(&self) -> Result<(), Error> {
        if self.version != VERSION {
            return Err(Error::Version);
        }
        if canonical_url(&self.url).as_ref() != Ok(&self.url) {
            return Err(Error::Url);
        }
        if !inert(&self.title, MAX_TITLE_BYTES)
            || !inert(&self.description, MAX_DESCRIPTION_BYTES)
            || (self.title.is_empty() && self.description.is_empty() && self.image.is_none())
        {
            return Err(Error::Metadata);
        }
        if let Some(image) = &self.image
            && (image.data.is_empty()
                || image.data.len() > MAX_IMAGE_BYTES
                || image.width == 0
                || image.height == 0
                || image.width > MAX_IMAGE_WIDTH
                || image.height > MAX_IMAGE_HEIGHT
                || !inert(&image.alt, MAX_ALT_BYTES))
        {
            return Err(Error::Image);
        }
        Ok(())
    }
    pub fn validate_for(&self, target: &str) -> Result<(), Error> {
        self.validate()?;
        if self.url != canonical_url(target)? {
            return Err(Error::Url);
        }
        Ok(())
    }
}

fn inert(text: &str, limit: usize) -> bool {
    text.len() <= limit
        && !text.chars().any(|ch| {
            ch.is_control() || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}')
        })
}

mod image_bytes {
    use super::*;
    use serde::{Deserializer, Serializer, de::Error as _};
    pub fn serialize<S: Serializer>(data: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(data))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
            return Err(D::Error::custom("preview image is oversized"));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(value)
            .map_err(|_| D::Error::custom("invalid preview image encoding"))?;
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(D::Error::custom("preview image is oversized"));
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample_preview() -> Preview {
        Preview {
            version: VERSION,
            provider: ProviderId::OpenGraphV1,
            url: "https://example.org/".into(),
            title: "A title".into(),
            description: "A description".into(),
            image: None,
        }
    }
    #[test]
    fn carried_data_has_a_version_and_cannot_change_the_actual_url() {
        let mut preview = sample_preview();
        let target = "https://example.org/";
        preview.validate_for(target).unwrap();
        preview.url = "https://different.example/".into();
        assert_eq!(preview.validate_for(target), Err(Error::Url));
        preview.version += 1;
        assert_eq!(preview.validate(), Err(Error::Version));
        assert!(
            !ProviderId::OpenGraphV1
                .capabilities()
                .follows_preview_relation
        );
    }
    #[test]
    fn images_are_embedded_bounded_bytes_and_strings_are_not_terminal_instructions() {
        let mut preview = sample_preview();
        preview.image = Some(EmbeddedImage {
            mime: ImageMime::Png,
            data: vec![0; MAX_IMAGE_BYTES + 1],
            width: 1,
            height: 1,
            alt: String::new(),
        });
        assert_eq!(preview.validate(), Err(Error::Image));
        preview.image = None;
        preview.title = "\x1b]52;clipboard".into();
        assert_eq!(preview.validate(), Err(Error::Metadata));
        let encoded = serde_json::to_string(&sample_preview()).unwrap();
        assert_eq!(
            serde_json::from_str::<Preview>(&encoded).unwrap(),
            sample_preview()
        );
        assert!(serde_json::from_str::<Preview>(r#"{"version":1,"provider":"rfc6903","url":"https://example.org/","title":"x","description":"","image":null}"#).is_err());
    }
}
