// SPDX-License-Identifier: EUPL-1.2
//! Bounded carried metadata. These conversions grant no fetching or rendering
//! authority; encoded images are opaque until the content worker checks them.
use super::{MAX_CONTENT_BYTES, v1};
use nuschl_core::preview as model;
use prost::Message as _;

/// All metadata fields at their domain limits, including one 32 KiB image.
pub const MAX_PREVIEW_BYTES: usize = 38 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("invalid carried preview: {0}")]
    Invalid(#[from] model::Error),
    #[error("preview or combined message body exceeds its byte limit")]
    TooLarge,
}

/// Check the wire cap before copying any metadata or parsing the URL. This
/// validates advertised dimensions and byte limits, not the encoded image.
pub fn to_model(value: &v1::Preview) -> Result<model::Preview, Error> {
    if value.encoded_len() > MAX_PREVIEW_BYTES {
        return Err(Error::TooLarge);
    }
    if value.version != u32::from(model::VERSION) {
        return Err(model::Error::Version.into());
    }
    let provider = match v1::preview::Provider::try_from(value.provider) {
        Ok(v1::preview::Provider::OpenGraphV1) => model::ProviderId::OpenGraphV1,
        _ => return Err(model::Error::Version.into()),
    };
    let image = match &value.image {
        Some(image) => {
            let mime = match v1::preview_image::Mime::try_from(image.mime) {
                Ok(v1::preview_image::Mime::Png) => model::ImageMime::Png,
                Ok(v1::preview_image::Mime::Jpeg) => model::ImageMime::Jpeg,
                _ => return Err(model::Error::Image.into()),
            };
            Some(model::EmbeddedImage {
                mime,
                data: image.data.clone(),
                width: image.width,
                height: image.height,
                alt: image.alt.clone(),
            })
        }
        None => None,
    };
    let preview = model::Preview {
        version: model::VERSION,
        provider,
        url: value.url.clone(),
        title: value.title.clone(),
        description: value.description.clone(),
        image,
    };
    preview.validate()?;
    Ok(preview)
}

/// Reject invalid domain data before copying it into a wire value.
pub fn from_model(value: &model::Preview) -> Result<v1::Preview, Error> {
    value.validate()?;
    let preview = v1::Preview {
        version: u32::from(value.version),
        provider: match value.provider {
            model::ProviderId::OpenGraphV1 => v1::preview::Provider::OpenGraphV1 as i32,
        },
        url: value.url.clone(),
        title: value.title.clone(),
        description: value.description.clone(),
        image: value.image.as_ref().map(|image| v1::PreviewImage {
            mime: match image.mime {
                model::ImageMime::Png => v1::preview_image::Mime::Png as i32,
                model::ImageMime::Jpeg => v1::preview_image::Mime::Jpeg as i32,
            },
            data: image.data.clone(),
            width: image.width,
            height: image.height,
            alt: image.alt.clone(),
        }),
    };
    if preview.encoded_len() > MAX_PREVIEW_BYTES {
        return Err(Error::TooLarge);
    }
    Ok(preview)
}

/// The preview shares the existing text budget. Callers still validate text,
/// format, capability and the preview itself; this is only byte accounting.
pub fn check_body_size(content: &str, preview: Option<&v1::Preview>) -> Result<(), Error> {
    let size = preview.map_or(0, |preview| preview.encoded_len());
    if size > MAX_PREVIEW_BYTES || content.len() > MAX_CONTENT_BYTES.saturating_sub(size) {
        return Err(Error::TooLarge);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> model::Preview {
        model::Preview {
            version: model::VERSION,
            provider: model::ProviderId::OpenGraphV1,
            url: "https://example.org/".into(),
            title: "A title".into(),
            description: "A description".into(),
            image: None,
        }
    }

    #[test]
    fn maximum_metadata_and_opaque_image_round_trip_without_changing_bytes() {
        let mut value = example();
        value
            .url
            .push_str(&"a".repeat(model::MAX_URL_BYTES - value.url.len()));
        value.title = "é".repeat(model::MAX_TITLE_BYTES / 2);
        value.description = "é".repeat(model::MAX_DESCRIPTION_BYTES / 2);
        value.image = Some(model::EmbeddedImage {
            mime: model::ImageMime::Jpeg,
            // No image parser runs here; actual decode is a worker admission.
            data: vec![0xab; model::MAX_IMAGE_BYTES],
            width: model::MAX_IMAGE_WIDTH,
            height: model::MAX_IMAGE_HEIGHT,
            alt: "é".repeat(model::MAX_ALT_BYTES / 2),
        });
        let wire = from_model(&value).unwrap();
        assert!(wire.encoded_len() <= MAX_PREVIEW_BYTES);
        let decoded = v1::Preview::decode(wire.encode_to_vec().as_slice()).unwrap();
        assert_eq!(to_model(&decoded).unwrap(), value);
    }

    #[test]
    fn unknown_versions_providers_and_image_mimes_are_not_silently_reinterpreted() {
        let wire = from_model(&example()).unwrap();
        for version in [0, 2, u32::MAX] {
            let mut changed = wire.clone();
            changed.version = version;
            assert_eq!(to_model(&changed), Err(model::Error::Version.into()));
        }
        for provider in [0, 2, i32::MAX] {
            let mut changed = wire.clone();
            changed.provider = provider;
            assert_eq!(to_model(&changed), Err(model::Error::Version.into()));
        }
        for mime in [0, 3, i32::MAX] {
            let mut changed = wire.clone();
            changed.image = Some(v1::PreviewImage {
                mime,
                data: vec![1],
                width: 1,
                height: 1,
                alt: String::new(),
            });
            assert_eq!(to_model(&changed), Err(model::Error::Image.into()));
        }
    }

    #[test]
    fn domain_validation_uses_utf8_bytes_and_canonical_urls_in_both_directions() {
        let mut value = example();
        value.title = "é".repeat(model::MAX_TITLE_BYTES / 2);
        assert!(from_model(&value).is_ok());
        value.title.push('é');
        assert_eq!(from_model(&value), Err(model::Error::Metadata.into()));
        let mut wire = from_model(&example()).unwrap();
        wire.title = value.title;
        assert_eq!(to_model(&wire), Err(model::Error::Metadata.into()));
        for url in [
            "https://user:password@example.org/",
            "https://EXAMPLE.org/",
            "https://example.org/\n",
        ] {
            value = example();
            value.url = url.into();
            assert_eq!(from_model(&value), Err(model::Error::Url.into()));
            wire = from_model(&example()).unwrap();
            wire.url = url.into();
            assert_eq!(to_model(&wire), Err(model::Error::Url.into()));
        }
    }

    #[test]
    fn wire_size_precedes_url_validation_and_domain_image_caps_are_enforced() {
        let mut wire = from_model(&example()).unwrap();
        wire.url = "not a URL".into();
        wire.title = "x".repeat(MAX_PREVIEW_BYTES + 1);
        assert_eq!(to_model(&wire), Err(Error::TooLarge));
        assert_eq!(check_body_size("", Some(&wire)), Err(Error::TooLarge));
        let mut value = example();
        value.image = Some(model::EmbeddedImage {
            mime: model::ImageMime::Png,
            data: vec![1; model::MAX_IMAGE_BYTES + 1],
            width: 1,
            height: 1,
            alt: String::new(),
        });
        assert_eq!(from_model(&value), Err(model::Error::Image.into()));
        wire = from_model(&example()).unwrap();
        wire.image = Some(v1::PreviewImage {
            mime: v1::preview_image::Mime::Png as i32,
            data: vec![1; model::MAX_IMAGE_BYTES + 1],
            width: 1,
            height: 1,
            alt: String::new(),
        });
        assert_eq!(to_model(&wire), Err(model::Error::Image.into()));
    }

    #[test]
    fn combined_body_cap_preserves_the_complete_original_text_budget() {
        let wire = from_model(&example()).unwrap();
        let remaining = MAX_CONTENT_BYTES - wire.encoded_len();
        let mut text = "é".repeat(remaining / 2);
        if !remaining.is_multiple_of(2) {
            text.push('x');
        }
        assert_eq!(text.len(), remaining);
        assert_eq!(check_body_size(&text, Some(&wire)), Ok(()));
        text.push('x');
        assert_eq!(check_body_size(&text, Some(&wire)), Err(Error::TooLarge));
        let mut plain = "x".repeat(MAX_CONTENT_BYTES);
        assert_eq!(check_body_size(&plain, None), Ok(()));
        plain.push('x');
        assert_eq!(check_body_size(&plain, None), Err(Error::TooLarge));
    }

    // The exact six-field ChatSend schema before preview/1. Keeping this
    // decoder independent catches accidental tag reuse or text replacement.
    #[derive(Clone, PartialEq, prost::Message)]
    struct PreviousSend {
        #[prost(uint64, tag = "1")]
        request_id: u64,
        #[prost(bytes = "vec", tag = "2")]
        conversation: Vec<u8>,
        #[prost(bytes = "vec", tag = "3")]
        idempotency_key: Vec<u8>,
        #[prost(int32, tag = "4")]
        format: i32,
        #[prost(string, tag = "5")]
        content: String,
        #[prost(uint64, tag = "6")]
        reply_to: u64,
    }

    #[test]
    fn previous_send_decoder_keeps_literal_text_and_none_keeps_original_wire_bytes() {
        let old = PreviousSend {
            request_id: 7,
            conversation: vec![2; 16],
            idempotency_key: vec![3; 16],
            format: v1::Format::Text as i32,
            content: "literal <text>\n{\"preview\": \"ordinary text\"} https://example.org/ "
                .into(),
            reply_to: 4,
        };
        let bytes = old.encode_to_vec();
        let mut send = v1::ChatSend::decode(bytes.as_slice()).unwrap();
        assert!(send.preview.is_none());
        assert_eq!(send.encode_to_vec(), bytes);
        send.preview = Some(from_model(&example()).unwrap());
        let attached = send.encode_to_vec();
        assert_eq!(PreviousSend::decode(attached.as_slice()).unwrap(), old);
        assert!(
            v1::ChatSend::decode(attached.as_slice())
                .unwrap()
                .preview
                .is_some()
        );

        // Previous ChatEdit and Message encodings, with text "x" and format1.
        let edit_bytes = b"\x08\x01\x20\x01\x2a\x01x";
        let edit = v1::ChatEdit::decode(edit_bytes.as_slice()).unwrap();
        assert!(edit.preview.is_none());
        assert_eq!(edit.encode_to_vec().as_slice(), edit_bytes.as_slice());
        let message_bytes = b"\x08\x01\x28\x01\x32\x01x";
        let message = v1::Message::decode(message_bytes.as_slice()).unwrap();
        assert!(message.preview.is_none());
        assert_eq!(message.encode_to_vec().as_slice(), message_bytes.as_slice());
    }

    #[test]
    fn direct_and_nested_wire_debug_never_print_carried_metadata_or_image_bytes() {
        let mut value = example();
        value.url = "https://example.org/private-path-token".into();
        value.title = "private-title-token".into();
        value.description = "private-description-token".into();
        value.image = Some(model::EmbeddedImage {
            mime: model::ImageMime::Png,
            data: b"private-image-token".to_vec(),
            width: 1,
            height: 1,
            alt: "private-alt-token".into(),
        });
        let wire = from_model(&value).unwrap();
        let send = v1::ChatSend {
            preview: Some(wire.clone()),
            ..Default::default()
        };
        let edit = v1::ChatEdit {
            preview: Some(wire.clone()),
            ..Default::default()
        };
        let message = v1::Message {
            preview: Some(wire.clone()),
            ..Default::default()
        };
        let printed = format!("{wire:?} {:?} {send:?} {edit:?} {message:?}", wire.image);
        for secret in [
            "private-path-token",
            "private-title-token",
            "private-description-token",
            "private-alt-token",
            "112, 114, 105, 118",
        ] {
            assert!(!printed.contains(secret), "carried data leaked in Debug");
        }
        assert!(printed.contains("bytes redacted"));
    }
}
