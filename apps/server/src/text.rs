// SPDX-License-Identifier: EUPL-1.2
//! Text limits for messages, comments and channel descriptions, following
//! Murmur's `isTextAllowed`: lengths count UTF-16 code units (Qt's
//! `QString::length`), `textmessagelength` bounds the visible text,
//! `imagemessagelength` the whole message including inline `data:` images, and
//! with HTML disabled markup is reduced to escaped plain text.

#[derive(Clone, Copy, Debug)]
pub struct TextLimits {
    pub allow_html: bool,
    /// 0 means unlimited.
    pub text: usize,
    /// 0 means unlimited.
    pub image: usize,
}

/// The text exceeds the server's limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TooLong;

/// `Ok(None)` when the text may pass unchanged, `Ok(Some(text))` when it was
/// rewritten (HTML disabled), `Err(TooLong)` when it is too long.
pub fn check(text: &str, limits: TextLimits) -> Result<Option<String>, TooLong> {
    if !limits.allow_html {
        let filtered = filter_html(text);
        let length = utf16_len(filtered.as_deref().unwrap_or(text));
        return if limits.text == 0 || length <= limits.text {
            Ok(filtered)
        } else {
            Err(TooLong)
        };
    }
    if limits.text == 0 && limits.image == 0 {
        return Ok(None);
    }
    let length = utf16_len(text);
    if limits.image != 0 && length > limits.image {
        return Err(TooLong);
    }
    if limits.text == 0 || length <= limits.text {
        return Ok(None);
    }
    // Over the text limit but within the image limit: only image data may make
    // up the difference.
    if !text.contains('<') || utf16_len_without_image_sources(text) > limits.text {
        return Err(TooLong);
    }
    Ok(None)
}

pub fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// The length with the values of `src` attributes in `<img>` tags left out.
fn utf16_len_without_image_sources(text: &str) -> usize {
    let mut length = 0;
    let mut rest = text;
    while let Some(start) = find_ascii_ci(rest, "<img") {
        length += utf16_len(&rest[..start]);
        let tag_end = rest[start..]
            .find('>')
            .map_or(rest.len(), |end| start + end + 1);
        length += utf16_len(&strip_src(&rest[start..tag_end]));
        rest = &rest[tag_end..];
    }
    length + utf16_len(rest)
}

/// Remove `src="…"` / `src='…'` / `src=…` from one tag.
fn strip_src(tag: &str) -> String {
    let mut out = String::with_capacity(tag.len());
    let mut rest = tag;
    while let Some(position) = find_ascii_ci(rest, "src") {
        let (before, after) = rest.split_at(position);
        let attribute_start = before
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || c == '<');
        let value = after[3..].trim_start();
        if !attribute_start || !value.starts_with('=') {
            out.push_str(&rest[..position + 3]);
            rest = &rest[position + 3..];
            continue;
        }
        out.push_str(before);
        let value = value[1..].trim_start();
        rest = match value.chars().next() {
            Some(quote @ ('"' | '\'')) => {
                value[1..].find(quote).map_or("", |end| &value[end + 2..])
            }
            _ => value
                .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
                .map_or("", |end| &value[end..]),
        };
    }
    out.push_str(rest);
    out
}

fn find_ascii_ci(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

/// Murmur's `HTMLFilter::filter`: text content with whitespace collapsed (line
/// breaks for `<br>` and `</p>` before collapsing, as Murmur does), angle
/// brackets escaped. Returns `None` when there was nothing to change.
pub fn filter_html(text: &str) -> Option<String> {
    let plain = if text.contains('<') {
        let mut plain = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find('<') {
            decode_entities(&rest[..start], &mut plain);
            let Some(end) = rest[start..].find('>') else {
                decode_entities(&rest[start..], &mut plain);
                rest = "";
                break;
            };
            let tag = rest[start + 1..start + end].trim().to_ascii_lowercase();
            let name = tag.trim_start_matches('/').trim_end_matches('/').trim();
            let name = name.split_whitespace().next().unwrap_or("");
            if name == "br" || (name == "p" && tag.starts_with('/')) {
                plain.push('\n');
            }
            rest = &rest[start + end + 1..];
        }
        decode_entities(rest, &mut plain);
        Some(plain)
    } else {
        None
    };
    let simplified = plain
        .as_deref()
        .unwrap_or(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let escaped = if plain.is_some() {
        simplified.replace('<', "&lt;").replace('>', "&gt;")
    } else {
        simplified
    };
    (escaped != text).then_some(escaped)
}

fn decode_entities(text: &str, out: &mut String) {
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest.bytes().take(12).position(|byte| byte == b';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| {
                    entity
                        .strip_prefix('#')
                        .and_then(|decimal| decimal.parse().ok())
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(character) => {
                out.push(character);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
}

#[cfg(test)]
mod tests {
    use super::*;

    const HTML: TextLimits = TextLimits {
        allow_html: true,
        text: 10,
        image: 100,
    };

    #[test]
    fn limits_count_utf16_and_allow_image_data_up_to_the_image_limit() {
        assert_eq!(check("0123456789", HTML), Ok(None));
        assert_eq!(check("01234567890", HTML), Err(TooLong));
        // Ten emoji are twenty UTF-16 units.
        assert_eq!(check(&"😀".repeat(5), HTML), Ok(None));
        assert_eq!(check(&"😀".repeat(6), HTML), Err(TooLong));
        let image = format!("hi<img src=\"data:image/png;base64,{}\"/>", "A".repeat(50));
        // Without the image data this is "hi<img />", nine units of text.
        assert_eq!(check(&image, HTML), Ok(None));
        let mut tight = HTML;
        tight.text = 5;
        assert_eq!(
            check(&image, tight),
            Err(TooLong),
            "the tag itself still counts as text"
        );
        let mut roomy = HTML;
        roomy.text = 20;
        assert_eq!(check(&image, roomy), Ok(None));
        assert_eq!(
            check(&format!("{image}{}", "B".repeat(30)), roomy),
            Err(TooLong)
        );
        let unlimited = TextLimits {
            allow_html: true,
            text: 0,
            image: 0,
        };
        assert_eq!(check(&"x".repeat(100_000), unlimited), Ok(None));
    }

    #[test]
    fn html_is_flattened_when_disallowed() {
        let plain = TextLimits {
            allow_html: false,
            text: 100,
            image: 0,
        };
        assert_eq!(check("hello  world", plain), Ok(Some("hello world".into())));
        assert_eq!(check("hello world", plain), Ok(None));
        assert_eq!(
            check(
                "<b>bold</b> &lt;tag&gt; &amp; <a href='x'>link</a><br/>next",
                plain
            ),
            Ok(Some("bold &lt;tag&gt; & link next".into()))
        );
        assert_eq!(
            check("<script>alert(1)</script>", plain),
            Ok(Some("alert(1)".into()))
        );
        assert_eq!(check("a < b", plain), Ok(Some("a &lt; b".into())));
    }

    #[test]
    fn src_stripping_handles_quotes_and_lookalikes() {
        assert_eq!(strip_src("<img src=\"abc\" alt='x'>"), "<img  alt='x'>");
        assert_eq!(
            strip_src("<img alt='nosrc' SRC='abc'>"),
            "<img alt='nosrc' >"
        );
        assert_eq!(strip_src("<img datasrc=1 src=abc>"), "<img datasrc=1 >");
        assert_eq!(strip_src("<img src=\"unterminated"), "<img ");
    }

    #[test]
    fn hostile_text_never_panics() {
        let limits = [
            HTML,
            TextLimits {
                allow_html: false,
                text: 5,
                image: 0,
            },
        ];
        for text in [
            "<",
            ">",
            "&",
            "&#;",
            "&#x110000;",
            "&#99999999999;",
            "<img",
            "<img src",
            "<img src=",
            "<img src='",
            "<<<>>>",
            "&amp",
            "é<é>é&é;",
        ] {
            for limit in limits {
                let _ = check(text, limit);
            }
        }
    }
}
