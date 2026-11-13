// SPDX-License-Identifier: EUPL-1.2
//! The legacy projection: one stock Mumble message holding the sender's
//! literal text followed by the card as a quote block.
//!
//! ```text
//! {text}<br/><blockquote><i>{host}</i><br/><a href="{url}"><b>{title or host}</b></a><br/>{description}<br/><img src="data:image/{png|jpeg};base64,…" /></blockquote>
//! ```
//!
//! Every string is escaped, every attribute quoted, and the result is checked
//! to be well-formed XML: Murmur refuses an image message it cannot parse.
//! Nothing refers to a remote resource; the image is the card's embedded,
//! already prepared derivative. The preview never shortens, blocks or changes
//! the text: when the card does not fit, a smaller card or the text alone is
//! the result.
use crate::card::Card;
use crate::model::EmbeddedImage;
use crate::text::{truncate_utf16, utf16_len};
use crate::xml;
use nuschl_core::model::ServerLimits;

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
    /// The text cannot be carried as XML (for example a control character),
    /// which Murmur would refuse once the card makes the message long.
    NotWellFormed,
    /// Not even the smallest card fits the server's limits.
    Limits,
}

/// Project `card` after the text. `text_html` must be exactly the wire text a
/// text-only send produces (`nuschl_legacy_html::legacy_body(text, true)`),
/// so receivers see the same text with or without the card. `limits` are the
/// connection's current server limits; the client's byte ceiling
/// (`nuschl_core::MAX_CONTENT_BYTES`) always applies.
pub fn project(text_html: &str, card: &Card, limits: &ServerLimits) -> Projection {
    if !limits.allow_html {
        return Projection::TextOnly(TextOnly::HtmlDisabled);
    }
    if limits.check_text_wire(text_html).is_err() {
        return Projection::TextOnly(TextOnly::TextRefused);
    }
    if !xml::well_formed(text_html) {
        return Projection::TextOnly(TextOnly::NotWellFormed);
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
        let html = card_html(text_html, card, with_description, image.map(|(element, _)| &**element));
        let source_units = image.map_or(0, |(_, source_units)| *source_units);
        if fits(&html, source_units, image.is_some(), limits) && xml::well_formed(&html) {
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
    push_escaped(&mut html, card.host());
    html.push_str("</i><br/><a href=\"");
    push_escaped(&mut html, card.url());
    html.push_str("\"><b>");
    push_escaped(&mut html, &truncate_utf16(card.display_title(), MAX_TITLE_UNITS));
    html.push_str("</b></a>");
    if description {
        html.push_str("<br/>");
        push_escaped(
            &mut html,
            &truncate_utf16(card.description(), MAX_DESCRIPTION_UNITS),
        );
    }
    if let Some(image) = image {
        html.push_str("<br/>");
        html.push_str(image);
    }
    html.push_str("</blockquote>");
    html
}

/// Card strings are normalised (no line breaks); quotes are escaped for
/// attribute values.
fn push_escaped(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
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
    use crate::extract::ExtractedMetadata;
    use crate::model::{ImageMime, MAX_IMAGE_BYTES};
    use crate::policy::Target;
    use nuschl_core::MAX_CONTENT_BYTES;

    /// What `nuschl_legacy_html::legacy_body(text, true)` produces for text
    /// without links (this crate does not link it): escaped, line breaks kept.
    fn legacy_text(text: &str) -> String {
        let mut out = String::new();
        for ch in text.chars() {
            match ch {
                '\n' => out.push_str("<br/>"),
                _ => push_escaped(&mut out, ch.encode_utf8(&mut [0; 4])),
            }
        }
        out
    }

    fn limits(text: usize, image: usize) -> ServerLimits {
        ServerLimits {
            message_length: text,
            image_message_length: image,
            ..ServerLimits::default()
        }
    }

    /// A card with the largest carried image the model allows.
    fn full_card(description: &str) -> Card {
        let target = Target::parse("https://example.org/a?x=1&y=\"2\"#top").unwrap();
        let mut picture = image(ImageMime::Png, 640, 360);
        picture.data.resize(MAX_IMAGE_BYTES, 0xfb);
        Card::new(
            &target,
            &ExtractedMetadata {
                title: "A <b>title</b> & \"more\"".into(),
                description: description.into(),
                image: None,
                image_alt: String::new(),
            },
            Some(picture),
        )
        .unwrap()
    }

    /// Murmur's `isTextAllowed` with HTML allowed, as an independent check
    /// of the projections (XML validity through this crate's checker).
    fn murmur_accepts(html: &str, limits: &ServerLimits) -> bool {
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
        if !html.contains('<') || !xml::well_formed(html) {
            return false;
        }
        // Image sources are left out; the writer wraps it in <document>.
        let mut without = String::new();
        let mut rest = html;
        while let Some(start) = rest.find("<img src=\"") {
            without.push_str(&rest[..start + 10]);
            let end = rest[start + 10..].find('"').unwrap();
            rest = &rest[start + 10 + end..];
        }
        without.push_str(rest);
        utf16_len(&without) + "<document></document>".len() <= limits.message_length
    }

    #[test]
    fn the_text_comes_first_and_the_card_is_escaped_inert_xml() {
        let text = "look: https://example.org/a <now> & \"then\"\nbye";
        let text_html = legacy_text(text);
        let card = full_card("Desc <script>alert(1)</script>");
        let Projection::Card {
            html,
            description: true,
            image: true,
        } = project(&text_html, &card, &ServerLimits::default())
        else {
            panic!("the default limits carry the full card");
        };
        assert!(html.starts_with(&text_html));
        let card_part = &html[text_html.len()..];
        assert!(card_part.starts_with(
            "<br/><blockquote><i>example.org</i><br/><a href=\"https://example.org/a?x=1&amp;y=%222%22#top\"><b>A &lt;b&gt;title&lt;/b&gt; &amp; &quot;more&quot;</b></a><br/>Desc &lt;script&gt;alert(1)&lt;/script&gt;<br/><img src=\"data:image/png;base64,iVBORw0KGgo"
        ));
        assert!(card_part.ends_with("\" /></blockquote>"));
        assert!(xml::well_formed(&html));
        // Exactly one image, from data: only; no other resource or markup.
        assert_eq!(html.matches("<img").count(), 1);
        assert_eq!(html.matches("src=").count(), 1);
        assert!(!html.contains("<script"));
        assert!(!html.contains("&nbsp;") && !html.contains("style="));
        // Mumble's 72-column percent-encoded base64.
        let source = &card_part[card_part.find("base64,").unwrap() + 7..card_part.len() - 17];
        assert!(source.lines().all(|line| line.len() <= 72 * 3));
        assert!(source.contains("%2B") || source.contains("%2F"));
        assert!(murmur_accepts(&html, &ServerLimits::default()));
    }

    #[test]
    fn cards_shrink_to_fit_the_server_and_never_shorten_the_text() {
        let text_html = legacy_text("see https://example.org/a");
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
            (limits(300, 0), Some((false, true))),
            // Room for the linked title only.
            (limits(250, 8000), Some((false, false))),
            // Not even that: the text alone.
            (limits(120, 0), None),
        ];
        for (limits, expected) in cases {
            let projection = project(&text_html, &card, &limits);
            match (projection, expected) {
                (
                    Projection::Card {
                        html,
                        description,
                        image,
                    },
                    Some((want_description, want_image)),
                ) => {
                    assert_eq!(
                        (description, image),
                        (want_description, want_image),
                        "{limits:?}"
                    );
                    assert!(html.starts_with(&text_html));
                    assert!(html.len() <= MAX_CONTENT_BYTES);
                    assert!(murmur_accepts(&html, &limits), "{limits:?}");
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
        for text_html in ["bell\u{7}", "a < b", "<br>", "&nbsp;", "x\u{fffe}"] {
            assert_eq!(
                project(text_html, &card, &ServerLimits::default()),
                Projection::TextOnly(TextOnly::NotWellFormed),
                "{text_html:?}"
            );
        }
    }

    #[test]
    fn long_titles_and_descriptions_are_cut_for_display_only() {
        let title = "t".repeat(400);
        let description = "😀".repeat(400);
        let target = Target::parse("https://example.org/").unwrap();
        let card = Card::new(
            &target,
            &ExtractedMetadata {
                title: title.clone(),
                description: description.clone(),
                image: None,
                image_alt: String::new(),
            },
            None,
        )
        .unwrap();
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
        let host_only = card_with("", "");
        let Projection::Card { html, .. } = project("x", &host_only, &limits(0, 0)) else {
            panic!("fits");
        };
        assert!(html.contains("<b>example.org</b>"));
    }

    fn card_with(title: &str, description: &str) -> Card {
        let target = Target::parse("https://example.org/").unwrap();
        Card::new(
            &target,
            &ExtractedMetadata {
                title: title.into(),
                description: description.into(),
                image: None,
                image_alt: String::new(),
            },
            Some(image(ImageMime::Jpeg, 8, 8)),
        )
        .unwrap()
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

    #[test]
    fn arbitrary_card_strings_always_project_to_well_formed_xml() {
        let pieces = [
            "<", ">", "&", "\"", "'", "]]>", "&amp;", "&#0;", "é", "😀", "\u{202e}", "\u{7}",
            "\u{fffe}", " ", "<br>", "</blockquote>", "a", "\n",
        ];
        let mut state = 0x9e37_79b9_u32;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as usize
        };
        for _ in 0..500 {
            let mut field = || -> String {
                (0..next() % 40)
                    .map(|_| pieces[next() % pieces.len()])
                    .collect()
            };
            let (title, description) = (field(), field());
            let Ok(card) = Card::new(
                &Target::parse("https://example.org/p").unwrap(),
                &ExtractedMetadata {
                    title,
                    description,
                    image: None,
                    image_alt: String::new(),
                },
                Some(image(ImageMime::Png, 2, 2)),
            ) else {
                continue;
            };
            for limits in [limits(5000, 131_072), limits(300, 600), limits(0, 0)] {
                if let Projection::Card { html, .. } = project("text", &card, &limits) {
                    assert!(xml::well_formed(&html), "{html}");
                    assert!(murmur_accepts(&html, &limits));
                }
            }
        }
    }
}
