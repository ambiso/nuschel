// SPDX-License-Identifier: EUPL-1.2
//! Renders LaTeX formulas from messages, one request at a time, for as long
//! as its input stays open (see `nuschel_ipc::math`).
use std::io;

use base64::Engine as _;
use nuschel_ipc::{
    Envelope,
    math::{MathImage, MathRequest, MathResponse},
    read_frame, write_frame,
};
use nuschel_math::Renderer;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let renderer = Renderer::new();
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    while let Some(request) = read_frame::<MathRequest>(&mut input)? {
        let MathRequest { tex, display, size } = request.payload;
        let response = match renderer.render(&tex, display, size) {
            Ok(formula) => MathResponse::Rendered(MathImage {
                width: formula.width,
                height: formula.height,
                baseline: formula.baseline,
                coverage: base64::engine::general_purpose::STANDARD.encode(formula.coverage),
            }),
            Err(error) => MathResponse::Rejected(error.to_string()),
        };
        write_frame(&mut output, &Envelope::new(request.request_id, response))?;
    }
    Ok(())
}
