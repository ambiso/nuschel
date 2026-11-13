// SPDX-License-Identifier: EUPL-1.2
//! The preview card: what the sender sees in the composer and what one
//! message carries. Built only from validated parts; converts losslessly to
//! and from the carried model in `nuschl_core::preview`.
use crate::metadata::ExtractedMetadata;
use crate::model::{
    self, EmbeddedImage, ImageMime, MAX_ALT_BYTES, MAX_DESCRIPTION_BYTES, MAX_TITLE_BYTES, Preview,
    ProviderId,
};
use crate::policy::Target;
use crate::text::normalize;
use thiserror::Error;
use url::Url;

/// A DNS name, or an IPv6 literal with its brackets.
pub const MAX_HOST_BYTES: usize = 253;

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum CardError {
    #[error("the preview URL is invalid or is not the sender's URL")]
    Url,
    #[error("the preview has no usable title, description or image")]
    Metadata,
    #[error("the preview image is empty, oversized, mislabelled or invalid")]
    Image,
    #[error("unsupported preview version or provider")]
    Version,
}

impl From<model::Error> for CardError {
    fn from(error: model::Error) -> Self {
        match error {
            model::Error::Version => Self::Version,
            model::Error::Metadata => Self::Metadata,
            model::Error::Url => Self::Url,
            model::Error::Image => Self::Image,
        }
    }
}

/// Invariants: `url` is canonical (`model::canonical_url`), `host` is that
/// URL's host, the strings are normalised plain text within the carried
/// limits, and an image passed [`check_image`]. A title can never claim
/// another site: the host shown always comes from the sender's URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Card {
    url: String,
    host: String,
    title: String,
    description: String,
    image: Option<EmbeddedImage>,
}

impl Card {
    /// A card for the sender's typed target from extracted metadata and an
    /// optional prepared derivative, whose alternative text becomes
    /// `metadata.image_alt`. Strings are normalised again: worker replies are
    /// sanitised, not trusted. The raw image reference in `metadata` is not
    /// used. On [`CardError::Image`] the caller may build the card again
    /// without the image: a preview image never becomes an attachment.
    pub fn new(
        target: &Target,
        metadata: &ExtractedMetadata,
        image: Option<EmbeddedImage>,
    ) -> Result<Self, CardError> {
        let url = model::canonical_url(target.as_str())?;
        let image = image.map(|image| EmbeddedImage {
            alt: metadata.image_alt.clone(),
            ..image
        });
        let card = Self::build(url, &metadata.title, &metadata.description, image)?;
        card.to_preview().validate_for(target.as_str())?;
        Ok(card)
    }

    /// A card from a carried preview: the model's validation, normalised
    /// strings, an image whose bytes match its type and dimensions. No fetch
    /// policy applies: displaying carried data never authorizes a request.
    pub fn from_preview(preview: &Preview) -> Result<Self, CardError> {
        preview.validate()?;
        if preview.provider != ProviderId::OpenGraphV1 {
            return Err(CardError::Version);
        }
        Self::build(
            preview.url.clone(),
            &preview.title,
            &preview.description,
            preview.image.clone(),
        )
    }

    fn build(
        url: String,
        title: &str,
        description: &str,
        image: Option<EmbeddedImage>,
    ) -> Result<Self, CardError> {
        let host = host_of(&url)?;
        let image = match image {
            Some(mut image) => {
                image.alt = normalize(&image.alt, MAX_ALT_BYTES);
                check_image(&image)?;
                Some(image)
            }
            None => None,
        };
        let card = Self {
            url,
            host,
            title: normalize(title, MAX_TITLE_BYTES),
            description: normalize(description, MAX_DESCRIPTION_BYTES),
            image,
        };
        if card.title.is_empty() && card.description.is_empty() && card.image.is_none() {
            return Err(CardError::Metadata);
        }
        card.to_preview().validate()?;
        Ok(card)
    }

    pub fn to_preview(&self) -> Preview {
        Preview {
            version: model::VERSION,
            provider: ProviderId::OpenGraphV1,
            url: self.url.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            image: self.image.clone(),
        }
    }

    /// Whether this card belongs to the sender's typed URL.
    pub fn is_for(&self, typed: &str) -> bool {
        model::canonical_url(typed).is_ok_and(|url| url == self.url)
    }

    pub fn url(&self) -> &str {
        &self.url
    }
    pub fn host(&self) -> &str {
        &self.host
    }
    pub fn title(&self) -> &str {
        &self.title
    }
    /// The linked line of the card: the title, or the host without one.
    pub fn display_title(&self) -> &str {
        if self.title.is_empty() {
            &self.host
        } else {
            &self.title
        }
    }
    pub fn description(&self) -> &str {
        &self.description
    }
    pub fn image(&self) -> Option<&EmbeddedImage> {
        self.image.as_ref()
    }
    pub fn without_image(&self) -> Self {
        Self {
            image: None,
            ..self.clone()
        }
    }
    /// Bytes this card holds, for cache budgets.
    pub fn weight(&self) -> usize {
        self.url.len()
            + self.host.len()
            + self.title.len()
            + self.description.len()
            + self
                .image
                .as_ref()
                .map_or(0, |image| image.data.len() + image.alt.len())
    }
}

/// The host as the card shows it. The URL parser has already converted an
/// internationalised name to its ASCII (punycode) form, so look-alike
/// characters cannot imitate another site's name.
fn host_of(url: &str) -> Result<String, CardError> {
    let url = Url::parse(url).map_err(|_| CardError::Url)?;
    let host = url.host_str().ok_or(CardError::Url)?;
    if host.is_empty() || host.len() > MAX_HOST_BYTES || !host.is_ascii() {
        return Err(CardError::Url);
    }
    Ok(host.to_owned())
}

/// A carried derivative's structural checks before anyone decodes it: the
/// model's size and dimension limits, the PNG or JPEG signature, and the
/// dimensions in the image header equal to the declared ones. Full decoding
/// and the container allowlist stay in the content worker.
pub fn check_image(image: &EmbeddedImage) -> Result<(), CardError> {
    if image.data.is_empty()
        || image.data.len() > model::MAX_IMAGE_BYTES
        || image.width == 0
        || image.height == 0
        || image.width > model::MAX_IMAGE_WIDTH
        || image.height > model::MAX_IMAGE_HEIGHT
        || !crate::text::is_normalized(&image.alt, MAX_ALT_BYTES)
    {
        return Err(CardError::Image);
    }
    if image_dimensions(image.mime, &image.data) != Some((image.width, image.height)) {
        return Err(CardError::Image);
    }
    Ok(())
}

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// The dimensions an encoded image's header declares, if it is the named
/// type: PNG's IHDR, which must come first, or JPEG's first frame header.
pub fn image_dimensions(mime: ImageMime, data: &[u8]) -> Option<(u32, u32)> {
    match mime {
        ImageMime::Png => {
            let rest = data.strip_prefix(PNG_SIGNATURE)?;
            let header = rest.get(..16)?;
            if header[..8] != b"\0\0\0\x0dIHDR"[..] {
                return None;
            }
            let width = u32::from_be_bytes(header[8..12].try_into().ok()?);
            let height = u32::from_be_bytes(header[12..16].try_into().ok()?);
            Some((width, height))
        }
        ImageMime::Jpeg => {
            if !data.starts_with(b"\xff\xd8\xff") {
                return None;
            }
            let mut at = 2;
            // Each step consumes at least one byte: linear in the data.
            while at + 4 <= data.len() {
                if data[at] != 0xff {
                    return None;
                }
                let marker = data[at + 1];
                match marker {
                    // Fill byte.
                    0xff => at += 1,
                    // Markers without a length.
                    0x01 | 0xd0..=0xd8 => at += 2,
                    // Scan data or the end before any frame header.
                    0xd9 | 0xda => return None,
                    _ => {
                        let length = usize::from(u16::from_be_bytes([data[at + 2], data[at + 3]]));
                        if length < 2 {
                            return None;
                        }
                        // SOF0–SOF15, except DHT, JPG and DAC.
                        if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc)
                        {
                            let frame = data.get(at + 4..at + 9)?;
                            let height = u32::from(u16::from_be_bytes([frame[1], frame[2]]));
                            let width = u32::from(u16::from_be_bytes([frame[3], frame[4]]));
                            return Some((width, height));
                        }
                        at += 2 + length;
                    }
                }
            }
            None
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn png(width: u32, height: u32, extra: usize) -> Vec<u8> {
        let mut data = PNG_SIGNATURE.to_vec();
        data.extend_from_slice(b"\0\0\0\x0dIHDR");
        data.extend_from_slice(&width.to_be_bytes());
        data.extend_from_slice(&height.to_be_bytes());
        data.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        data.resize(data.len() + extra, 0);
        data
    }

    pub(crate) fn jpeg(width: u16, height: u16) -> Vec<u8> {
        let mut data = b"\xff\xd8\xff\xe0\x00\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0".to_vec();
        // A DHT segment before the frame header is skipped.
        data.extend_from_slice(b"\xff\xc4\x00\x03\x00");
        data.extend_from_slice(b"\xff\xc0\x00\x11\x08");
        data.extend_from_slice(&height.to_be_bytes());
        data.extend_from_slice(&width.to_be_bytes());
        data.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        data.extend_from_slice(b"\xff\xd9");
        data
    }

    pub(crate) fn image(mime: ImageMime, width: u32, height: u32) -> EmbeddedImage {
        EmbeddedImage {
            mime,
            data: match mime {
                ImageMime::Png => png(width, height, 64),
                ImageMime::Jpeg => jpeg(width as u16, height as u16),
            },
            width,
            height,
            alt: String::new(),
        }
    }

    pub(crate) fn metadata(title: &str, description: &str) -> ExtractedMetadata {
        ExtractedMetadata {
            title: title.into(),
            description: description.into(),
            image: None,
            image_alt: String::new(),
        }
    }

    pub(crate) fn card(url: &str, title: &str, description: &str) -> Card {
        Card::new(
            &Target::parse(url).unwrap(),
            &metadata(title, description),
            None,
        )
        .unwrap()
    }

    fn with_image(card: &Card, image: EmbeddedImage) -> Result<Card, CardError> {
        let target = Target::parse(card.url()).unwrap();
        let metadata = ExtractedMetadata {
            image_alt: image.alt.clone(),
            ..self::metadata(card.title(), card.description())
        };
        Card::new(&target, &metadata, Some(image))
    }

    #[test]
    fn cards_name_the_typed_host_and_round_trip_through_the_carried_model() {
        let target = Target::parse("https://Example.ORG./a?b=1#frag").unwrap();
        let card = Card::new(&target, &metadata("  A\ttitle ", "Desc\u{202e}"), None).unwrap();
        assert_eq!(card.url(), "https://example.org/a?b=1#frag");
        assert_eq!(card.host(), "example.org");
        assert_eq!(card.title(), "A title");
        assert_eq!(card.description(), "Desc");
        assert!(card.is_for("https://example.org/a?b=1#frag"));
        assert!(!card.is_for("https://example.org/a?b=2#frag"));
        let preview = card.to_preview();
        preview.validate_for(target.as_str()).unwrap();
        assert_eq!(Card::from_preview(&preview).unwrap(), card);

        let pictured = with_image(
            &card,
            EmbeddedImage {
                alt: " An\nalt ".into(),
                ..image(ImageMime::Png, 640, 360)
            },
        )
        .unwrap();
        assert_eq!(pictured.image().unwrap().alt, "An alt");
        assert_eq!(
            Card::from_preview(&pictured.to_preview()).unwrap(),
            pictured
        );
        assert_eq!(pictured.without_image(), card);
        assert!(pictured.weight() > card.weight());
    }

    #[test]
    fn hosts_are_ascii_and_titles_fall_back_to_them() {
        let card = card("https://bücher.example/", "", "Only a description");
        assert_eq!(card.host(), "xn--bcher-kva.example");
        assert_eq!(card.display_title(), "xn--bcher-kva.example");
        let card = Card::new(
            &Target::parse("https://[2606:4700::1]/").unwrap(),
            &metadata("t", ""),
            None,
        )
        .unwrap();
        assert_eq!(card.host(), "[2606:4700::1]");
        // An image alone is a card too; the alt text comes from the metadata.
        let image_only = Card::new(
            &Target::parse("https://example.org/").unwrap(),
            &ExtractedMetadata {
                image_alt: "A cat".into(),
                ..metadata("", "")
            },
            Some(EmbeddedImage {
                alt: "ignored".into(),
                ..image(ImageMime::Png, 4, 4)
            }),
        )
        .unwrap();
        assert_eq!(image_only.display_title(), "example.org");
        assert_eq!(image_only.image().unwrap().alt, "A cat");
    }

    #[test]
    fn empty_or_hostile_metadata_is_refused() {
        let target = Target::parse("https://example.org/").unwrap();
        assert_eq!(
            Card::new(&target, &metadata(" \u{202e} ", "\n\u{1b}"), None),
            Err(CardError::Metadata)
        );
        // Over-long worker strings are cut to the carried limits, not trusted.
        let card = Card::new(
            &target,
            &metadata(&"t".repeat(10_000), &"d".repeat(10_000)),
            None,
        )
        .unwrap();
        assert_eq!(card.title().len(), MAX_TITLE_BYTES);
        assert_eq!(card.description().len(), MAX_DESCRIPTION_BYTES);
        // Carried previews: another URL, version or provider-free data fails.
        let mut preview = card.to_preview();
        preview.version = 2;
        assert_eq!(Card::from_preview(&preview), Err(CardError::Version));
        let mut preview = card.to_preview();
        preview.url = "javascript:alert(1)".into();
        assert_eq!(Card::from_preview(&preview), Err(CardError::Url));
        let mut preview = card.to_preview();
        preview.title = "\u{7}".into();
        assert_eq!(Card::from_preview(&preview), Err(CardError::Metadata));
        let mut preview = card.to_preview();
        preview.title.clear();
        preview.description = "  ".into();
        assert_eq!(Card::from_preview(&preview), Err(CardError::Metadata));
    }

    #[test]
    fn images_must_match_their_type_and_dimensions() {
        let card = card("https://example.org/", "t", "");
        for (mime, width, height) in [(ImageMime::Png, 1, 1), (ImageMime::Jpeg, 640, 360)] {
            assert!(with_image(&card, image(mime, width, height)).is_ok());
        }
        let refused = [
            // Declared dimensions differ from the header.
            EmbeddedImage {
                width: 2,
                ..image(ImageMime::Png, 1, 1)
            },
            EmbeddedImage {
                height: 10,
                ..image(ImageMime::Jpeg, 20, 20)
            },
            // A JPEG labelled PNG and the reverse.
            EmbeddedImage {
                mime: ImageMime::Png,
                ..image(ImageMime::Jpeg, 1, 1)
            },
            EmbeddedImage {
                mime: ImageMime::Jpeg,
                ..image(ImageMime::Png, 1, 1)
            },
            // Over the carried limits.
            image(ImageMime::Png, 641, 1),
            image(ImageMime::Jpeg, 1, 361),
            EmbeddedImage {
                data: png(1, 1, model::MAX_IMAGE_BYTES),
                ..image(ImageMime::Png, 1, 1)
            },
            EmbeddedImage {
                data: Vec::new(),
                ..image(ImageMime::Png, 1, 1)
            },
            EmbeddedImage {
                width: 0,
                height: 0,
                data: png(0, 0, 8),
                ..image(ImageMime::Png, 1, 1)
            },
            // Truncated headers and a scan before the frame header.
            EmbeddedImage {
                data: png(1, 1, 0)[..20].to_vec(),
                ..image(ImageMime::Png, 1, 1)
            },
            EmbeddedImage {
                data: b"\xff\xd8\xff\xda\x00\x02".to_vec(),
                ..image(ImageMime::Jpeg, 1, 1)
            },
        ];
        for bad in refused {
            assert_eq!(with_image(&card, bad), Err(CardError::Image));
        }
        // A carried preview with a mislabelled image is refused as a whole.
        let mut preview = card.to_preview();
        preview.image = Some(EmbeddedImage {
            mime: ImageMime::Jpeg,
            ..image(ImageMime::Png, 1, 1)
        });
        assert_eq!(Card::from_preview(&preview), Err(CardError::Image));
    }

    #[test]
    fn jpeg_header_walks_are_bounded_on_hostile_bytes() {
        assert_eq!(image_dimensions(ImageMime::Jpeg, b"\xff\xd8\xff"), None);
        let mut fill = b"\xff\xd8".to_vec();
        fill.resize(64 * 1024, 0xff);
        assert_eq!(image_dimensions(ImageMime::Jpeg, &fill), None);
        let mut zero_length = b"\xff\xd8\xff\xe1\x00\x00".to_vec();
        zero_length.resize(64, 0);
        assert_eq!(image_dimensions(ImageMime::Jpeg, &zero_length), None);
        // A segment length past the end stops the walk.
        assert_eq!(
            image_dimensions(ImageMime::Jpeg, b"\xff\xd8\xff\xe1\xff\xff\0\0"),
            None
        );
        let mut cut = jpeg(4, 4);
        cut.truncate(cut.len() - 14);
        assert_eq!(image_dimensions(ImageMime::Jpeg, &cut), None);
    }
}
