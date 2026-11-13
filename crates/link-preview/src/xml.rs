// SPDX-License-Identifier: EUPL-1.2
//! A conservative check that a fragment is well-formed XML 1.0 content.
//!
//! Murmur parses an image message that exceeds its text limit with
//! `QXmlStreamReader` (as `<document>…</document>`) and refuses it when the
//! parse fails, so every legacy projection must pass this check. Only a
//! subset is accepted: elements with ASCII names, quoted attributes without
//! duplicates, character data, the five predefined entities and numeric
//! character references to XML characters. Comments, CDATA sections,
//! processing instructions and doctypes are refused: a refusal only ever
//! means "send the text alone". Linear in the input.

/// Deeper element nesting is refused.
pub const MAX_DEPTH: usize = 64;
/// More attributes on one element are refused.
pub const MAX_ATTRIBUTES: usize = 16;

/// XML 1.0's `Char` production.
pub fn is_xml_char(ch: char) -> bool {
    matches!(
        ch,
        '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}'
    )
}

fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

pub fn well_formed(fragment: &str) -> bool {
    if !fragment.chars().all(is_xml_char) {
        return false;
    }
    let bytes = fragment.as_bytes();
    let mut open: Vec<&[u8]> = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'<' if bytes.get(at + 1) == Some(&b'/') => {
                let Some((name, mut next)) = name_at(bytes, at + 2) else {
                    return false;
                };
                while next < bytes.len() && is_space(bytes[next]) {
                    next += 1;
                }
                if bytes.get(next) != Some(&b'>') || open.pop() != Some(name) {
                    return false;
                }
                at = next + 1;
            }
            b'<' => {
                let Some(next) = start_tag(bytes, at + 1, &mut open) else {
                    return false;
                };
                if open.len() > MAX_DEPTH {
                    return false;
                }
                at = next;
            }
            b'&' => {
                let Some(next) = reference(bytes, at) else {
                    return false;
                };
                at = next;
            }
            // `]]>` may not appear in character data.
            b'>' if at >= 2 && &bytes[at - 2..at] == b"]]" => return false,
            _ => at += 1,
        }
    }
    open.is_empty()
}

/// A start or empty-element tag after its '<'; pushes a start tag's name.
/// Returns the position after the tag.
fn start_tag<'a>(bytes: &'a [u8], from: usize, open: &mut Vec<&'a [u8]>) -> Option<usize> {
    let (name, mut at) = name_at(bytes, from)?;
    let mut attributes: Vec<&[u8]> = Vec::new();
    loop {
        let spaced = at < bytes.len() && is_space(bytes[at]);
        while at < bytes.len() && is_space(bytes[at]) {
            at += 1;
        }
        match bytes.get(at)? {
            b'>' => {
                open.push(name);
                return Some(at + 1);
            }
            b'/' => {
                return (bytes.get(at + 1) == Some(&b'>')).then_some(at + 2);
            }
            _ if !spaced => return None,
            _ => {
                let (attribute, next) = name_at(bytes, at)?;
                if attributes.contains(&attribute) || attributes.len() == MAX_ATTRIBUTES {
                    return None;
                }
                attributes.push(attribute);
                at = next;
                while at < bytes.len() && is_space(bytes[at]) {
                    at += 1;
                }
                if bytes.get(at) != Some(&b'=') {
                    return None;
                }
                at += 1;
                while at < bytes.len() && is_space(bytes[at]) {
                    at += 1;
                }
                let quote = *bytes.get(at).filter(|byte| matches!(byte, b'"' | b'\''))?;
                at += 1;
                loop {
                    match *bytes.get(at)? {
                        byte if byte == quote => break,
                        b'<' => return None,
                        b'&' => at = reference(bytes, at)?,
                        _ => at += 1,
                    }
                }
                at += 1;
            }
        }
    }
}

/// An ASCII XML name: `[A-Za-z_:][A-Za-z0-9._:-]*`.
fn name_at(bytes: &[u8], from: usize) -> Option<(&[u8], usize)> {
    let first = *bytes.get(from)?;
    if !(first.is_ascii_alphabetic() || matches!(first, b'_' | b':')) {
        return None;
    }
    let len = bytes[from..]
        .iter()
        .take_while(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-')
        })
        .count();
    Some((&bytes[from..from + len], from + len))
}

/// A reference at '&': a predefined entity or a numeric reference to an XML
/// character. Returns the position after its ';'.
fn reference(bytes: &[u8], at: usize) -> Option<usize> {
    let rest = &bytes[at + 1..];
    for entity in [&b"amp;"[..], b"lt;", b"gt;", b"quot;", b"apos;"] {
        if rest.starts_with(entity) {
            return Some(at + 1 + entity.len());
        }
    }
    let numeric = rest.strip_prefix(b"#")?;
    let (hex, digits) = match numeric.strip_prefix(b"x") {
        Some(digits) => (true, digits),
        None => (false, numeric),
    };
    let len = digits
        .iter()
        .take_while(|byte| {
            if hex {
                byte.is_ascii_hexdigit()
            } else {
                byte.is_ascii_digit()
            }
        })
        .count();
    // Eight digits cover every code point; more are refused, not parsed.
    if len == 0 || len > 8 || digits.get(len) != Some(&b';') {
        return None;
    }
    let text = std::str::from_utf8(&digits[..len]).ok()?;
    let value = u32::from_str_radix(text, if hex { 16 } else { 10 }).ok()?;
    char::from_u32(value).filter(|ch| is_xml_char(*ch))?;
    Some(at + 1 + 1 + usize::from(hex) + len + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_projection_subset_is_well_formed() {
        for fragment in [
            "",
            "plain text",
            "a &amp; b &lt; c &gt; d &quot; &apos; &#233; &#xE9; &#x1F600;",
            "<br/>",
            "<br />",
            "line<br/>line",
            r#"<a href="https://example.org/?a=1&amp;b=2">x</a>"#,
            r#"<blockquote><i>h</i><br/><a href='u'><b>t</b></a></blockquote>"#,
            "<img src=\"data:image/png;base64,iVBO%2B\nAAAA%3D\" />",
            "> alone is fine",
            "<a\nhref = \"x\" >y</a >",
        ] {
            assert!(well_formed(fragment), "{fragment}");
        }
    }

    #[test]
    fn invalid_or_unsupported_xml_is_refused() {
        for fragment in [
            "<br>",
            "<b><i></b></i>",
            "</b>",
            "<b>",
            "a < b",
            "a & b",
            "&nbsp;",
            "&#0;",
            "&#x7;",
            "&#xD800;",
            "&#xFFFE;",
            "&#1114112;",
            "&#123456789;",
            "&#;",
            "&#x;",
            "a ]]> b",
            "<!-- c -->",
            "<![CDATA[x]]>",
            "<?xml version=\"1.0\"?>",
            "<!DOCTYPE x>",
            "<a href=x>y</a>",
            "<a href=\"x\"href=\"y\">z</a>",
            "<a href=\"x\" href=\"y\">z</a>",
            "<a href=\"<\">z</a>",
            "<a href=\"&\">z</a>",
            "<a href=\"x>z</a>",
            "<a href>z</a>",
            "<1a/>",
            "< a/>",
            "<a/ >",
            "text\u{7}bell",
            "text\u{fffe}",
            "text\u{0}",
        ] {
            assert!(!well_formed(fragment), "{fragment:?}");
        }
        let attributes =
            |count: usize| -> String { (0..count).map(|n| format!(" a{n}=\"\"")).collect() };
        assert!(well_formed(&format!("<a{}/>", attributes(MAX_ATTRIBUTES))));
        assert!(!well_formed(&format!(
            "<a{}/>",
            attributes(MAX_ATTRIBUTES + 1)
        )));
        let nested = |depth: usize| format!("{}{}", "<b>".repeat(depth), "</b>".repeat(depth));
        assert!(well_formed(&nested(MAX_DEPTH)));
        assert!(!well_formed(&nested(MAX_DEPTH + 1)));
    }

    #[test]
    fn hostile_fragments_take_linear_time() {
        let size = 256 * 1024;
        for fragment in [
            "<a".repeat(size / 2),
            "&#".repeat(size / 2),
            "</".repeat(size / 2),
            "<a ".repeat(size / 3),
            format!("<a b=\"{}", "&amp;".repeat(size / 5)),
            "]]".repeat(size / 2),
            "<b/>".repeat(size / 4),
        ] {
            let started = std::time::Instant::now();
            let _ = well_formed(&fragment);
            assert!(started.elapsed() < std::time::Duration::from_secs(1));
        }
    }
}
