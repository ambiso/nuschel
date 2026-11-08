// SPDX-License-Identifier: EUPL-1.2
use std::io;

use nuschel_content::ContentRenderer;
use nuschel_ipc::{ContentRequest, ContentResponse, Envelope, read_frame, write_frame};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let renderer = ContentRenderer::new()?;
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    while let Some(request) = read_frame::<ContentRequest>(&mut input)? {
        let payload = match renderer.render(&request.payload.text) {
            Ok(document) => ContentResponse::Rendered(document),
            Err(error) => ContentResponse::Rejected(error.to_string()),
        };
        write_frame(&mut output, &Envelope::new(request.request_id, payload))?;
    }
    Ok(())
}
