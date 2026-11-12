// SPDX-License-Identifier: EUPL-1.2
use std::io;

use base64::Engine as _;
use nuschl_content::ContentRenderer;
use nuschl_core::{ContentBlock, MessageDocument};
use nuschl_ipc::{
    ContentRequest, ContentResponse, EncodedImage, Envelope, PreparedContent, read_frame,
    write_frame,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // On Windows, the launch stage starts this worker again in its AppContainer.
    #[cfg(windows)]
    nuschl_sandbox_windows::launch_if_requested();
    let renderer = ContentRenderer::new()?;
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    while let Some(request) = read_frame::<ContentRequest>(&mut input)? {
        let response = answer(&renderer, request.payload).unwrap_or_else(ContentResponse::Rejected);
        write_frame(&mut output, &Envelope::new(request.request_id, response))?;
    }
    Ok(())
}

fn answer(renderer: &ContentRenderer, request: ContentRequest) -> Result<ContentResponse, String> {
    let image = |block: Option<ContentBlock>| {
        block
            .map(|image| {
                ContentResponse::Rendered(MessageDocument {
                    blocks: vec![image],
                })
            })
            .ok_or_else(|| "unsupported or invalid image".to_owned())
    };
    let base64 = |text: &str| {
        base64::engine::general_purpose::STANDARD
            .decode(text)
            .map_err(|_| "invalid base64".to_owned())
    };
    match request {
        ContentRequest::Render { text } => renderer
            .render(&text)
            .map(ContentResponse::Rendered)
            .map_err(|error| error.to_string()),
        ContentRequest::LegacyHtml { html } => renderer
            .legacy_plain_text(&html)
            .and_then(|source| {
                let document = renderer.render(&source)?;
                Ok(ContentResponse::Prepared(PreparedContent {
                    source,
                    document,
                }))
            })
            .map_err(|error| error.to_string()),
        ContentRequest::Avatar { image: encoded } => {
            image(nuschl_content::decode_image(&base64(&encoded)?, 96, 96))
        }
        ContentRequest::Thumbnail {
            image: encoded,
            max_width,
            max_height,
        } => {
            if max_width == 0
                || max_height == 0
                || max_width > nuschl_ipc::MAX_THUMBNAIL_EDGE
                || max_height > nuschl_ipc::MAX_THUMBNAIL_EDGE
            {
                return Err("thumbnail dimensions exceed their budget".to_owned());
            }
            if encoded.len() > nuschl_ipc::MAX_THUMBNAIL_INPUT_BYTES.div_ceil(3) * 4 {
                return Err("thumbnail input exceeds its budget".to_owned());
            }
            let bytes = base64(&encoded)?;
            if bytes.len() > nuschl_ipc::MAX_THUMBNAIL_INPUT_BYTES {
                return Err("thumbnail input exceeds its budget".to_owned());
            }
            image(nuschl_content::decode_image(&bytes, max_width, max_height))
        }
        ContentRequest::Icon { image: encoded } => {
            image(nuschl_content::decode_icon(&base64(&encoded)?))
        }
        ContentRequest::View { uri } => image(nuschl_content::decode_view(&uri)),
        ContentRequest::EncodeImage { image, limit } => {
            nuschl_content::encode_image_message(&base64(&image)?, limit as usize)
                .map(|(html, thumbnail)| ContentResponse::Encoded(EncodedImage { html, thumbnail }))
                .map_err(|error| error.to_string())
        }
    }
}

#[cfg(test)]
mod thumbnail_tests {
    use super::*;
    #[test]
    fn thumbnail_bounds_invalid_input_and_exact_pixels() {
        let renderer = ContentRenderer::new().unwrap();
        let request = |image: &str, width, height| ContentRequest::Thumbnail {
            image: image.to_owned(),
            max_width: width,
            max_height: height,
        };
        let ContentResponse::Rendered(document) = answer(&renderer, request("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGM4EaABAAMkAUEdGZ/EAAAAAElFTkSuQmCC", 1, 1)).unwrap()
            else { panic!("expected pixels"); };
        assert_eq!(document.blocks.len(), 1);
        let ContentBlock::Image {
            width,
            height,
            rgba,
            source,
        } = &document.blocks[0]
        else {
            panic!("expected one Image");
        };
        assert_eq!((*width, *height), (1, 1));
        assert!(source.is_none());
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(rgba)
                .unwrap(),
            [200, 80, 40, 255]
        );
        for (image, width, height) in [
            (
                "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGM4EaABAAMkAUEdGZ/EAAAAAElFTkSuQmCC",
                0,
                1,
            ),
            (
                "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGM4EaABAAMkAUEdGZ/EAAAAAElFTkSuQmCC",
                401,
                1,
            ),
            ("!!not-base64", 300, 300),
            ("", 300, 300),
            (
                "iVBORw0KGgoAAAANSUhEUgAAE4gAAAABCAIAAAC9c9PfAAAAJUlEQVR4nO3BgQAAAADDoPlTX+EAVQEAAAAAAAAAAAAAAAAAwGE6mQAB52yShgAAAABJRU5ErkJggg==",
                300,
                300,
            ),
        ] {
            assert!(answer(&renderer, request(image, width, height)).is_err());
        }
        let oversized = "A".repeat(nuschl_ipc::MAX_THUMBNAIL_INPUT_BYTES.div_ceil(3) * 4 + 1);
        assert!(answer(&renderer, request(&oversized, 300, 300)).is_err());
    }
}
