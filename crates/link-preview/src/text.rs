// SPDX-License-Identifier: EUPL-1.2
//! Plain-text normalisation shared by extraction, the card model and the
//! legacy projection. Every function is linear in its input and its output is
//! bounded by an explicit byte or UTF-16 budget.
use std::borrow::Cow;

/// Characters a carried preview string may not contain: what
/// `nuschl_core::preview` refuses (controls and bidi overrides/marks), plus the
/// two BMP noncharacters XML 1.0 cannot carry, so a card always projects into
/// well-formed legacy HTML.
pub fn refused(ch: char) -> bool {
    ch.is_control()
        || matches!(
            ch,
            '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{fffe}'
                | '\u{ffff}'
        )
}

/// Whether `text` is already what [`normalize`] would produce within
/// `max_bytes`: no refused characters, no leading, trailing or repeated
/// whitespace, and no whitespace other than U+0020.
pub fn is_normalized(text: &str, max_bytes: usize) -> bool {
    text.len() <= max_bytes && normalize(text, max_bytes) == text
}

/// Collapse every run of Unicode whitespace into one space, drop refused
/// characters, trim both ends and cut at a character boundary so the result
/// holds at most `max_bytes` bytes.
pub fn normalize(input: &str, max_bytes: usize) -> String {
    let mut out = String::with_capacity(input.len().min(max_bytes));
    let mut space = false;
    for ch in input.chars() {
        // Tabs and line breaks are controls too: whitespace is decided first.
        if ch.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if refused(ch) {
            continue;
        }
        let needed = ch.len_utf8() + usize::from(space);
        if out.len() + needed > max_bytes {
            break;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(ch);
    }
    out
}

/// Cut `text` to at most `max_units` UTF-16 code units (what Murmur and Qt
/// count), ending a shortened text with "…". Display only: carried values are
/// never shortened this way.
pub fn truncate_utf16(text: &str, max_units: usize) -> Cow<'_, str> {
    if utf16_len(text) <= max_units {
        return Cow::Borrowed(text);
    }
    if max_units == 0 {
        return Cow::Borrowed("");
    }
    let mut units = 0;
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        // One unit stays free for the ellipsis.
        if units + ch.len_utf16() > max_units - 1 {
            break;
        }
        units += ch.len_utf16();
        end = index + ch.len_utf8();
    }
    let mut out = text[..end].trim_end().to_owned();
    out.push('…');
    Cow::Owned(out)
}

pub fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_collapses_and_instructions_disappear() {
        assert_eq!(
            normalize("  A\t\n title\u{2028}\u{a0} here  ", 64),
            "A title here"
        );
        assert_eq!(
            normalize("\u{1b}]52;c;\u{7}x\u{202e}gnp.exe\u{2066}", 64),
            "]52;c;xgnp.exe"
        );
        assert_eq!(normalize("a\u{fffe}\u{ffff}b\u{0}c\u{85}d", 64), "abc d");
        assert_eq!(normalize("\n\n\t", 64), "");
        assert!(is_normalized("A title", 64));
        assert!(!is_normalized("A  title", 64));
        assert!(!is_normalized(" A", 64));
        assert!(!is_normalized("A\u{200f}", 64));
        assert!(!is_normalized("Abc", 2));
    }

    #[test]
    fn byte_budgets_cut_at_character_boundaries() {
        // "é" is two bytes, "😀" four: the cut never splits one.
        assert_eq!(normalize("ééé", 5), "éé");
        assert_eq!(normalize("a😀", 4), "a");
        assert_eq!(normalize("a😀", 5), "a😀");
        // A space is only written before a following character that fits.
        assert_eq!(normalize("ab cd", 3), "ab");
        assert_eq!(normalize("ab cd", 0), "");
        let long = "word ".repeat(10_000);
        let short = normalize(&long, 512);
        assert!(short.len() <= 512 && !short.ends_with(' '));
    }

    #[test]
    fn display_truncation_counts_utf16_units() {
        assert_eq!(truncate_utf16("short", 5), "short");
        assert_eq!(truncate_utf16("abcdef", 5), "abcd…");
        assert_eq!(truncate_utf16("abc def", 5), "abc…");
        // An astral character is two units and is never split.
        assert_eq!(truncate_utf16("ab😀cd", 4), "ab…");
        assert_eq!(truncate_utf16("ab😀cd", 5), "ab😀…");
        assert_eq!(truncate_utf16("abc", 0), "");
        assert_eq!(truncate_utf16("abc", 1), "…");
        assert!(utf16_len(&truncate_utf16(&"😀".repeat(500), 200)) <= 200);
    }
}
