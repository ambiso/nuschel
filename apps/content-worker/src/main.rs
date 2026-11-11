// SPDX-License-Identifier: EUPL-1.2
use std::io;

use base64::Engine as _;
use nuschel_content::ContentRenderer;
use nuschel_core::{ContentBlock, MessageDocument};
use nuschel_ipc::{
    ContentRequest, ContentResponse, Envelope, PreparedContent, read_frame, write_frame,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
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
            image(nuschel_content::decode_image(&base64(&encoded)?, 96, 96))
        }
        ContentRequest::View { uri } => image(nuschel_content::decode_view(&uri)),
        ContentRequest::EncodeImage { image, limit } => {
            nuschel_content::encode_image_message(&base64(&image)?, limit as usize)
                .map(ContentResponse::Encoded)
                .map_err(|error| error.to_string())
        }
    }
}
