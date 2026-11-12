// SPDX-License-Identifier: EUPL-1.2
//! Hostile legacy HTML and plain text through the content worker's conversion
//! into a document, as messages, comments and channel descriptions take it:
//! nothing may panic, every span must lie inside its text, in order, with an
//! allowed link target, and blocks and spans stay within the worker's limits.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuschl_content::{ContentRenderer, MAX_URL_BYTES};
use nuschl_core::{ContentBlock, EMBED, MAX_CONTENT_BYTES, MessageDocument};

thread_local! {
    // Loads syntect's syntaxes once, on the first code block, as one content
    // worker does for all its requests. Not `Sync` (it loads lazily), so it
    // is per thread; libFuzzer runs inputs on one.
    static RENDERER: ContentRenderer = ContentRenderer::default();
}

fn check(document: &MessageDocument) {
    // The worker's limits: 512 blocks, with room for the eight images and
    // the text around them, and 2,048 styled spans, which links may split.
    assert!(
        document.blocks.len() <= 600,
        "{} blocks",
        document.blocks.len()
    );
    let spans: usize = document
        .blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text { spans, .. } => spans.len(),
            _ => 0,
        })
        .sum();
    assert!(
        spans <= 2048 + 3 * nuschl_content::MAX_LINKS,
        "{spans} spans"
    );
    for block in &document.blocks {
        let ContentBlock::Text { text, spans, .. } = block else {
            continue;
        };
        assert!(!text.contains(EMBED), "a marker was left in the text");
        let mut end = 0;
        for span in spans {
            assert!(end <= span.start && span.start < span.end && span.end <= text.len());
            assert!(text.is_char_boundary(span.start) && text.is_char_boundary(span.end));
            if let Some(url) = &span.link {
                assert!(nuschl_core::allowed_link(url) && url.len() <= MAX_URL_BYTES);
                assert!(!url.contains(|c: char| c.is_control() || c.is_whitespace()));
            }
            end = span.end;
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    RENDERER.with(|renderer| run(renderer, source));
});

fn run(renderer: &ContentRenderer, source: &str) {
    // Legacy HTML becomes the text history stores, which renders as a
    // document with the text's lines (`markdown::check`, its line invariant
    // included).
    if let Ok(stored) = renderer.legacy_plain_text(source) {
        assert!(stored.len() <= MAX_CONTENT_BYTES);
        if let Ok(document) = renderer.render(&stored) {
            check(&document);
            if let Err(error) = nuschl_content::markdown::check(&stored, &document) {
                panic!("{stored:?}: {error}");
            }
        }
    }
    // Plain messages (and stored text) render directly.
    if let Ok(document) = renderer.render(source) {
        check(&document);
    }
}
