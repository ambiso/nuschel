// SPDX-License-Identifier: EUPL-1.2
//! The grammar of formulas in messages (docs/math.md): hostile text through
//! the content worker's parse. Every formula token is exactly the bytes typed
//! between its delimiters; none comes from what is code when the text is
//! parsed without math (code spans, fenced and indented code) or overlaps a
//! URL the linkifier links; the document
//! shows the tokens in order, as typed; and its text, formulas included, is
//! the message's without exactly the Markdown syntax it hides.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuschl_content::{ContentRenderer, bare_urls, markdown, math};
use nuschl_core::{ContentBlock, EMBED};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};

thread_local! {
    static RENDERER: ContentRenderer = ContentRenderer::default();
}

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    let tokens = math::tokens(source);
    // What is code without math.
    let mut code = Vec::new();
    let mut block = None;
    for (event, range) in Parser::new(source).into_offset_iter() {
        match event {
            Event::Code(_) => code.push(range),
            Event::Start(Tag::CodeBlock(_)) => block = Some(range.start),
            Event::End(TagEnd::CodeBlock) => {
                code.extend(block.take().map(|start| start..range.end))
            }
            _ => {}
        }
    }
    let urls = bare_urls(source);
    let mut end = 0;
    for token in &tokens {
        assert!(end <= token.range.start, "tokens are in order and disjoint");
        end = token.range.end;
        let delimiter = if token.display { "$$" } else { "$" };
        let written = &source[token.range.clone()];
        let tex = token.tex(source);
        assert_eq!(written, format!("{delimiter}{tex}{delimiter}"));
        assert!(!tex.trim().is_empty() && !tex.contains('`'));
        for code in &code {
            assert!(
                token.range.end <= code.start || code.end <= token.range.start,
                "{written:?} overlaps code at {code:?}"
            );
        }
        for (link, _) in &urls {
            assert!(
                token.range.end <= link.start || link.end <= token.range.start,
                "{written:?} overlaps the link at {link:?}"
            );
        }
    }
    RENDERER.with(|renderer| {
        let Ok(document) = renderer.render(source) else {
            return;
        };
        // The formulas shown, as written, in order; fenced math aside.
        let shown: Vec<&str> = document
            .blocks
            .iter()
            .flat_map(|block| match block {
                ContentBlock::Text { text, spans, .. } => spans
                    .iter()
                    .filter(|span| span.math.is_some())
                    .map(|span| &text[span.start..span.end])
                    .collect(),
                ContentBlock::Math { source, .. } if source.starts_with("$$") => {
                    vec![source.as_str()]
                }
                _ => Vec::new(),
            })
            .collect();
        // U+FFFC is the content worker's marker: what follows one until the
        // next is markup, not text, and formulas there are not shown.
        if source.contains(EMBED) {
            // `render` reads formulas in the visible text, between directives.
            let visible = markdown::Marked::new(source).visible.into_owned();
            let visible_tokens = math::tokens(&visible);
            let mut rest = visible_tokens
                .iter()
                .map(|token| &visible[token.range.clone()]);
            for shown in &shown {
                assert!(rest.any(|token| token == *shown), "{shown:?} out of order");
            }
            assert!(shown.len() <= visible_tokens.len().min(math::MAX_FORMULAS));
        } else {
            for (shown, token) in shown.iter().zip(&tokens) {
                assert_eq!(*shown, &source[token.range.clone()]);
            }
            assert!(shown.len() <= tokens.len().min(math::MAX_FORMULAS));
        }
        for block in &document.blocks {
            match block {
                ContentBlock::Text { text, spans, .. } => {
                    for span in spans {
                        if let Some(tex) = &span.math {
                            let written = &text[span.start..span.end];
                            assert!(written == format!("${tex}$"), "{written:?} for {tex:?}");
                        }
                    }
                }
                ContentBlock::Math {
                    source: written,
                    tex,
                } if written.starts_with("$$") => {
                    assert_eq!(*written, format!("$${tex}$$"));
                }
                _ => {}
            }
        }
        // The document's text, formulas as written, is the message's with
        // exactly its hidden Markdown syntax left out (and the other
        // Markdown invariants, `markdown::check`).
        if let Err(error) = markdown::check(source, &document) {
            panic!("{source:?}: {error}");
        }
    });
});
