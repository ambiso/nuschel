// SPDX-License-Identifier: EUPL-1.2
//! Hostile text through the copy stock Mumble clients receive
//! (`nuschel-legacy-html`, made by nuscheld for every Nuschel message and by the
//! client on stock servers), and that copy back through the content worker as
//! another Nuschel client reads it. The copy may contain only escaped text,
//! `<br/>` and links with an allowed scheme whose text is their target; it
//! must read back as exactly the input, and as the same text and links once
//! made inert again.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuschel_content::{ContentRenderer, MAX_LINKS, MAX_URL_BYTES};
use nuschel_core::EMBED;
use nuschel_legacy_html::{legacy_body, text_html};

thread_local! {
    static RENDERER: ContentRenderer = ContentRenderer::default();
}

/// The copy's text (entities and `<br/>` undone) and its links' targets,
/// asserting that nothing else is in it.
fn read(html: &str) -> (String, Vec<String>) {
    fn unescape(escaped: &str) -> String {
        let mut text = String::new();
        let mut rest = escaped;
        while let Some(at) = rest.find(['&', '<', '>', '"']) {
            text.push_str(&rest[..at]);
            rest = &rest[at..];
            let (entity, c) = [
                ("&amp;", '&'),
                ("&lt;", '<'),
                ("&gt;", '>'),
                ("&quot;", '"'),
            ]
            .into_iter()
            .find(|(entity, _)| rest.starts_with(entity))
            .unwrap_or_else(|| panic!("unescaped markup at {rest:?}"));
            text.push(c);
            rest = &rest[entity.len()..];
        }
        text + rest
    }
    let (mut text, mut links) = (String::new(), Vec::new());
    let mut rest = html;
    while let Some(at) = rest.find('<') {
        text.push_str(&unescape(&rest[..at]));
        rest = &rest[at..];
        if let Some(after) = rest.strip_prefix("<br/>") {
            text.push('\n');
            rest = after;
            continue;
        }
        let after = rest
            .strip_prefix("<a href=\"")
            .unwrap_or_else(|| panic!("an element other than a link or break: {rest:?}"));
        let (href, after) = after.split_once("\">").expect("a closed href");
        let (label, after) = after.split_once("</a>").expect("a closed link");
        let (href, label) = (unescape(href), unescape(label));
        assert!(nuschel_core::allowed_link(&href), "{href:?}");
        assert!(!href.contains(|c: char| c.is_whitespace() || c.is_control()));
        // Its text is the target, apart from the scheme a www. address gets.
        let expected = if label
            .get(..4)
            .is_some_and(|start| start.eq_ignore_ascii_case("www."))
        {
            format!("https://{label}")
        } else {
            label.clone()
        };
        assert_eq!(href, expected);
        text.push_str(&label);
        links.push(href);
        rest = after;
    }
    text.push_str(&unescape(rest));
    (text, links)
}

/// What an HTML parser cannot keep of text in a body, and leading or trailing
/// line breaks and spaces: whitespace (collapsed, or before the body), NUL,
/// a leading byte-order mark and the content worker's marker.
fn comparable(text: &str) -> String {
    text.strip_prefix('\u{FEFF}')
        .unwrap_or(text)
        .chars()
        .filter(|&c| !c.is_whitespace() && c != '\0' && c != EMBED)
        .collect()
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let html = text_html(text);
    let (read_back, links) = read(&html);
    assert_eq!(read_back, text);
    // A message opening with a code fence gets a line break of its own.
    let body = legacy_body(text, true);
    if text.starts_with("```") {
        assert_eq!(body, format!("<br/>{html}"));
    } else {
        assert_eq!(body, html);
    }
    // Another Nuschel client reads the copy: the same text, and the same
    // links (as far as the content worker keeps links).
    RENDERER.with(|renderer| {
        let Ok(stored) = renderer.legacy_plain_text(&html) else {
            return;
        };
        let parts: Vec<&str> = stored.split(EMBED).collect();
        let visible: String = parts.iter().step_by(2).copied().collect();
        assert_eq!(comparable(&visible), comparable(text), "{html:?}");
        let kept: Vec<&str> = parts
            .iter()
            .skip(1)
            .step_by(2)
            .filter_map(|directive| directive.strip_prefix("link:"))
            .collect();
        let expected: Vec<&str> = links
            .iter()
            .map(String::as_str)
            .filter(|href| href.len() <= MAX_URL_BYTES)
            .take(MAX_LINKS)
            .collect();
        assert_eq!(kept, expected, "{html:?}");
        if let Ok(document) = renderer.render(&stored) {
            assert!(!document.blocks.is_empty());
        }
    });
});
