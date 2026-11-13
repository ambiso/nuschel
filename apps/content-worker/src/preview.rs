// SPDX-License-Identifier: EUPL-1.2
//! Offline operations only; metadata URLs never initiate a worker fetch.
use base64::Engine as _;
use nuschl_content::preview::{self, TransportCharset};
use nuschl_ipc::preview::{
    Charset, MAX_DOCUMENT_BYTES, MAX_SOURCE_IMAGE_BYTES, Metadata, Pixels, Request, Response,
    SCHEMA, Unavailable,
};

fn unavailable(reason: Unavailable) -> Response {
    Response::Unavailable {
        schema: SCHEMA,
        reason,
    }
}

fn decode(encoded: &str, cap: usize) -> Result<Vec<u8>, Unavailable> {
    if encoded.len() > cap.div_ceil(3) * 4 {
        return Err(Unavailable::TooLarge);
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| Unavailable::Malformed)?;
    if bytes.len() > cap {
        return Err(Unavailable::TooLarge);
    }
    Ok(bytes)
}

fn failure(reason: preview::Unavailable) -> Unavailable {
    match reason {
        preview::Unavailable::NoMetadata => Unavailable::NoMetadata,
        preview::Unavailable::Encoding => Unavailable::Encoding,
        preview::Unavailable::TooLarge => Unavailable::TooLarge,
        _ => Unavailable::Malformed,
    }
}

pub fn answer(request: Request) -> Response {
    let schema = match &request {
        Request::Document { schema, .. }
        | Request::PrepareImage { schema, .. }
        | Request::RenderImage { schema, .. } => *schema,
    };
    if schema != SCHEMA {
        return unavailable(Unavailable::Version);
    }
    if !request.is_bounded() {
        return unavailable(Unavailable::TooLarge);
    }
    let result = (|| match request {
        Request::Document {
            body,
            charset,
            truncated,
            ..
        } => {
            if truncated {
                return Err(Unavailable::TooLarge);
            }
            let body = decode(&body, MAX_DOCUMENT_BYTES)?;
            let charset = match charset {
                Charset::Utf8 => TransportCharset::Utf8,
                Charset::Unspecified => TransportCharset::Unspecified,
                Charset::Other => TransportCharset::Other,
            };
            let found = preview::extract_open_graph(&body, charset).map_err(failure)?;
            Ok(Response::Metadata {
                schema: SCHEMA,
                metadata: Metadata {
                    title: found.title,
                    description: found.description,
                    image: found.image,
                    image_alt: found.image_alt,
                },
            })
        }
        Request::PrepareImage { image, alt, .. } => {
            let bytes = decode(&image, MAX_SOURCE_IMAGE_BYTES)?;
            let image = preview::prepare_image(&bytes, &alt).map_err(failure)?;
            Ok(Response::Image {
                schema: SCHEMA,
                image,
            })
        }
        Request::RenderImage { image, .. } => {
            let rgba = preview::render_image(&image).map_err(failure)?;
            Ok(Response::Pixels {
                schema: SCHEMA,
                pixels: Pixels {
                    width: image.width,
                    height: image.height,
                    rgba: base64::engine::general_purpose::STANDARD.encode(rgba),
                },
            })
        }
    })();
    match result {
        Ok(response) if response.is_bounded() => response,
        Ok(_) => unavailable(Unavailable::Malformed),
        Err(reason) => unavailable(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGM4EaABAAMkAUEdGZ/EAAAAAElFTkSuQmCC";

    #[test]
    fn extraction_is_bounded_inert_and_truncation_refuses_partial_metadata() {
        let request = |body: &str, truncated| Request::Document {
            schema: SCHEMA,
            body: base64::engine::general_purpose::STANDARD.encode(body),
            charset: Charset::Utf8,
            truncated,
        };
        let Response::Metadata { metadata, .. } = answer(request(
            "<meta property=og:title content='A &amp; B'><meta property=og:image content=/image.png>",
            false,
        )) else {
            panic!("expected metadata");
        };
        assert_eq!(metadata.title, "A & B");
        assert_eq!(metadata.image.as_deref(), Some("/image.png"));
        assert!(metadata.is_bounded());
        assert_eq!(
            answer(request("<title>partial</title>", true)),
            unavailable(Unavailable::TooLarge)
        );
        assert_eq!(
            answer(Request::Document {
                schema: SCHEMA,
                body: "!!!!".into(),
                charset: Charset::Utf8,
                truncated: false
            }),
            unavailable(Unavailable::Malformed)
        );
        assert_eq!(
            answer(Request::Document {
                schema: SCHEMA,
                body: "A".repeat(MAX_DOCUMENT_BYTES.div_ceil(3) * 4 + 1),
                charset: Charset::Utf8,
                truncated: false
            }),
            unavailable(Unavailable::TooLarge)
        );
    }

    #[test]
    fn preparation_and_exact_carried_decode_return_the_same_source_free_pixels() {
        let Response::Image { image, .. } = answer(Request::PrepareImage {
            schema: SCHEMA,
            image: PNG.into(),
            alt: " A\nsmall image ".into(),
        }) else {
            panic!("expected prepared image");
        };
        assert_eq!(image.alt, "A small image");
        assert_eq!(
            answer(Request::RenderImage {
                schema: SCHEMA,
                image: image.clone()
            }),
            Response::Pixels {
                schema: SCHEMA,
                pixels: Pixels {
                    width: 1,
                    height: 1,
                    rgba: base64::engine::general_purpose::STANDARD.encode([200, 80, 40, 255]),
                }
            }
        );
        let mut wrong = image;
        wrong.width = 2;
        assert_eq!(
            answer(Request::RenderImage {
                schema: SCHEMA,
                image: wrong
            }),
            unavailable(Unavailable::Malformed)
        );
        assert_eq!(
            answer(Request::PrepareImage {
                schema: SCHEMA,
                image: "A".repeat(MAX_SOURCE_IMAGE_BYTES.div_ceil(3) * 4 + 1),
                alt: String::new()
            }),
            unavailable(Unavailable::TooLarge)
        );
        assert_eq!(
            answer(Request::PrepareImage {
                schema: SCHEMA + 1,
                image: PNG.into(),
                alt: String::new()
            }),
            unavailable(Unavailable::Version)
        );
    }
}
