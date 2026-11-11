// SPDX-License-Identifier: EUPL-1.2
use std::io;

use nuschel_content::ContentRenderer;
use nuschel_ipc::{
    ContentRequest, ContentResponse, Envelope, PreparedContent, read_frame, write_frame,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let renderer = ContentRenderer::new()?;
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    while let Some(request) = read_frame::<ContentRequest>(&mut input)? {
        let prepared = if request.payload.avatar {
            use base64::Engine as _;
            Ok(base64::engine::general_purpose::STANDARD
                .decode(&request.payload.text)
                .ok()
                .and_then(|bytes| nuschel_content::decode_image(&bytes, 96, 96))
                .map(|image| {
                    ContentResponse::Rendered(nuschel_core::MessageDocument {
                        blocks: vec![image],
                    })
                })
                .unwrap_or_else(|| {
                    ContentResponse::Rejected("unsupported or invalid avatar image".into())
                }))
        } else if request.payload.view {
            Ok(nuschel_content::decode_view(&request.payload.text)
                .map(|image| {
                    ContentResponse::Rendered(nuschel_core::MessageDocument {
                        blocks: vec![image],
                    })
                })
                .unwrap_or_else(|| {
                    ContentResponse::Rejected("unsupported or invalid image".into())
                }))
        } else if request.payload.legacy_html {
            renderer
                .legacy_plain_text(&request.payload.text)
                .and_then(|source| {
                    renderer.render(&source).map(|document| {
                        ContentResponse::Prepared(PreparedContent { source, document })
                    })
                })
        } else {
            renderer
                .render(&request.payload.text)
                .map(ContentResponse::Rendered)
        };
        let payload = match prepared {
            Ok(response) => response,
            Err(error) => ContentResponse::Rejected(error.to_string()),
        };
        write_frame(&mut output, &Envelope::new(request.request_id, payload))?;
    }
    Ok(())
}
