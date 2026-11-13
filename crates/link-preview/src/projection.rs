// SPDX-License-Identifier: EUPL-1.2
//! The legacy projection: one stock Mumble message holding the sender's
//! literal text followed by the card as a quote block.
//!
//! ```text
//! {legacy_body(text)}<br/><blockquote><i>{host}</i><br/><a href="{url}"><b>{title or host}</b></a><br/>{description}<br/><img src="data:image/{png|jpeg};base64,…" /></blockquote>
//! ```
//!
//! The text part is exactly what a text-only send puts on the wire
//! (`nuschl_legacy_html::legacy_body(text, true)`), and every card string goes
//! through the same `escape_html`. The grammar is fixed: those escaped
//! strings, `<br/>`, `<blockquote>`, `<i>`, `<b>`, `<a href="…">` and one
//! `<img src="data:…" />`, all closed and quoted. It is well-formed XML
//! whenever every character is an XML character, which is checked: Murmur
//! refuses an image message it cannot parse. Nothing refers to a remote
//! resource; the image is the card's embedded, already prepared derivative.
//! The preview never shortens, blocks or changes the text: when the card
//! does not fit, a smaller card or the text alone is the result.
use crate::card::Card;
use crate::model::EmbeddedImage;
use crate::text::{truncate_utf16, utf16_len};
use nuschl_core::model::ServerLimits;
use nuschl_legacy_html::{escape_html, legacy_body};

/// Display truncation of the title in the projection (UTF-16 units).
pub const MAX_TITLE_UNITS: usize = 200;
/// Display truncation of the description in the projection (UTF-16 units).
pub const MAX_DESCRIPTION_UNITS: usize = 300;
/// Murmur counts an image message's text after re-serialising it inside a
/// `<document>` element without image sources; this margin covers what that
/// adds (21 units for the wrapper, attribute re-escaping).
pub const SERIALISATION_MARGIN: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Projection {
    /// One message: the literal text, then the card.
    Card {
        html: String,
        description: bool,
        image: bool,
    },
    /// Send the text alone, exactly as without a preview.
    TextOnly(TextOnly),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextOnly {
    /// The server does not allow HTML: no card (and no fetch for it).
    HtmlDisabled,
    /// The text alone already exceeds the limits; the usual refusal applies.
    TextRefused,
    /// The text holds a character XML cannot carry (most control
    /// characters), which Murmur would refuse once the card makes the
    /// message long.
    NotXml,
    /// Not even the smallest card fits the server's limits.
    Limits,
}

/// XML 1.0's `Char` production.
pub fn is_xml_char(ch: char) -> bool {
    matches!(
        ch,
        '\t' | '\n'
            | '\r'
            | '\u{20}'..='\u{d7ff}'
            | '\u{e000}'..='\u{fffd}'
            | '\u{10000}'..='\u{10ffff}'
    )
}

/// Project `card` after the draft `text` for the connection's current server
/// `limits`; the client's byte ceiling (`nuschl_core::MAX_CONTENT_BYTES`)
/// always applies.
pub fn project(text: &str, card: &Card, limits: &ServerLimits) -> Projection {
    if !limits.allow_html {
        return Projection::TextOnly(TextOnly::HtmlDisabled);
    }
    let text_html = legacy_body(text, true);
    if limits.check_text_wire(&text_html).is_err() {
        return Projection::TextOnly(TextOnly::TextRefused);
    }
    if !text_html.chars().all(is_xml_char) {
        return Projection::TextOnly(TextOnly::NotXml);
    }
    let image = card.image().map(img_element);
    let description = !card.description().is_empty();
    // Largest first: the image and description, then each alone, then the
    // linked title only.
    let shapes = [(true, true), (true, false), (false, true), (false, false)];
    for (with_image, with_description) in shapes {
        if (with_image && image.is_none()) || (with_description && !description) {
            continue;
        }
        let image = image.as_ref().filter(|_| with_image);
        let html = card_html(
            &text_html,
            card,
            with_description,
            image.map(|(element, _)| &**element),
        );
        let source_units = image.map_or(0, |(_, source_units)| *source_units);
        // Card strings are normalised and the image is ASCII; checked anyway.
        if fits(&html, source_units, image.is_some(), limits) && html.chars().all(is_xml_char) {
            return Projection::Card {
                html,
                description: with_description,
                image: image.is_some(),
            };
        }
    }
    Projection::TextOnly(TextOnly::Limits)
}

/// Murmur's `isTextAllowed` with HTML allowed: the whole message within the
/// image limit, and its text (without image sources) within the text limit.
fn fits(html: &str, source_units: usize, image: bool, limits: &ServerLimits) -> bool {
    // The client byte ceiling and the whole-message (image) limit.
    if limits.check_html_total(html).is_err() {
        return false;
    }
    if limits.message_length == 0 {
        return true;
    }
    let units = utf16_len(html);
    if image {
        units - source_units + SERIALISATION_MARGIN <= limits.message_length
    } else {
        units <= limits.message_length
    }
}

fn card_html(text_html: &str, card: &Card, description: bool, image: Option<&str>) -> String {
    let mut html = String::with_capacity(text_html.len() + 256 + image.map_or(0, str::len));
    html.push_str(text_html);
    html.push_str("<br/><blockquote><i>");
    html.push_str(&escape_html(card.host()));
    html.push_str("</i><br/><a href=\"");
    html.push_str(&escape_html(card.url()));
    html.push_str("\"><b>");
    html.push_str(&escape_html(&truncate_utf16(
        card.display_title(),
        MAX_TITLE_UNITS,
    )));
    html.push_str("</b></a>");
    if description {
        html.push_str("<br/>");
        html.push_str(&escape_html(&truncate_utf16(
            card.description(),
            MAX_DESCRIPTION_UNITS,
        )));
    }
    if let Some(image) = image {
        html.push_str("<br/>");
        html.push_str(image);
    }
    html.push_str("</blockquote>");
    html
}

/// `<img src="data:image/<kind>;base64,…" />` as stock Mumble writes it: the
/// base64 text in lines of 72 characters, each percent-encoded. Also returns
/// the UTF-16 length of the `src` value, which Murmur leaves out of the text.
fn img_element(image: &EmbeddedImage) -> (String, usize) {
    let encoded = base64(&image.data);
    let mut source = String::with_capacity(encoded.len() * 9 / 8 + 32);
    source.push_str("data:");
    source.push_str(image.mime.as_str());
    source.push_str(";base64,");
    for (index, line) in encoded.as_bytes().chunks(72).enumerate() {
        if index > 0 {
            source.push('\n');
        }
        for &byte in line {
            match byte {
                b'+' => source.push_str("%2B"),
                b'/' => source.push_str("%2F"),
                b'=' => source.push_str("%3D"),
                other => source.push(char::from(other)),
            }
        }
    }
    let units = source.len();
    (format!("<img src=\"{source}\" />"), units)
}

/// Standard, padded base64 (RFC 4648 section 4).
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let bytes = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let group = (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2]);
        let symbol = |shift: u32| char::from(ALPHABET[((group >> shift) & 63) as usize]);
        out.push(symbol(18));
        out.push(symbol(12));
        out.push(if chunk.len() > 1 { symbol(6) } else { '=' });
        out.push(if chunk.len() > 2 { symbol(0) } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::tests::image;
    use crate::metadata::ExtractedMetadata;
    use crate::model::{ImageMime, MAX_IMAGE_BYTES};
    use crate::policy::Target;
    use nuschl_core::MAX_CONTENT_BYTES;
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    fn limits(text: usize, image: usize) -> ServerLimits {
        ServerLimits {
            message_length: text,
            image_message_length: image,
            ..ServerLimits::default()
        }
    }

    fn card_with(title: &str, description: &str, picture: Option<EmbeddedImage>) -> Card {
        Card::new(
            &Target::parse("https://example.org/a?x=1&y=\"2\"#top").unwrap(),
            &ExtractedMetadata {
                title: title.into(),
                description: description.into(),
                image: None,
                image_alt: String::new(),
            },
            picture,
        )
        .unwrap()
    }

    /// A card with the largest carried image the model allows.
    fn full_card(description: &str) -> Card {
        let mut picture = image(ImageMime::Png, 640, 360);
        picture.data.resize(MAX_IMAGE_BYTES, 0xfb);
        card_with("A <b>title</b> & \"more\"", description, Some(picture))
    }

    /// What an independent XML parser says about each fragment, read as
    /// Murmur does (`<document>…</document>`): `None` when it refuses it,
    /// else the UTF-16 length of the document re-serialised without image
    /// sources (Murmur's text count). Python's expat, not this crate, parses.
    fn oracle(fragments: &[&str]) -> Vec<Option<usize>> {
        const SCRIPT: &str = r#"
import sys, xml.etree.ElementTree as ET
out = []
for fragment in sys.stdin.buffer.read().decode('utf-8').split('\0'):
    try:
        root = ET.fromstring('<document>' + fragment + '</document>')
    except ET.ParseError:
        out.append('refused')
        continue
    for img in root.iter('img'):
        img.attrib.pop('src', None)
    out.append(str(len(ET.tostring(root, encoding='unicode').encode('utf-16-le')) // 2))
sys.stdout.write('\n'.join(out))
"#;
        let mut child = Command::new("python3")
            .args(["-I", "-c", SCRIPT])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("python3 is the independent XML parser for this test");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(fragments.join("\0").as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let lines: Vec<Option<usize>> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| line.parse().ok())
            .collect();
        assert_eq!(lines.len(), fragments.len());
        lines
    }

    /// Murmur's `isTextAllowed` with HTML allowed, with the XML parse and the
    /// text count from the independent oracle.
    fn murmur_accepts(html: &str, xml: Option<usize>, limits: &ServerLimits) -> bool {
        let length = utf16_len(html);
        if limits.message_length == 0 && limits.image_message_length == 0 {
            return true;
        }
        if limits.image_message_length != 0 && length > limits.image_message_length {
            return false;
        }
        if limits.message_length == 0 || length <= limits.message_length {
            return true;
        }
        html.contains('<') && xml.is_some_and(|units| units <= limits.message_length)
    }

    #[test]
    fn the_independent_parser_accepts_every_projection_and_refuses_broken_xml() {
        // The oracle is live: it refuses what Murmur's parser refuses.
        assert_eq!(
            oracle(&["<br>", "a &nbsp; b", "bell\u{7}", "</b>", "ok<br/>"]),
            [None, None, None, None, Some(29)]
        );
        let texts = [
            "look: https://example.org/a <now> & \"then\"\nbye",
            "```\ncode & <tags>\n```",
            "www.example.org, (https://example.net/x_(y)) 'quoted' é 😀",
            "",
        ];
        let cards = [
            full_card(&"word & <word> ".repeat(60)),
            card_with("", "", Some(image(ImageMime::Jpeg, 8, 8))),
            card_with("]]> \"q\" 'a' &amp; <x>", "d", None),
        ];
        let all_limits = [
            limits(5000, 131_072),
            limits(0, 0),
            limits(5000, 0),
            limits(1000, 8000),
            limits(300, 0),
            limits(250, 8000),
        ];
        let mut cases = Vec::new();
        for text in texts {
            for card in &cards {
                for limits in &all_limits {
                    if let Projection::Card { html, .. } = project(text, card, limits) {
                        cases.push((html, limits));
                    }
                }
            }
        }
        assert!(cases.len() > 40);
        let fragments: Vec<&str> = cases.iter().map(|(html, _)| html.as_str()).collect();
        for ((html, limits), xml) in cases.iter().zip(oracle(&fragments)) {
            assert!(xml.is_some(), "refused: {html}");
            assert!(murmur_accepts(html, xml, limits), "{limits:?}: {html}");
        }
    }

    #[test]
    fn the_text_comes_first_and_the_card_is_escaped_inert_markup() {
        let text = "look: https://example.org/a <now> & \"then\"\nbye";
        let card = full_card("Desc <script>alert(1)</script>");
        let Projection::Card {
            html,
            description: true,
            image: true,
        } = project(text, &card, &ServerLimits::default())
        else {
            panic!("the default limits carry the full card");
        };
        let text_html = legacy_body(text, true);
        assert!(text_html.contains("<a href=\"https://example.org/a\">"));
        assert!(html.starts_with(&text_html));
        let card_part = &html[text_html.len()..];
        assert!(card_part.starts_with(
            "<br/><blockquote><i>example.org</i><br/><a href=\"https://example.org/a?x=1&amp;y=%222%22#top\"><b>A &lt;b&gt;title&lt;/b&gt; &amp; &quot;more&quot;</b></a><br/>Desc &lt;script&gt;alert(1)&lt;/script&gt;<br/><img src=\"data:image/png;base64,iVBORw0KGgo"
        ));
        assert!(card_part.ends_with("\" /></blockquote>"));
        // Exactly one image, from data: only; no other resource or markup.
        assert_eq!(html.matches("<img").count(), 1);
        assert_eq!(html.matches("src=").count(), 1);
        assert!(!html.contains("<script"));
        assert!(!html.contains("&nbsp;") && !html.contains("style="));
        // Mumble's 72-column percent-encoded base64.
        let source = &card_part[card_part.find("base64,").unwrap() + 7..card_part.len() - 17];
        assert!(source.lines().all(|line| line.len() <= 72 * 3));
        assert!(source.contains("%2B") || source.contains("%2F"));
    }

    #[test]
    fn cards_shrink_to_fit_the_server_and_never_shorten_the_text() {
        let text = "see https://example.org/a";
        let card = full_card(&"word ".repeat(100));
        let cases = [
            // Murmur's defaults: everything.
            (limits(5000, 131_072), Some((true, true))),
            // Unlimited by the server: the client ceiling still applies.
            (limits(0, 0), Some((true, true))),
            (limits(5000, 0), Some((true, true))),
            // Tight image limit: no image, the description stays.
            (limits(1000, 8000), Some((true, false))),
            // Room for the image without the description.
            (limits(330, 0), Some((false, true))),
            // Room for the linked title only.
            (limits(280, 8000), Some((false, false))),
            // Not even that: the text alone.
            (limits(150, 0), None),
        ];
        for (limits, expected) in cases {
            match (project(text, &card, &limits), expected) {
                (
                    Projection::Card {
                        html,
                        description,
                        image,
                    },
                    Some(want),
                ) => {
                    assert_eq!((description, image), want, "{limits:?}");
                    assert!(html.starts_with(&legacy_body(text, true)));
                    assert!(html.len() <= MAX_CONTENT_BYTES);
                    assert!(limits.check_html_total(&html).is_ok());
                }
                (Projection::TextOnly(TextOnly::Limits), None) => {}
                (other, _) => panic!("{limits:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn text_that_cannot_carry_a_card_is_sent_alone() {
        let card = full_card("d");
        let no_html = ServerLimits {
            allow_html: false,
            ..ServerLimits::default()
        };
        assert_eq!(
            project("hi", &card, &no_html),
            Projection::TextOnly(TextOnly::HtmlDisabled)
        );
        assert_eq!(
            project(&"x".repeat(5001), &card, &ServerLimits::default()),
            Projection::TextOnly(TextOnly::TextRefused)
        );
        assert_eq!(
            project(&"x".repeat(MAX_CONTENT_BYTES + 1), &card, &limits(0, 0)),
            Projection::TextOnly(TextOnly::TextRefused)
        );
        for text in [
            "bell\u{7}",
            "nul\u{0}",
            "esc\u{1b}[31m",
            "x\u{fffe}",
            "\u{ffff}",
        ] {
            assert_eq!(
                project(text, &card, &ServerLimits::default()),
                Projection::TextOnly(TextOnly::NotXml),
                "{text:?}"
            );
        }
        // Markup in the text is the sender's literal text, escaped.
        let Projection::Card { html, .. } = project("<br> &nbsp;", &card, &limits(0, 0)) else {
            panic!("fits");
        };
        assert!(html.starts_with("&lt;br&gt; &amp;nbsp;<br/><blockquote>"));
    }

    #[test]
    fn xml_characters_follow_the_char_production() {
        for ch in [
            '\t', '\n', '\r', ' ', 'é', '\u{d7ff}', '\u{e000}', '\u{fffd}', '😀',
        ] {
            assert!(is_xml_char(ch), "{ch:?}");
        }
        for ch in [
            '\u{0}', '\u{7}', '\u{b}', '\u{c}', '\u{1f}', '\u{fffe}', '\u{ffff}',
        ] {
            assert!(!is_xml_char(ch), "{ch:?}");
        }
    }

    #[test]
    fn long_titles_and_descriptions_are_cut_for_display_only() {
        let title = "t".repeat(400);
        let description = "😀".repeat(400);
        let card = card_with(&title, &description, None);
        let Projection::Card { html, .. } = project("x", &card, &limits(0, 0)) else {
            panic!("fits");
        };
        assert!(html.contains(&format!("<b>{}…</b>", "t".repeat(MAX_TITLE_UNITS - 1))));
        // Astral characters are two units each and never split.
        let shown = html.split("</a><br/>").nth(1).unwrap();
        let shown = shown.strip_suffix("</blockquote>").unwrap();
        assert_eq!(utf16_len(shown), MAX_DESCRIPTION_UNITS - 1);
        assert!(shown.ends_with("😀…"));
        // The carried model keeps the full values.
        assert_eq!(card.to_preview().title, title);
        assert_eq!(card.to_preview().description, description);
        // A card without a title links its host.
        let host_only = card_with("", "", Some(image(ImageMime::Jpeg, 8, 8)));
        let Projection::Card { html, .. } = project("x", &host_only, &limits(0, 0)) else {
            panic!("fits");
        };
        assert!(html.contains("<b>example.org</b>"));
    }

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        for (input, output) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), output);
        }
        assert_eq!(base64(&[0xfb, 0xff]), "+/8=");
        let (element, units) = img_element(&EmbeddedImage {
            data: vec![0xfb, 0xff],
            ..image(ImageMime::Jpeg, 1, 1)
        });
        assert_eq!(element, "<img src=\"data:image/jpeg;base64,%2B%2F8%3D\" />");
        assert_eq!(units, "data:image/jpeg;base64,%2B%2F8%3D".len());
        let (element, _) = img_element(&EmbeddedImage {
            data: vec![0; 54 * 2],
            ..image(ImageMime::Png, 1, 1)
        });
        assert_eq!(element.matches('\n').count(), 1);
    }
}
