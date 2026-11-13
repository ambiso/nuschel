// SPDX-License-Identifier: EUPL-1.2
//! Open Graph v1 metadata from the head of an already-fetched HTML document.
//!
//! This is a strictly bounded, head-only scanner, not an HTML parser and not a
//! renderer: it never builds a tree, executes anything or resolves a
//! reference. It reads `<meta>` attributes and the first `<title>`, follows
//! the HTML tokenizer's tag, attribute, comment and raw-text rules closely
//! enough that markup-looking text inside `<script>`, `<style>`, `<noscript>`,
//! `<template>` and comments is skipped, and stops at the first `<body>` or
//! `</head>`. Every byte of input is visited a bounded number of times, so
//! hostile documents take linear time; nesting has no cost because no stack
//! is kept.
//!
//! The document is untrusted and page-controlled: call this only inside the
//! content worker's one-shot process, never in a credential-bearing process.
//! The returned image reference is page-controlled as well: the supervisor
//! resolves it against the fetched document URL with `policy::Target::join`,
//! which applies the full target policy again.
//!
//! Known simplifications, each of which can only make extraction miss data
//! (never read outside the input or take more time): `<template>` content is
//! skipped like raw text up to the first `</template>`; the script-data escape
//! states are not modelled, so a `</script>` inside `<!-- <script>` ends the
//! script; named character references need their `;` and only a common subset
//! is known (unknown ones stay literal text).
use crate::model::{MAX_ALT_BYTES, MAX_DESCRIPTION_BYTES, MAX_TITLE_BYTES, MAX_URL_BYTES};
use crate::text::normalize;

/// The largest document accepted, as the fetch caps it.
pub const MAX_DOCUMENT_BYTES: usize = 256 * 1024;
/// Scanning stops after this many tags (start, end, comment and doctype).
pub const MAX_TAGS: usize = 4096;
/// Scanning stops after this many `<meta>` start tags.
pub const MAX_META_TAGS: usize = 256;
/// Attributes after this many in one tag are consumed but ignored.
pub const MAX_ATTRIBUTES: usize = 64;
/// At most this many bytes of an attribute value or of the title text are
/// read; the rest is consumed but ignored.
pub const MAX_VALUE_BYTES: usize = 4096;
/// Entity decoding stops writing after this many bytes; the normalised
/// fields are much shorter.
const MAX_DECODED_BYTES: usize = 2 * MAX_VALUE_BYTES;
/// Tag names compared here are short; longer names are consumed, not kept.
const MAX_NAME_BYTES: usize = 16;

/// What the HTTP `Content-Type` charset parameter said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportCharset {
    Utf8,
    /// No charset parameter: a `<meta>` declaration may name one.
    Unspecified,
    /// Any other label: no preview (non-UTF-8 decoding is not supported).
    Other,
}

impl TransportCharset {
    /// Classify the `charset` parameter's label (`None` when absent).
    pub fn from_label(label: Option<&str>) -> Self {
        match label {
            None => Self::Unspecified,
            Some(label) if utf8_label(label.as_bytes()) => Self::Utf8,
            Some(_) => Self::Other,
        }
    }
}

/// Normalised, bounded metadata. Strings are inert plain text within the
/// carried model's limits; `image` is a raw, page-controlled reference.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtractedMetadata {
    pub title: String,
    pub description: String,
    pub image: Option<String>,
    pub image_alt: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtractError {
    /// The document exceeds [`MAX_DOCUMENT_BYTES`].
    TooLarge,
    /// A byte-order mark, the transport or the document declares an encoding
    /// other than UTF-8.
    Encoding,
    /// Neither a title, a description nor an image was found.
    NoMetadata,
}

impl std::fmt::Display for ExtractError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::TooLarge => "the page is too large for a preview",
            Self::Encoding => "the page does not use UTF-8",
            Self::NoMetadata => "the page has no preview metadata",
        })
    }
}
impl std::error::Error for ExtractError {}

/// Open Graph v1: the first `og:title`, `og:description`, `og:image` (or
/// `og:image:url`) and `og:image:alt` win; the `<title>` text and
/// `meta[name=description]` are fallbacks. `og:url`, `og:site_name`,
/// `twitter:*`, canonical links and `<base>` are ignored: nothing can replace
/// the sender's URL.
pub fn extract_open_graph(
    document: &[u8],
    charset: TransportCharset,
) -> Result<ExtractedMetadata, ExtractError> {
    if document.len() > MAX_DOCUMENT_BYTES {
        return Err(ExtractError::TooLarge);
    }
    // A byte-order mark decides the encoding over everything else.
    let (document, charset) = match document {
        [0xef, 0xbb, 0xbf, rest @ ..] => (rest, TransportCharset::Utf8),
        [0xfe, 0xff, ..] | [0xff, 0xfe, ..] => return Err(ExtractError::Encoding),
        _ => (document, charset),
    };
    if charset == TransportCharset::Other {
        return Err(ExtractError::Encoding);
    }
    let found = Scanner::new(document).run();
    if charset == TransportCharset::Unspecified && found.declared_charset == Some(false) {
        return Err(ExtractError::Encoding);
    }
    let title = found
        .og_title
        .or(found.title)
        .map(|raw| field(raw, MAX_TITLE_BYTES))
        .unwrap_or_default();
    let description = found
        .og_description
        .or(found.description)
        .map(|raw| field(raw, MAX_DESCRIPTION_BYTES))
        .unwrap_or_default();
    let image_alt = found
        .og_image_alt
        .map(|raw| field(raw, MAX_ALT_BYTES))
        .unwrap_or_default();
    let image = found.og_image.and_then(image_reference);
    if title.is_empty() && description.is_empty() && image.is_none() {
        return Err(ExtractError::NoMetadata);
    }
    // Alternative text describes the chosen image only.
    let image_alt = if image.is_some() {
        image_alt
    } else {
        String::new()
    };
    Ok(ExtractedMetadata {
        title,
        description,
        image,
        image_alt,
    })
}

fn field(raw: &[u8], max_bytes: usize) -> String {
    normalize(&decode_entities(raw), max_bytes)
}

/// A usable `og:image` reference: decoded, trimmed, without controls, and not
/// longer than a URL may be (an over-long reference is dropped, never cut).
fn image_reference(raw: &[u8]) -> Option<String> {
    let decoded = decode_entities(raw);
    // The URL parser removes tabs and newlines; any other control refuses.
    let reference: String = decoded
        .trim_matches(|ch: char| ch.is_ascii_whitespace())
        .chars()
        .filter(|ch| !matches!(ch, '\t' | '\n' | '\r'))
        .collect();
    if reference.is_empty()
        || reference.len() > MAX_URL_BYTES
        || reference.chars().any(crate::text::refused)
    {
        return None;
    }
    Some(reference)
}

/// The raw values found, as slices of the input (at most
/// [`MAX_VALUE_BYTES`] each). Nothing is decoded before it is chosen.
#[derive(Default)]
struct Found<'a> {
    og_title: Option<&'a [u8]>,
    og_description: Option<&'a [u8]>,
    og_image: Option<&'a [u8]>,
    og_image_alt: Option<&'a [u8]>,
    title: Option<&'a [u8]>,
    description: Option<&'a [u8]>,
    /// The first charset declaration: whether it names UTF-8.
    declared_charset: Option<bool>,
}

/// The attributes this scanner uses; all others are skipped.
#[derive(Default)]
struct Attributes<'a> {
    property: Option<&'a [u8]>,
    name: Option<&'a [u8]>,
    content: Option<&'a [u8]>,
    charset: Option<&'a [u8]>,
    http_equiv: Option<&'a [u8]>,
}

/// A tag name, lowercased, kept up to [`MAX_NAME_BYTES`].
#[derive(Default)]
struct Name {
    bytes: [u8; MAX_NAME_BYTES],
    len: usize,
    overlong: bool,
}

impl Name {
    fn push(&mut self, byte: u8) {
        if self.len < MAX_NAME_BYTES {
            self.bytes[self.len] = byte.to_ascii_lowercase();
            self.len += 1;
        } else {
            self.overlong = true;
        }
    }
    fn is(&self, name: &[u8]) -> bool {
        !self.overlong && &self.bytes[..self.len] == name
    }
}

struct Scanner<'a> {
    input: &'a [u8],
    at: usize,
    tags: usize,
    metas: usize,
    found: Found<'a>,
}

fn is_space(byte: u8) -> bool {
    matches!(byte, b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

impl<'a> Scanner<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            at: 0,
            tags: 0,
            metas: 0,
            found: Found::default(),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.at).copied()
    }

    fn rest(&self) -> &'a [u8] {
        &self.input[self.at.min(self.input.len())..]
    }

    /// Move past the next `needle` (or to the end without one).
    fn skip_past(&mut self, needle: &[u8]) {
        self.at = match find(self.rest(), needle) {
            Some(offset) => self.at + offset + needle.len(),
            None => self.input.len(),
        };
    }

    /// After `<!--`: HTML ends a comment at `>` or `->` right away, else at
    /// the first `-->` or `--!>`.
    fn skip_comment(&mut self) {
        if self.rest().starts_with(b">") {
            self.at += 1;
            return;
        }
        if self.rest().starts_with(b"->") {
            self.at += 2;
            return;
        }
        let mut from = self.at;
        while let Some(offset) = find(&self.input[from..], b"--") {
            let after = from + offset + 2;
            match self.input.get(after) {
                Some(b'>') => {
                    self.at = after + 1;
                    return;
                }
                Some(b'!') if self.input.get(after + 1) == Some(&b'>') => {
                    self.at = after + 2;
                    return;
                }
                _ => from += offset + 1,
            }
        }
        self.at = self.input.len();
    }

    fn run(mut self) -> Found<'a> {
        while let Some(offset) = self.rest().iter().position(|&byte| byte == b'<') {
            self.at += offset + 1;
            let keep_going = match self.peek() {
                Some(b'!') => {
                    self.count_tag() && {
                        if self.rest().starts_with(b"!--") {
                            self.at += 3;
                            self.skip_comment();
                        } else {
                            // Doctype, CDATA and bogus comments end at '>'.
                            self.skip_past(b">");
                        }
                        true
                    }
                }
                Some(b'?') => self.count_tag() && {
                    self.skip_past(b">");
                    true
                },
                Some(b'/') => {
                    self.at += 1;
                    match self.peek() {
                        Some(byte) if byte.is_ascii_alphabetic() => {
                            self.count_tag() && self.end_tag()
                        }
                        // `</>` is dropped; other `</…` are bogus comments.
                        Some(b'>') => {
                            self.at += 1;
                            true
                        }
                        Some(_) => self.count_tag() && {
                            self.skip_past(b">");
                            true
                        },
                        None => false,
                    }
                }
                Some(byte) if byte.is_ascii_alphabetic() => self.count_tag() && self.start_tag(),
                // A '<' in text.
                Some(_) => true,
                None => false,
            };
            if !keep_going {
                break;
            }
        }
        self.found
    }

    fn count_tag(&mut self) -> bool {
        self.tags += 1;
        self.tags <= MAX_TAGS
    }

    fn tag_name(&mut self) -> Name {
        let mut name = Name::default();
        while let Some(byte) = self.peek() {
            if is_space(byte) || byte == b'/' || byte == b'>' {
                break;
            }
            name.push(byte);
            self.at += 1;
        }
        name
    }

    /// An end tag: attributes are tokenized and discarded. Whether to go on.
    fn end_tag(&mut self) -> bool {
        let name = self.tag_name();
        let complete = self.attributes(&mut Attributes::default());
        complete && !name.is(b"head")
    }

    /// A start tag. Whether to go on scanning.
    fn start_tag(&mut self) -> bool {
        let name = self.tag_name();
        let mut attributes = Attributes::default();
        // A tag cut off by the end of the input is never emitted.
        if !self.attributes(&mut attributes) {
            return false;
        }
        match &name.bytes[..name.len] {
            _ if name.overlong => true,
            b"body" | b"plaintext" => false,
            b"meta" => {
                self.metas += 1;
                if self.metas > MAX_META_TAGS {
                    return false;
                }
                self.meta(&attributes);
                true
            }
            b"title" => {
                let text = self.raw_text(b"title");
                if self.found.title.is_none() {
                    self.found.title = Some(clip(text));
                }
                true
            }
            raw @ (b"script" | b"style" | b"noscript" | b"template" | b"textarea" | b"xmp"
            | b"iframe" | b"noembed" | b"noframes") => {
                self.raw_text(raw);
                true
            }
            _ => true,
        }
    }

    /// The text up to the matching end tag, which is left for the main loop
    /// to consume (or to the end of the input).
    fn raw_text(&mut self, name: &[u8]) -> &'a [u8] {
        let start = self.at;
        let mut from = self.at;
        while let Some(offset) = self.input[from..].iter().position(|&byte| byte == b'<') {
            let lt = from + offset;
            let after = &self.input[lt + 1..];
            if after.first() == Some(&b'/')
                && after.len() > name.len()
                && after[1..=name.len()].eq_ignore_ascii_case(name)
                && after
                    .get(name.len() + 1)
                    .is_some_and(|&byte| is_space(byte) || byte == b'/' || byte == b'>')
            {
                self.at = lt;
                return &self.input[start..lt];
            }
            from = lt + 1;
        }
        self.at = self.input.len();
        &self.input[start..]
    }

    /// Tokenize attributes up to and including the tag's '>'. Only the first
    /// occurrence of each used attribute is kept (HTML drops duplicates).
    /// False when the input ends inside the tag.
    fn attributes(&mut self, kept: &mut Attributes<'a>) -> bool {
        let mut count = 0;
        loop {
            while let Some(byte) = self.peek() {
                if is_space(byte) || byte == b'/' {
                    self.at += 1;
                } else {
                    break;
                }
            }
            match self.peek() {
                None => return false,
                Some(b'>') => {
                    self.at += 1;
                    return true;
                }
                Some(_) => {}
            }
            // The name: a leading '=' belongs to it, as in HTML.
            let mut name = Name::default();
            name.push(self.input[self.at]);
            self.at += 1;
            while let Some(byte) = self.peek() {
                if is_space(byte) || matches!(byte, b'/' | b'>' | b'=') {
                    break;
                }
                name.push(byte);
                self.at += 1;
            }
            while self.peek().is_some_and(is_space) {
                self.at += 1;
            }
            let mut value: &'a [u8] = b"";
            if self.peek() == Some(b'=') {
                self.at += 1;
                while self.peek().is_some_and(is_space) {
                    self.at += 1;
                }
                match self.peek() {
                    Some(quote @ (b'"' | b'\'')) => {
                        self.at += 1;
                        let start = self.at;
                        let Some(offset) = self.rest().iter().position(|&byte| byte == quote)
                        else {
                            self.at = self.input.len();
                            return false;
                        };
                        value = &self.input[start..start + offset];
                        self.at = start + offset + 1;
                    }
                    // `name=>`: a missing value; the '>' ends the tag.
                    Some(b'>') | None => {}
                    Some(_) => {
                        let start = self.at;
                        while let Some(byte) = self.peek() {
                            if is_space(byte) || byte == b'>' {
                                break;
                            }
                            self.at += 1;
                        }
                        value = &self.input[start..self.at];
                    }
                }
            }
            count += 1;
            if count > MAX_ATTRIBUTES {
                continue;
            }
            let slot = if name.is(b"property") {
                &mut kept.property
            } else if name.is(b"name") {
                &mut kept.name
            } else if name.is(b"content") {
                &mut kept.content
            } else if name.is(b"charset") {
                &mut kept.charset
            } else if name.is(b"http-equiv") {
                &mut kept.http_equiv
            } else {
                continue;
            };
            if slot.is_none() {
                *slot = Some(clip(value));
            }
        }
    }

    fn meta(&mut self, attributes: &Attributes<'a>) {
        if self.found.declared_charset.is_none() {
            if let Some(label) = attributes.charset {
                self.found.declared_charset = Some(meta_utf8_label(label));
            } else if attributes
                .http_equiv
                .is_some_and(|value| trim(value).eq_ignore_ascii_case(b"content-type"))
                && let Some(label) = attributes.content.and_then(content_type_charset)
            {
                self.found.declared_charset = Some(meta_utf8_label(label));
            }
        }
        let Some(content) = attributes.content else {
            return;
        };
        let (key, by_name) = match (attributes.property, attributes.name) {
            (Some(property), _) => (trim(property), false),
            (None, Some(name)) => (trim(name), true),
            (None, None) => return,
        };
        let found = &mut self.found;
        let usable = |raw: &[u8], max_bytes: usize| !field(raw, max_bytes).is_empty();
        if key.eq_ignore_ascii_case(b"og:title") {
            if found.og_title.is_none() && usable(content, MAX_TITLE_BYTES) {
                found.og_title = Some(content);
            }
        } else if key.eq_ignore_ascii_case(b"og:description") {
            if found.og_description.is_none() && usable(content, MAX_DESCRIPTION_BYTES) {
                found.og_description = Some(content);
            }
        } else if key.eq_ignore_ascii_case(b"og:image") || key.eq_ignore_ascii_case(b"og:image:url")
        {
            if found.og_image.is_none() && image_reference(content).is_some() {
                found.og_image = Some(content);
            }
        } else if key.eq_ignore_ascii_case(b"og:image:alt") {
            if found.og_image_alt.is_none() {
                found.og_image_alt = Some(content);
            }
        } else if by_name
            && key.eq_ignore_ascii_case(b"description")
            && found.description.is_none()
            && usable(content, MAX_DESCRIPTION_BYTES)
        {
            found.description = Some(content);
        }
    }
}

fn clip(value: &[u8]) -> &[u8] {
    &value[..value.len().min(MAX_VALUE_BYTES)]
}

fn trim(value: &[u8]) -> &[u8] {
    let start = value
        .iter()
        .position(|&byte| !is_space(byte))
        .unwrap_or(value.len());
    let end = value
        .iter()
        .rposition(|&byte| !is_space(byte))
        .map_or(start, |end| end + 1);
    &value[start..end]
}

/// Linear substring search (the needles are at most three bytes long).
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let (&first, tail) = needle.split_first()?;
    let mut from = 0;
    while let Some(offset) = haystack[from..].iter().position(|&byte| byte == first) {
        let at = from + offset;
        if haystack[at + 1..].starts_with(tail) {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

/// The WHATWG labels of UTF-8.
fn utf8_label(label: &[u8]) -> bool {
    let label = trim(label);
    [
        &b"unicode-1-1-utf-8"[..],
        b"unicode11utf8",
        b"unicode20utf8",
        b"utf-8",
        b"utf8",
        b"x-unicode20utf8",
    ]
    .iter()
    .any(|known| label.eq_ignore_ascii_case(known))
}

/// A `<meta>` declaration of UTF-16 means UTF-8 (HTML's prescan: a document
/// whose meta tags are readable as ASCII is not UTF-16).
fn meta_utf8_label(label: &[u8]) -> bool {
    utf8_label(label)
        || [
            &b"unicodefffe"[..],
            b"utf-16be",
            b"csunicode",
            b"iso-10646-ucs-2",
            b"ucs-2",
            b"unicode",
            b"unicodefeff",
            b"utf-16",
            b"utf-16le",
        ]
        .iter()
        .any(|known| trim(label).eq_ignore_ascii_case(known))
}

/// The charset in `text/html; charset=…` (HTML's algorithm for extracting a
/// character encoding from a meta element).
fn content_type_charset(content: &[u8]) -> Option<&[u8]> {
    let mut from = 0;
    loop {
        let at = from
            + content[from..]
                .windows(7)
                .position(|window| window.eq_ignore_ascii_case(b"charset"))?;
        let mut rest = &content[at + 7..];
        while rest.first().is_some_and(|&byte| is_space(byte)) {
            rest = &rest[1..];
        }
        let Some(after_equals) = rest.strip_prefix(b"=") else {
            from = at + 7;
            continue;
        };
        let mut value = after_equals;
        while value.first().is_some_and(|&byte| is_space(byte)) {
            value = &value[1..];
        }
        return match value.first() {
            Some(&quote @ (b'"' | b'\'')) => {
                let value = &value[1..];
                let end = value.iter().position(|&byte| byte == quote)?;
                Some(&value[..end])
            }
            Some(_) => {
                let end = value
                    .iter()
                    .position(|&byte| is_space(byte) || byte == b';')
                    .unwrap_or(value.len());
                Some(&value[..end])
            }
            None => None,
        };
    }
}

/// Character references in text or attribute values, after lossy UTF-8
/// decoding. Writes at most [`MAX_DECODED_BYTES`] bytes.
fn decode_entities(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let mut out = String::with_capacity(text.len().min(MAX_DECODED_BYTES));
    let mut rest: &str = &text;
    while let Some(amp) = rest.find('&') {
        if !push_bounded(&mut out, &rest[..amp]) {
            return out;
        }
        rest = &rest[amp + 1..];
        let (decoded, used) = reference(rest);
        match decoded {
            Some(ch) => {
                if out.len() + ch.len_utf8() > MAX_DECODED_BYTES {
                    return out;
                }
                out.push(ch);
                rest = &rest[used..];
            }
            None => {
                if !push_bounded(&mut out, "&") {
                    return out;
                }
            }
        }
    }
    push_bounded(&mut out, rest);
    out
}

/// Append as much of `text` as fits; false when something did not.
fn push_bounded(out: &mut String, text: &str) -> bool {
    let room = MAX_DECODED_BYTES - out.len();
    if text.len() <= room {
        out.push_str(text);
        return true;
    }
    let mut end = room;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    out.push_str(&text[..end]);
    false
}

/// The character a reference after '&' stands for, and the bytes it used.
fn reference(text: &str) -> (Option<char>, usize) {
    let bytes = text.as_bytes();
    if let Some(numeric) = text.strip_prefix('#') {
        let (hex, digits_from) = match numeric.as_bytes().first() {
            Some(b'x' | b'X') => (true, 2),
            _ => (false, 1),
        };
        let digits = bytes[digits_from..]
            .iter()
            .take_while(|byte| {
                if hex {
                    byte.is_ascii_hexdigit()
                } else {
                    byte.is_ascii_digit()
                }
            })
            .count();
        if digits == 0 {
            return (None, 0);
        }
        let mut value: u32 = 0;
        for &byte in &bytes[digits_from..digits_from + digits] {
            let digit = char::from(byte).to_digit(16).expect("checked digit");
            // Saturates past the largest code point: still U+FFFD below.
            value = value
                .saturating_mul(if hex { 16 } else { 10 })
                .saturating_add(digit)
                .min(0x11_0000);
        }
        let mut used = digits_from + digits;
        if bytes.get(used) == Some(&b';') {
            used += 1;
        }
        return (Some(numeric_character(value)), used);
    }
    let name_len = bytes
        .iter()
        .take(32)
        .take_while(|byte| byte.is_ascii_alphanumeric())
        .count();
    if name_len == 0 || bytes.get(name_len) != Some(&b';') {
        return (None, 0);
    }
    match named(&text[..name_len]) {
        Some(ch) => (Some(ch), name_len + 1),
        None => (None, 0),
    }
}

/// HTML's numeric character reference rules: NUL, surrogates and values past
/// U+10FFFF become U+FFFD; 0x80–0x9F are read as windows-1252.
fn numeric_character(value: u32) -> char {
    const WINDOWS_1252: [char; 32] = [
        '\u{20ac}', '\u{81}', '\u{201a}', '\u{192}', '\u{201e}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{2c6}', '\u{2030}', '\u{160}', '\u{2039}', '\u{152}', '\u{8d}', '\u{17d}',
        '\u{8f}', '\u{90}', '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}', '\u{2022}',
        '\u{2013}', '\u{2014}', '\u{2dc}', '\u{2122}', '\u{161}', '\u{203a}', '\u{153}', '\u{9d}',
        '\u{17e}', '\u{178}',
    ];
    match value {
        0 => '\u{fffd}',
        0x80..=0x9f => WINDOWS_1252[(value - 0x80) as usize],
        _ => char::from_u32(value).unwrap_or('\u{fffd}'),
    }
}

fn named(name: &str) -> Option<char> {
    Some(match name {
        "amp" | "AMP" => '&',
        "lt" | "LT" => '<',
        "gt" | "GT" => '>',
        "quot" | "QUOT" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "copy" | "COPY" => '©',
        "reg" | "REG" => '®',
        "trade" => '™',
        "hellip" => '…',
        "mdash" => '—',
        "ndash" => '–',
        "lsquo" => '‘',
        "rsquo" => '’',
        "sbquo" => '‚',
        "ldquo" => '“',
        "rdquo" => '”',
        "bdquo" => '„',
        "laquo" => '«',
        "raquo" => '»',
        "lsaquo" => '‹',
        "rsaquo" => '›',
        "middot" => '·',
        "bull" => '•',
        "deg" => '°',
        "euro" => '€',
        "pound" => '£',
        "yen" => '¥',
        "cent" => '¢',
        "sect" => '§',
        "para" => '¶',
        "times" => '×',
        "divide" => '÷',
        "plusmn" => '±',
        "frac12" => '½',
        "auml" => 'ä',
        "ouml" => 'ö',
        "uuml" => 'ü',
        "Auml" => 'Ä',
        "Ouml" => 'Ö',
        "Uuml" => 'Ü',
        "szlig" => 'ß',
        "aacute" => 'á',
        "eacute" => 'é',
        "iacute" => 'í',
        "oacute" => 'ó',
        "uacute" => 'ú',
        "Eacute" => 'É',
        "agrave" => 'à',
        "egrave" => 'è',
        "ccedil" => 'ç',
        "ntilde" => 'ñ',
        "zwnj" => '\u{200c}',
        "zwj" => '\u{200d}',
        "shy" => '\u{ad}',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn extract(html: &str) -> Result<ExtractedMetadata, ExtractError> {
        extract_open_graph(html.as_bytes(), TransportCharset::Unspecified)
    }

    #[test]
    fn open_graph_first_property_wins_with_title_and_description_fallbacks() {
        let found = extract(
            r#"<!doctype html><html><head>
            <title>Fallback title</title>
            <meta name="description" content="Fallback description">
            <meta property="og:title" content="First &amp; best">
            <meta property="og:title" content="Second">
            <meta property="og:image" content="/img/a.png">
            <meta property="og:image" content="/img/b.png">
            <meta property="og:image:alt" content="An &quot;alt&quot;">
            </head><body>"#,
        )
        .unwrap();
        assert_eq!(found.title, "First & best");
        assert_eq!(found.description, "Fallback description");
        assert_eq!(found.image.as_deref(), Some("/img/a.png"));
        assert_eq!(found.image_alt, "An \"alt\"");

        let found = extract("<title>Only &lt;the&gt; title</title>").unwrap();
        assert_eq!(found.title, "Only <the> title");
        assert_eq!(found.description, "");
        assert_eq!(found.image, None);

        // og:image:url counts as og:image; the name attribute works too.
        let found = extract(
            r#"<meta name="og:image:url" content=" https://cdn.example/x.jpg ">
               <meta name="og:description" content="Desc">"#,
        )
        .unwrap();
        assert_eq!(found.image.as_deref(), Some("https://cdn.example/x.jpg"));
        assert_eq!(found.description, "Desc");
        assert_eq!(found.title, "");
    }

    #[test]
    fn canonical_and_other_providers_never_replace_anything() {
        assert_eq!(
            extract(
                r#"<meta property="og:url" content="https://evil.example/">
                   <meta property="og:site_name" content="Bank">
                   <meta name="twitter:title" content="T">
                   <link rel="canonical" href="https://evil.example/">
                   <base href="https://evil.example/">"#
            ),
            Err(ExtractError::NoMetadata)
        );
        // `description` only counts from the name attribute.
        assert_eq!(
            extract(r#"<meta property="description" content="x">"#),
            Err(ExtractError::NoMetadata)
        );
    }

    #[test]
    fn markup_in_scripts_comments_and_after_the_head_is_not_metadata() {
        for html in [
            r#"<script>document.write('<meta property="og:title" content="no">')</script>"#,
            r#"<SCRIPT type=x><meta property="og:title" content="no"></script >"#,
            r#"<style>/* <meta property="og:title" content="no"> */</style>"#,
            r#"<noscript><meta property="og:title" content="no"></noscript>"#,
            r#"<template><meta property="og:title" content="no"></template>"#,
            r#"<textarea><meta property="og:title" content="no"></textarea>"#,
            r#"<!-- <meta property="og:title" content="no"> -->"#,
            r#"<!-- a -- b --!> <!----><!-- <meta property="og:title" content="no"> -->"#,
            r#"</head><meta property="og:title" content="no">"#,
            r#"<body><meta property="og:title" content="no">"#,
            r#"<plaintext><meta property="og:title" content="no">"#,
            r#"<meta property="og:title" content="no""#,
            r#"<meta property="og:title" content="no"#,
            r#"<script><meta property="og:title" content="no">"#,
        ] {
            assert_eq!(extract(html), Err(ExtractError::NoMetadata), "{html}");
        }
        // `<!-->` and `<!--->` are complete (empty) comments in HTML.
        for html in [
            r#"<!--><meta property="og:title" content="yes">"#,
            r#"<!---><meta property="og:title" content="yes">"#,
            r#"<!-- x --!><meta property="og:title" content="yes">"#,
            r#"<!-- x ----><meta property="og:title" content="yes">"#,
        ] {
            assert_eq!(extract(html).unwrap().title, "yes", "{html}");
        }
        // The script ends at its end tag, not at a longer name.
        let found = extract(
            r#"<script>x</scriptx><meta property="og:title" content="no"></script>
               <meta property="og:title" content="yes">"#,
        )
        .unwrap();
        assert_eq!(found.title, "yes");
        // An attribute value may contain '>' and markup.
        let found =
            extract(r#"<meta content="a > <b> c" data-x='>' property="og:title">"#).unwrap();
        assert_eq!(found.title, "a > <b> c");
    }

    #[test]
    fn attributes_follow_html_tokenizing() {
        // Unquoted values, odd spacing, upper case and self-closing tags.
        let found = extract(
            "<META\nPROPERTY = og:title\tCONTENT=Unquoted/><meta property=og:description content=d>",
        )
        .unwrap();
        assert_eq!(found.title, "Unquoted/");
        assert_eq!(found.description, "d");
        // The first of duplicate attributes wins, as in HTML.
        let found =
            extract(r#"<meta property="og:title" property="x" content="a" content="b">"#).unwrap();
        assert_eq!(found.title, "a");
        // Attributes past the limit are ignored.
        let padding = "a ".repeat(MAX_ATTRIBUTES);
        assert_eq!(
            extract(&format!(
                r#"<meta {padding} property="og:title" content="late">"#
            )),
            Err(ExtractError::NoMetadata)
        );
        // An empty og:title gives way to a later one.
        let found =
            extract(r#"<meta property="og:title" content=" "><meta property="og:title" content="b">"#)
                .unwrap();
        assert_eq!(found.title, "b");
    }

    #[test]
    fn entities_whitespace_controls_and_bidi_are_normalised() {
        let found = extract(
            "<title>  A&#x20;&#32;&#9;b &#150; &eacute;&hellip; &unknown; &amp &#0; &#xD800; &#1114112; \u{1b}[31m \u{202e}x\u{2066}\n\n</title>",
        )
        .unwrap();
        assert_eq!(found.title, "A b – é… &unknown; &amp \u{fffd} \u{fffd} \u{fffd} [31m x");
        // Numeric references cannot smuggle controls or bidi overrides back in.
        let found = extract(r#"<meta property="og:title" content="a&#27;b&#x202E;c&#x7;">"#).unwrap();
        assert_eq!(found.title, "abc");
        // Invalid UTF-8 is replaced, not trusted.
        let found = extract_open_graph(
            b"<title>caf\xe9 \xff</title>",
            TransportCharset::Unspecified,
        )
        .unwrap();
        assert_eq!(found.title, "caf\u{fffd} \u{fffd}");
    }

    #[test]
    fn fields_are_cut_to_the_carried_limits_at_character_boundaries() {
        let long = "é".repeat(5000);
        let found = extract(&format!(
            r#"<meta property="og:title" content="{long}"><meta property="og:description" content="{long}"><meta property="og:image" content="/a"><meta property="og:image:alt" content="{long}">"#
        ))
        .unwrap();
        assert_eq!(found.title.len(), MAX_TITLE_BYTES);
        assert_eq!(found.description.len(), MAX_DESCRIPTION_BYTES);
        assert_eq!(found.image_alt.len(), MAX_ALT_BYTES);
        // An over-long or control-bearing image reference is dropped, not cut.
        let reference = format!("/{}", "a".repeat(MAX_URL_BYTES));
        let found = extract(&format!(
            r#"<title>t</title><meta property="og:image" content="{reference}"><meta property="og:image" content="/x&#1;y"><meta property="og:image:alt" content="orphan">"#
        ))
        .unwrap();
        assert_eq!(found.image, None);
        assert_eq!(found.image_alt, "");
        // A newline inside a reference is removed, as URL parsing does.
        let found = extract("<meta property=\"og:image\" content=\"/a\nb\">").unwrap();
        assert_eq!(found.image.as_deref(), Some("/ab"));
    }

    #[test]
    fn only_utf8_documents_are_read() {
        let page = r#"<meta charset="windows-1252"><meta property="og:title" content="x">"#;
        assert_eq!(extract(page), Err(ExtractError::Encoding));
        // The transport's charset wins over the document's.
        assert!(extract_open_graph(page.as_bytes(), TransportCharset::Utf8).is_ok());
        assert_eq!(
            extract_open_graph(b"<title>x</title>", TransportCharset::Other),
            Err(ExtractError::Encoding)
        );
        // A byte-order mark wins over both.
        let mut bom = b"\xef\xbb\xbf".to_vec();
        bom.extend_from_slice(page.as_bytes());
        assert!(extract_open_graph(&bom, TransportCharset::Other).is_ok());
        assert_eq!(
            extract_open_graph(b"\xff\xfe<\0t\0", TransportCharset::Utf8),
            Err(ExtractError::Encoding)
        );
        // The first declaration counts; UTF-16 in a meta tag means UTF-8.
        for (page, ok) in [
            (r#"<meta charset=" UTF-8 "><meta charset="koi8-r"><title>x</title>"#, true),
            (r#"<meta charset="utf-16le"><title>x</title>"#, true),
            (r#"<meta http-equiv="Content-Type" content="text/html; charset=utf8"><title>x</title>"#, true),
            (r#"<meta http-equiv="content-type" content='text/html;charset="Shift_JIS"'><title>x</title>"#, false),
            (r#"<meta http-equiv="refresh" content="0; charset=koi8-r"><title>x</title>"#, true),
        ] {
            assert_eq!(extract(page).is_ok(), ok, "{page}");
        }
        assert_eq!(TransportCharset::from_label(None), TransportCharset::Unspecified);
        assert_eq!(TransportCharset::from_label(Some("UTF-8")), TransportCharset::Utf8);
        assert_eq!(TransportCharset::from_label(Some("latin1")), TransportCharset::Other);
    }

    #[test]
    fn documents_over_the_cap_and_tag_floods_stop() {
        assert_eq!(
            extract_open_graph(&vec![b' '; MAX_DOCUMENT_BYTES + 1], TransportCharset::Utf8),
            Err(ExtractError::TooLarge)
        );
        let before_limit = format!(
            "{}<meta property=\"og:title\" content=\"late\">",
            "<p>".repeat(MAX_TAGS)
        );
        assert_eq!(extract(&before_limit), Err(ExtractError::NoMetadata));
        let metas = format!(
            "{}<meta property=\"og:title\" content=\"late\">",
            "<meta>".repeat(MAX_META_TAGS)
        );
        assert_eq!(extract(&metas), Err(ExtractError::NoMetadata));
        // Within the limits it is still found.
        let within = format!(
            "{}<meta property=\"og:title\" content=\"in\">",
            "<meta>".repeat(MAX_META_TAGS - 1)
        );
        assert_eq!(extract(&within).unwrap().title, "in");
    }

    #[test]
    fn hostile_documents_take_linear_time() {
        let size = MAX_DOCUMENT_BYTES;
        let fill = |unit: &str| unit.repeat(size / unit.len());
        let one_tag = format!("<meta {}", "a ".repeat(size / 2 - 4));
        let many_attrs = format!("<meta {}>", "a=b ".repeat(size / 4 - 4));
        let title_entities = format!("<title>{}", fill("&#x41;"));
        let long_value = format!("<meta content=\"{}", "x".repeat(size - 20));
        for document in [
            fill("<meta "),
            fill("<"),
            fill("</"),
            fill("<!"),
            fill("<!-"),
            fill("<a"),
            fill("<div>"),
            fill("</x>"),
            fill("<script>"),
            fill("</scrip"),
            fill("&#"),
            fill("&amp"),
            fill("\0"),
            format!("<!--{}", fill("-")),
            format!("<script>{}", fill("</scrip")),
            format!("<title>{}", fill("<")),
            one_tag,
            many_attrs,
            title_entities,
            long_value,
            fill("<meta http-equiv=content-type content='charset charset charset'>"),
        ] {
            let document = &document.as_bytes()[..document.len().min(size)];
            let started = Instant::now();
            let _ = extract_open_graph(document, TransportCharset::Unspecified);
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "{:?} for {:?}",
                started.elapsed(),
                String::from_utf8_lossy(&document[..16])
            );
        }
    }

    #[test]
    fn arbitrary_bytes_never_panic_and_stay_within_limits() {
        // A deterministic byte soup biased towards markup.
        let alphabet = b"<>/!-=\"' \n\tmetapropertycontentog:titleimage&#x;0129\xc3\xa9\xff\0";
        let mut state = 0x2545_f491_u32;
        for round in 0..2000 {
            let len = (round * 37) % 2048;
            let document: Vec<u8> = (0..len)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    alphabet[state as usize % alphabet.len()]
                })
                .collect();
            if let Ok(found) = extract_open_graph(&document, TransportCharset::Unspecified) {
                assert!(found.title.len() <= MAX_TITLE_BYTES);
                assert!(found.description.len() <= MAX_DESCRIPTION_BYTES);
                assert!(found.image_alt.len() <= MAX_ALT_BYTES);
                assert!(found.image.as_ref().is_none_or(|image| image.len() <= MAX_URL_BYTES));
                for text in [&found.title, &found.description, &found.image_alt] {
                    assert!(crate::text::is_normalized(text, usize::MAX));
                }
            }
        }
    }
}
