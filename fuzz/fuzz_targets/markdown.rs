// SPDX-License-Identifier: EUPL-1.2
//! Markdown in messages (docs/markdown-rendering-plan.md): hostile text
//! through the content worker's one shared parse. What is hidden is only the
//! syntax of a construct the parse reports there; the document shows the
//! text with exactly that left out; nothing hidden touches a formula, code, a
//! bare URL or raw HTML; formulas are shown as written; links go only to
//! allowed targets, within their limits; text without syntax shows as typed;
//! and every line break typed is in the document or on a syntax-only line
//! (`nuschl_content::markdown::check`). The same holds for the text stored
//! for stock clients' HTML, where directives may add lines but never take one.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuschl_content::{ContentRenderer, markdown};

thread_local! {
    static RENDERER: ContentRenderer = ContentRenderer::default();
}

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    RENDERER.with(|renderer| {
        if let Ok(document) = renderer.render(source) {
            if let Err(error) = markdown::check(source, &document) {
                panic!("{source:?}: {error}\n{document:?}");
            }
        }
        if let Ok(stored) = renderer.legacy_plain_text(source)
            && let Ok(document) = renderer.render(&stored)
            && let Err(error) = markdown::check(&stored, &document)
        {
            panic!("{stored:?}: {error}\n{document:?}");
        }
    });
});
