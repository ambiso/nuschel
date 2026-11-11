// SPDX-License-Identifier: EUPL-1.2
//! Hostile LaTeX through the math worker's whole path: the converter to
//! Typst math, the check of what it wrote, Typst in its restricted world,
//! and the rasteriser. Nothing may panic; the converter may never write
//! what its own check refuses ("internal: …"); a rendered formula stays
//! within the size limits and its mask matches its size. The first byte
//! picks inline or display and the size.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuschel_math::{Error, MAX_HEIGHT, MAX_PIXELS, MAX_WIDTH, Renderer, latex};

thread_local! {
    // One per worker, as the math worker keeps one.
    static RENDERER: Renderer = Renderer::new();
}

fuzz_target!(|data: &[u8]| {
    let Some((&first, rest)) = data.split_first() else {
        return;
    };
    let Ok(tex) = std::str::from_utf8(rest) else {
        return;
    };
    match latex::to_typst(tex) {
        Ok(typst) => latex::check(&typst).expect("the converter's output passes its check"),
        Err(latex::Error(reason)) => assert!(!reason.starts_with("internal"), "{reason}"),
    }
    let display = first & 1 == 1;
    let size = 8.0 + f32::from(first >> 1) / 2.0;
    RENDERER.with(|renderer| match renderer.render(tex, display, size) {
        Ok(formula) => {
            assert!(formula.width <= MAX_WIDTH && formula.height <= MAX_HEIGHT);
            assert!(formula.width * formula.height <= MAX_PIXELS);
            assert_eq!(formula.coverage.len(), (formula.width * formula.height) as usize);
            assert!(formula.baseline <= formula.height);
        }
        Err(Error::Latex(reason)) => assert!(!reason.starts_with("internal"), "{reason}"),
        Err(Error::Typst(_) | Error::TooLarge) => {}
        Err(Error::Size) => panic!("sizes from 8 to 71.5 are valid"),
    });
});
