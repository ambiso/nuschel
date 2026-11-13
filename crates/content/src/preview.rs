// SPDX-License-Identifier: EUPL-1.2
//! Open Graph v1 metadata from an already-fetched HTML document, for the link
//! preview a sender is composing.
//!
//! The document is untrusted and page-controlled: only the content worker's
//! isolated process (or the content WASM worker) calls this. It runs
//! html5ever's tokenizer alone, with no tree, script, style or resource, and
//! switches it to the raw-text states for `script`, `style`, `noscript` and
//! similar elements as the tree builder would, so markup-looking text inside
//! them is never read; `<template>` content is skipped too. Scanning stops at
//! the first `<body>` or `</head>`, or at the tag and `<meta>` limits.
//!
//! The result is the bounded `nuschl_link_preview::metadata::ExtractedMetadata`.
//! Its image reference is page-controlled: the supervisor resolves it against
//! the fetched document URL with `policy::Target::join`, which applies the
//! full target policy again.
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::states::RawKind;
use html5ever::tokenizer::{
    BufferQueue, Tag, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer,
};
/// The bounded result types, so the content worker needs no link-preview
/// dependency of its own.
pub use nuschl_link_preview::metadata::{
    ExtractedMetadata, MAX_DOCUMENT_BYTES, TransportCharset, Unavailable,
};
use nuschl_link_preview::model::{MAX_ALT_BYTES, MAX_DESCRIPTION_BYTES, MAX_TITLE_BYTES};
use nuschl_link_preview::text::{normalize, refused};
use std::cell::RefCell;

/// Scanning stops after this many tags, comments and doctypes.
pub(crate) const MAX_TAGS: usize = 4096;
/// Scanning stops after this many `<meta>` start tags.
pub(crate) const MAX_META_TAGS: usize = 256;
/// At most this many bytes of an attribute value or of the title are read.
pub(crate) const MAX_VALUE_BYTES: usize = 4096;
/// The tokenizer is fed this much at a time, so it stops soon after the
/// sink has seen enough.
const CHUNK_BYTES: usize = 4096;
pub const MAX_SOURCE_IMAGE_BYTES: usize = 2 * 1024 * 1024;

/// A canonical derivative and pixels decoded from those exact carried bytes.
#[cfg(feature = "images")]
pub struct PreparedImage {
    pub image: nuschl_core::preview::EmbeddedImage,
    pub rgba: Vec<u8>,
}

/// Decode only inside the content role; no page resource survives re-encoding.
#[cfg(feature = "images")]
pub fn prepare_image(bytes: &[u8], alt: &str) -> Result<PreparedImage, Unavailable> {
    use nuschl_core::preview::{
        EmbeddedImage, ImageMime, MAX_IMAGE_BYTES, MAX_IMAGE_HEIGHT, MAX_IMAGE_WIDTH,
    };
    if bytes.is_empty() || bytes.len() > MAX_SOURCE_IMAGE_BYTES || alt.len() > MAX_ALT_BYTES {
        return Err(Unavailable::TooLarge);
    }
    let formats = [
        image::ImageFormat::Png,
        image::ImageFormat::Jpeg,
        image::ImageFormat::Gif,
        image::ImageFormat::WebP,
    ];
    let source = super::decode(
        bytes,
        &formats,
        super::MAX_SOURCE_DIMENSION,
        64 * 1024 * 1024,
    )
    .ok_or(Unavailable::Malformed)?;
    let mut candidate = super::shrink(source, MAX_IMAGE_WIDTH, MAX_IMAGE_HEIGHT);
    let alt = normalize(alt, MAX_ALT_BYTES);
    // Each unsuccessful round strictly shrinks both dimensions. This bounds
    // encoding work as well as the eventual encoded and decoded reply sizes.
    for _ in 0..8 {
        let png = super::encode_png(&candidate).map_err(|_| Unavailable::Malformed)?;
        let chosen = if png.len() <= MAX_IMAGE_BYTES {
            Some((ImageMime::Png, png))
        } else {
            let opaque = super::flatten(&candidate);
            let mut chosen = None;
            for quality in [85, 70, 55] {
                let jpeg =
                    super::encode_jpeg(&opaque, quality).map_err(|_| Unavailable::Malformed)?;
                if jpeg.len() <= MAX_IMAGE_BYTES {
                    chosen = Some((ImageMime::Jpeg, jpeg));
                    break;
                }
            }
            chosen
        };
        if let Some((mime, data)) = chosen {
            let image = EmbeddedImage {
                mime,
                data,
                width: candidate.width(),
                height: candidate.height(),
                alt,
            };
            // JPEG preparation can be lossy: the composer receives the exact
            // carried derivative's pixels, rather than the pre-encode source.
            let rgba = render_image(&image)?;
            return Ok(PreparedImage { image, rgba });
        }
        let width = candidate.width() * 3 / 4;
        let height = candidate.height() * 3 / 4;
        if width == 0 || height == 0 {
            break;
        }
        candidate = candidate.resize(width, height, image::imageops::FilterType::Triangle);
    }
    Err(Unavailable::TooLarge)
}

/// Decode a carried image without recompressing it. Header dimensions are
/// checked before decoding and actual dimensions/byte count after decoding.
#[cfg(feature = "images")]
pub fn render_image(image: &nuschl_core::preview::EmbeddedImage) -> Result<Vec<u8>, Unavailable> {
    use nuschl_core::preview::{ImageMime, MAX_IMAGE_WIDTH};
    nuschl_link_preview::card::check_image(image).map_err(|_| Unavailable::Malformed)?;
    let format = match image.mime {
        ImageMime::Png => image::ImageFormat::Png,
        ImageMime::Jpeg => image::ImageFormat::Jpeg,
    };
    let decoded = super::decode(&image.data, &[format], MAX_IMAGE_WIDTH, 64 * 1024 * 1024)
        .ok_or(Unavailable::Malformed)?;
    if decoded.width() != image.width || decoded.height() != image.height {
        return Err(Unavailable::Malformed);
    }
    let rgba = decoded.to_rgba8().into_raw();
    let expected = usize::try_from(
        image
            .width
            .checked_mul(image.height)
            .and_then(|n| n.checked_mul(4))
            .ok_or(Unavailable::Malformed)?,
    )
    .map_err(|_| Unavailable::Malformed)?;
    if rgba.len() != expected {
        return Err(Unavailable::Malformed);
    }
    Ok(rgba)
}

/// Classify the HTTP `charset` parameter's label (`None` when absent).
pub fn transport_charset(label: Option<&str>) -> TransportCharset {
    match label {
        None => TransportCharset::Unspecified,
        Some(label) if utf8_label(label) => TransportCharset::Utf8,
        Some(_) => TransportCharset::Other,
    }
}

/// Open Graph v1: the first usable `og:title`, `og:description` and
/// `og:image` (or `og:image:url`) win, with the `og:image:alt` that follows
/// the chosen image; the `<title>` text and `meta[name=description]` are
/// fallbacks. `og:url`, `og:site_name`,
/// `twitter:*`, canonical links and `<base>` are ignored: nothing can replace
/// the sender's URL. Only UTF-8 is read: a byte-order mark, the transport's
/// charset or the first `<meta>` declaration naming anything else means no
/// preview.
///
/// Cost: linear in the document, except that html5ever compares each new
/// attribute name with the earlier ones in the same tag, so one tag full of
/// distinct names is quadratic up to the 256 KiB cap (about 66 000 names);
/// the content worker's CPU and wall limits bound it.
pub fn extract_open_graph(
    document: &[u8],
    charset: TransportCharset,
) -> Result<ExtractedMetadata, Unavailable> {
    if document.len() > MAX_DOCUMENT_BYTES {
        return Err(Unavailable::TooLarge);
    }
    // A byte-order mark decides the encoding over everything else.
    let (document, charset) = match document {
        [0xef, 0xbb, 0xbf, rest @ ..] => (rest, TransportCharset::Utf8),
        [0xfe, 0xff, ..] | [0xff, 0xfe, ..] => return Err(Unavailable::Encoding),
        _ => (document, charset),
    };
    if charset == TransportCharset::Other {
        return Err(Unavailable::Encoding);
    }
    let found = scan(&String::from_utf8_lossy(document));
    if charset == TransportCharset::Unspecified && found.declared_charset == Some(false) {
        return Err(Unavailable::Encoding);
    }
    let field = |raw: Option<String>, limit| normalize(raw.as_deref().unwrap_or(""), limit);
    let title = field(found.og_title.or(found.title), MAX_TITLE_BYTES);
    let description = field(
        found.og_description.or(found.description),
        MAX_DESCRIPTION_BYTES,
    );
    let image = found.og_image.as_deref().and_then(image_reference);
    if title.is_empty() && description.is_empty() && image.is_none() {
        return Err(Unavailable::NoMetadata);
    }
    // Alternative text describes the chosen image only.
    let image_alt = match image {
        Some(_) => field(found.og_image_alt, MAX_ALT_BYTES),
        None => String::new(),
    };
    Ok(ExtractedMetadata {
        title,
        description,
        image,
        image_alt,
    })
}

/// A usable `og:image` reference: trimmed, without controls (tabs and line
/// breaks are removed, as URL parsing does), and not longer than a URL may
/// be (an over-long reference is dropped, never cut).
fn image_reference(raw: &str) -> Option<String> {
    let reference: String = raw
        .trim_matches(|ch: char| ch.is_ascii_whitespace())
        .chars()
        .filter(|ch| !matches!(ch, '\t' | '\n' | '\r'))
        .collect();
    if reference.is_empty()
        || reference.len() > nuschl_link_preview::model::MAX_URL_BYTES
        || reference.chars().any(refused)
    {
        return None;
    }
    Some(reference)
}

/// The raw values found, each at most [`MAX_VALUE_BYTES`].
#[derive(Default)]
struct Found {
    og_title: Option<String>,
    og_description: Option<String>,
    og_image: Option<String>,
    og_image_alt: Option<String>,
    /// The most recent `og:image` value, and whether it is the chosen one:
    /// `og:image:alt` describes the image just before it.
    latest_image: Option<String>,
    latest_is_chosen: bool,
    title: Option<String>,
    description: Option<String>,
    /// The first charset declaration: whether it names UTF-8.
    declared_charset: Option<bool>,
}

#[derive(Default)]
struct State {
    found: Found,
    tags: usize,
    metas: usize,
    /// Open `<template>` elements: their content is not the document's.
    templates: usize,
    /// The first `<title>`'s text while it is read.
    title: Option<String>,
    done: bool,
}

#[derive(Default)]
struct Sink(RefCell<State>);

impl TokenSink for Sink {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        let mut state = self.0.borrow_mut();
        if state.done {
            return TokenSinkResult::Continue;
        }
        match token {
            Token::CharacterTokens(text) => {
                if let Some(title) = &mut state.title {
                    push_clipped(title, &text);
                }
                TokenSinkResult::Continue
            }
            Token::TagToken(tag) => state.tag(&tag),
            Token::CommentToken(_) | Token::DoctypeToken(_) => {
                state.count();
                TokenSinkResult::Continue
            }
            _ => TokenSinkResult::Continue,
        }
    }
}

impl State {
    fn count(&mut self) {
        self.tags += 1;
        if self.tags > MAX_TAGS {
            self.done = true;
        }
    }

    fn tag(&mut self, tag: &Tag) -> TokenSinkResult<()> {
        self.count();
        if self.done {
            return TokenSinkResult::Continue;
        }
        let name: &str = &tag.name;
        match tag.kind {
            TagKind::EndTag => match name {
                "head" => self.done = true,
                "template" => self.templates = self.templates.saturating_sub(1),
                "title" => self.end_title(),
                _ => {}
            },
            TagKind::StartTag => match name {
                "body" => self.done = true,
                "plaintext" => {
                    self.done = true;
                    return TokenSinkResult::Plaintext;
                }
                "template" => self.templates += 1,
                "meta" => {
                    self.metas += 1;
                    if self.metas > MAX_META_TAGS {
                        self.done = true;
                    } else if self.templates == 0 {
                        self.meta(tag);
                    }
                }
                "title" => {
                    if self.templates == 0 && self.found.title.is_none() && self.title.is_none() {
                        self.title = Some(String::new());
                    }
                    return TokenSinkResult::RawData(RawKind::Rcdata);
                }
                "textarea" => return TokenSinkResult::RawData(RawKind::Rcdata),
                "script" => return TokenSinkResult::RawData(RawKind::ScriptData),
                "style" | "xmp" | "iframe" | "noembed" | "noframes" | "noscript" => {
                    return TokenSinkResult::RawData(RawKind::Rawtext);
                }
                _ => {}
            },
        }
        TokenSinkResult::Continue
    }

    fn end_title(&mut self) {
        if let Some(text) = self.title.take() {
            self.found.title.get_or_insert(text);
        }
    }

    fn meta(&mut self, tag: &Tag) {
        let mut property = None;
        let mut name = None;
        let mut content = None;
        let mut charset = None;
        let mut http_equiv = None;
        // The tokenizer has already dropped duplicate attributes.
        for attribute in &tag.attrs {
            let slot = match &*attribute.name.local {
                "property" => &mut property,
                "name" => &mut name,
                "content" => &mut content,
                "charset" => &mut charset,
                "http-equiv" => &mut http_equiv,
                _ => continue,
            };
            *slot = Some(clip(&attribute.value));
        }
        if self.found.declared_charset.is_none() {
            if let Some(label) = charset {
                self.found.declared_charset = Some(meta_utf8_label(label));
            } else if http_equiv
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("content-type"))
                && let Some(label) = content.and_then(content_type_charset)
            {
                self.found.declared_charset = Some(meta_utf8_label(label));
            }
        }
        let Some(content) = content else {
            return;
        };
        let (key, by_name) = match (property, name) {
            (Some(property), _) => (property.trim(), false),
            (None, Some(name)) => (name.trim(), true),
            (None, None) => return,
        };
        let found = &mut self.found;
        let text = |limit| !normalize(content, limit).is_empty();
        let slot = if key.eq_ignore_ascii_case("og:title") {
            Some((&mut found.og_title, text(MAX_TITLE_BYTES)))
        } else if key.eq_ignore_ascii_case("og:description") {
            Some((&mut found.og_description, text(MAX_DESCRIPTION_BYTES)))
        } else if key.eq_ignore_ascii_case("og:image") || key.eq_ignore_ascii_case("og:image:url") {
            // `og:image:url` repeating the current image describes that image.
            let same = key.eq_ignore_ascii_case("og:image:url")
                && found.latest_image.as_deref() == Some(content);
            if !same {
                let chosen = found.og_image.is_none() && image_reference(content).is_some();
                if chosen {
                    found.og_image = Some(content.to_owned());
                }
                found.latest_image = Some(content.to_owned());
                found.latest_is_chosen = chosen;
            }
            None
        } else if key.eq_ignore_ascii_case("og:image:alt") {
            Some((&mut found.og_image_alt, found.latest_is_chosen))
        } else if by_name && key.eq_ignore_ascii_case("description") {
            Some((&mut found.description, text(MAX_DESCRIPTION_BYTES)))
        } else {
            None
        };
        // The first usable value wins.
        if let Some((slot, usable)) = slot
            && slot.is_none()
            && usable
        {
            *slot = Some(content.to_owned());
        }
    }
}

/// Feed the tokenizer in chunks and stop once the sink is done.
fn scan(text: &str) -> Found {
    let tokenizer = Tokenizer::new(Sink::default(), Default::default());
    let input = BufferQueue::default();
    let mut rest = text;
    let mut stopped = false;
    while !rest.is_empty() {
        let mut end = rest.len().min(CHUNK_BYTES);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        input.push_back(StrTendril::from(&rest[..end]));
        rest = &rest[end..];
        let _ = tokenizer.feed(&input);
        if tokenizer.sink.0.borrow().done {
            stopped = true;
            break;
        }
    }
    if !stopped {
        tokenizer.end();
    }
    let mut state = tokenizer.sink.0.take();
    // A title still open at the end of the input ends there.
    state.end_title();
    state.found
}

fn clip(value: &str) -> &str {
    let mut end = value.len().min(MAX_VALUE_BYTES);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// Append what fits of `text` within [`MAX_VALUE_BYTES`], at a character
/// boundary.
fn push_clipped(out: &mut String, text: &str) {
    let mut end = text.len().min(MAX_VALUE_BYTES.saturating_sub(out.len()));
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    out.push_str(&text[..end]);
}

/// The WHATWG labels of UTF-8.
fn utf8_label(label: &str) -> bool {
    let label = label.trim_matches(|ch: char| ch.is_ascii_whitespace());
    [
        "unicode-1-1-utf-8",
        "unicode11utf8",
        "unicode20utf8",
        "utf-8",
        "utf8",
        "x-unicode20utf8",
    ]
    .iter()
    .any(|known| label.eq_ignore_ascii_case(known))
}

/// A `<meta>` declaration of UTF-16 means UTF-8 (HTML's prescan: a document
/// whose meta tags are readable as ASCII is not UTF-16).
fn meta_utf8_label(label: &str) -> bool {
    let trimmed = label.trim_matches(|ch: char| ch.is_ascii_whitespace());
    utf8_label(label)
        || [
            "unicodefffe",
            "utf-16be",
            "csunicode",
            "iso-10646-ucs-2",
            "ucs-2",
            "unicode",
            "unicodefeff",
            "utf-16",
            "utf-16le",
        ]
        .iter()
        .any(|known| trimmed.eq_ignore_ascii_case(known))
}

/// The charset in `text/html; charset=…` (HTML's algorithm for extracting a
/// character encoding from a meta element).
fn content_type_charset(content: &str) -> Option<&str> {
    let lower = content.to_ascii_lowercase();
    let mut from = 0;
    loop {
        let at = from + lower[from..].find("charset")?;
        let rest = content[at + 7..].trim_start_matches(|ch: char| ch.is_ascii_whitespace());
        let Some(value) = rest.strip_prefix('=') else {
            from = at + 7;
            continue;
        };
        let value = value.trim_start_matches(|ch: char| ch.is_ascii_whitespace());
        return match value.chars().next() {
            Some(quote @ ('"' | '\'')) => {
                let value = &value[1..];
                value.find(quote).map(|end| &value[..end])
            }
            Some(_) => Some(
                value
                    .split(|ch: char| ch.is_ascii_whitespace() || ch == ';')
                    .next()
                    .unwrap_or(""),
            ),
            None => None,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuschl_link_preview::model::MAX_URL_BYTES;
    use std::time::{Duration, Instant};

    fn extract(html: &str) -> Result<ExtractedMetadata, Unavailable> {
        extract_open_graph(html.as_bytes(), TransportCharset::Unspecified)
    }

    #[test]
    #[cfg(feature = "images")]
    fn prepared_noisy_image_fits_the_carried_budget_and_pixels_match_exact_bytes() {
        use nuschl_core::preview::{ImageMime, MAX_IMAGE_BYTES, MAX_IMAGE_HEIGHT, MAX_IMAGE_WIDTH};
        let mut state = 0x2545_f491_u32;
        let source = image::RgbImage::from_fn(512, 512, |_, _| {
            let mut channels = [0; 3];
            for channel in &mut channels {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                *channel = state.to_le_bytes()[0];
            }
            image::Rgb(channels)
        });
        let encoded = super::super::encode_png(&image::DynamicImage::ImageRgb8(source)).unwrap();
        let prepared = prepare_image(&encoded, " Noisy\nimage ").unwrap();
        assert!(prepared.image.data.len() <= MAX_IMAGE_BYTES);
        assert!(
            prepared.image.width <= MAX_IMAGE_WIDTH && prepared.image.height <= MAX_IMAGE_HEIGHT
        );
        assert_eq!(prepared.image.mime, ImageMime::Jpeg);
        assert_eq!(prepared.image.alt, "Noisy image");
        assert_eq!(render_image(&prepared.image).unwrap(), prepared.rgba);
        assert_eq!(
            prepared.rgba.len(),
            usize::try_from(prepared.image.width * prepared.image.height * 4).unwrap()
        );
    }

    #[test]
    #[cfg(feature = "images")]
    fn source_image_caps_formats_and_dimensions_refuse_before_preparation() {
        assert!(matches!(
            prepare_image(&vec![0; MAX_SOURCE_IMAGE_BYTES + 1], ""),
            Err(Unavailable::TooLarge)
        ));
        assert!(matches!(
            prepare_image(b"<svg></svg>", ""),
            Err(Unavailable::Malformed)
        ));
        use base64::Engine as _;
        let oversized = base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAE4gAAAABCAIAAAC9c9PfAAAAJUlEQVR4nO3BgQAAAADDoPlTX+EAVQEAAAAAAAAAAAAAAAAAwGE6mQAB52yShgAAAABJRU5ErkJggg==").unwrap();
        assert!(matches!(
            prepare_image(&oversized, ""),
            Err(Unavailable::Malformed)
        ));
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
            <meta property="og:image:alt" content="An &quot;alt&quot;">
            <meta property="og:image" content="/img/b.png">
            <meta property="og:image:alt" content="b's alt">
            </head><body>"#,
        )
        .unwrap();
        assert_eq!(found.title, "First & best");
        assert_eq!(found.description, "Fallback description");
        assert_eq!(found.image.as_deref(), Some("/img/a.png"));
        assert_eq!(found.image_alt, "An \"alt\"");
        assert!(found.is_bounded());

        let found = extract("<title>Only &lt;the&gt; title</title>").unwrap();
        assert_eq!(found.title, "Only <the> title");
        assert_eq!((found.description.as_str(), found.image), ("", None));

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
            Err(Unavailable::NoMetadata)
        );
        // `description` only counts from the name attribute.
        assert_eq!(
            extract(r#"<meta property="description" content="x">"#),
            Err(Unavailable::NoMetadata)
        );
    }

    #[test]
    fn markup_in_raw_text_comments_templates_and_after_the_head_is_not_metadata() {
        for html in [
            r#"<script>document.write('<meta property="og:title" content="no">')</script>"#,
            r#"<SCRIPT type=x><meta property="og:title" content="no"></script >"#,
            r#"<style>/* <meta property="og:title" content="no"> */</style>"#,
            r#"<noscript><meta property="og:title" content="no"></noscript>"#,
            r#"<template><meta property="og:title" content="no"></template>"#,
            r#"<template><template></template><meta property="og:title" content="no"></template>"#,
            r#"<textarea><meta property="og:title" content="no"></textarea>"#,
            r#"<iframe><meta property="og:title" content="no"></iframe>"#,
            r#"<!-- <meta property="og:title" content="no"> -->"#,
            r#"</head><meta property="og:title" content="no">"#,
            r#"<body><meta property="og:title" content="no">"#,
            r#"<plaintext><meta property="og:title" content="no">"#,
            r#"<meta property="og:title" content="no""#,
            r#"<meta property="og:title" content="no"#,
            r#"<script><meta property="og:title" content="no">"#,
        ] {
            assert_eq!(extract(html), Err(Unavailable::NoMetadata), "{html}");
        }
        // HTML's comment and raw-text rules decide where those end.
        for (html, title) in [
            (r#"<!--><meta property="og:title" content="yes">"#, "yes"),
            (
                r#"<!-- x --!><meta property="og:title" content="yes">"#,
                "yes",
            ),
            (
                r#"<script>x</scriptx><meta property="og:title" content="no"></script><meta property="og:title" content="yes">"#,
                "yes",
            ),
            (
                r#"<meta content="a > <b> c" data-x='>' property="og:title"><meta property="og:title" content="no">"#,
                "a > <b> c",
            ),
        ] {
            assert_eq!(extract(html).unwrap().title, title, "{html}");
        }
        // A title inside a template is not the document's.
        let found = extract("<template><title>no</title></template><title>yes</title>").unwrap();
        assert_eq!(found.title, "yes");
        // A title cut off by the end of the input ends there.
        assert_eq!(extract("<title>open").unwrap().title, "open");
    }

    #[test]
    fn attributes_follow_html_tokenizing() {
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
        // However many attributes come first.
        let padding: String = (0..1000).map(|n| format!("a{n} ")).collect();
        assert_eq!(
            extract(&format!(
                r#"<meta {padding}property="og:title" content="in">"#
            ))
            .unwrap()
            .title,
            "in"
        );
        // An empty og:title gives way to a later one.
        let found = extract(
            r#"<meta property="og:title" content=" "><meta property="og:title" content="b">"#,
        )
        .unwrap();
        assert_eq!(found.title, "b");
    }

    #[test]
    fn alternative_text_belongs_to_the_chosen_image() {
        let long = format!("/{}", "a".repeat(MAX_URL_BYTES));
        for (html, image, alt) in [
            // The first image is over-long and dropped: its alt goes with it.
            (
                format!(
                    r#"<meta property="og:image" content="{long}"><meta property="og:image:alt" content="dropped"><meta property="og:image" content="/b"><meta property="og:title" content="t">"#
                ),
                Some("/b"),
                "",
            ),
            (
                format!(
                    r#"<meta property="og:image" content="{long}"><meta property="og:image:alt" content="dropped"><meta property="og:image" content="/b"><meta property="og:image:alt" content="b">"#
                ),
                Some("/b"),
                "b",
            ),
            // Before any image, alternative text describes nothing.
            (
                r#"<meta property="og:image:alt" content="early"><meta property="og:image" content="/a">"#.into(),
                Some("/a"),
                "",
            ),
            // og:image:url repeating the chosen image is the same image.
            (
                r#"<meta property="og:image" content="/a"><meta property="og:image:url" content="/a"><meta property="og:image:alt" content="a">"#.into(),
                Some("/a"),
                "a",
            ),
            // Only the first alt of the chosen image counts.
            (
                r#"<meta property="og:image" content="/a"><meta property="og:image:alt" content="one"><meta property="og:image:alt" content="two">"#.into(),
                Some("/a"),
                "one",
            ),
        ] {
            let found = extract(&html).unwrap();
            assert_eq!(found.image.as_deref(), image, "{html}");
            assert_eq!(found.image_alt, alt, "{html}");
        }
    }

    #[test]
    fn entities_whitespace_controls_and_bidi_are_normalised() {
        let found = extract(
            "<title>  A&#x20;&#32;&#9;b &#150; &eacute;&hellip; &unknown; &#0; &#xD800; &#1114112; \u{1b}[31m \u{202e}x\u{2066}\n\n</title>",
        )
        .unwrap();
        assert_eq!(
            found.title,
            "A b – é… &unknown; \u{fffd} \u{fffd} \u{fffd} [31m x"
        );
        // References cannot smuggle controls or bidi overrides back in.
        let found =
            extract(r#"<meta property="og:title" content="a&#27;b&#x202E;c&#x7;">"#).unwrap();
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
        assert!(found.is_bounded());
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
        // A long title text is read up to the value limit.
        let found = extract(&format!("<title>{}</title>", "t".repeat(100_000))).unwrap();
        assert_eq!(found.title.len(), MAX_TITLE_BYTES);
    }

    #[test]
    fn only_utf8_documents_are_read() {
        let page = r#"<meta charset="windows-1252"><meta property="og:title" content="x">"#;
        assert_eq!(extract(page), Err(Unavailable::Encoding));
        // The transport's charset wins over the document's.
        assert!(extract_open_graph(page.as_bytes(), TransportCharset::Utf8).is_ok());
        assert_eq!(
            extract_open_graph(b"<title>x</title>", TransportCharset::Other),
            Err(Unavailable::Encoding)
        );
        // A byte-order mark wins over both.
        let mut bom = b"\xef\xbb\xbf".to_vec();
        bom.extend_from_slice(page.as_bytes());
        assert!(extract_open_graph(&bom, TransportCharset::Other).is_ok());
        assert_eq!(
            extract_open_graph(b"\xff\xfe<\0t\0", TransportCharset::Utf8),
            Err(Unavailable::Encoding)
        );
        // The first declaration counts; UTF-16 in a meta tag means UTF-8.
        for (page, ok) in [
            (
                r#"<meta charset=" UTF-8 "><meta charset="koi8-r"><title>x</title>"#,
                true,
            ),
            (r#"<meta charset="utf-16le"><title>x</title>"#, true),
            (
                r#"<meta http-equiv="Content-Type" content="text/html; charset=utf8"><title>x</title>"#,
                true,
            ),
            (
                r#"<meta http-equiv="content-type" content='text/html;charset="Shift_JIS"'><title>x</title>"#,
                false,
            ),
            (
                r#"<meta http-equiv="refresh" content="0; charset=koi8-r"><title>x</title>"#,
                true,
            ),
        ] {
            assert_eq!(extract(page).is_ok(), ok, "{page}");
        }
        assert_eq!(transport_charset(None), TransportCharset::Unspecified);
        assert_eq!(transport_charset(Some("UTF-8")), TransportCharset::Utf8);
        assert_eq!(transport_charset(Some("latin1")), TransportCharset::Other);
    }

    #[test]
    fn documents_over_the_cap_and_tag_floods_stop() {
        assert_eq!(
            extract_open_graph(&vec![b' '; MAX_DOCUMENT_BYTES + 1], TransportCharset::Utf8),
            Err(Unavailable::TooLarge)
        );
        let late = "<meta property=\"og:title\" content=\"late\">";
        assert_eq!(
            extract(&format!("{}{late}", "<p>".repeat(MAX_TAGS))),
            Err(Unavailable::NoMetadata)
        );
        assert_eq!(
            extract(&format!("{}{late}", "<meta>".repeat(MAX_META_TAGS))),
            Err(Unavailable::NoMetadata)
        );
        // Within the limits it is still found.
        let within = format!(
            "{}<meta property=\"og:title\" content=\"in\">",
            "<meta>".repeat(MAX_META_TAGS - 1)
        );
        assert_eq!(extract(&within).unwrap().title, "in");
    }

    #[test]
    fn hostile_documents_take_bounded_time() {
        let size = MAX_DOCUMENT_BYTES;
        let fill = |unit: &str| unit.repeat(size / unit.len());
        for document in [
            fill("<meta "),
            fill("<"),
            fill("</"),
            fill("<!"),
            fill("<a"),
            fill("<div>"),
            fill("</x>"),
            fill("<script>"),
            fill("&#"),
            fill("&amp"),
            fill("\0"),
            format!("<!--{}", fill("-")),
            format!("<script>{}", fill("</scrip")),
            format!("<title>{}", fill("&#x41;")),
            format!("<meta {}>", "a=b ".repeat(size / 4 - 4)),
            format!("<meta content=\"{}", "x".repeat(size - 20)),
            fill("<meta http-equiv=content-type content='charset charset charset'>"),
        ] {
            let document = &document.as_bytes()[..document.len().min(size)];
            let started = Instant::now();
            let _ = extract_open_graph(document, TransportCharset::Unspecified);
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "{:?} for {:?}",
                started.elapsed(),
                String::from_utf8_lossy(&document[..16])
            );
        }
    }

    /// One tag holding as many distinct attribute names as fit in `size`,
    /// shortest first: 65 one-character names (printable ASCII without
    /// upper case, which the tokenizer folds, and without '/', '>' and '=',
    /// which end a name), then two and three characters.
    fn shortest_distinct_names(size: usize) -> (String, usize) {
        let alphabet: Vec<char> = (0x21u8..=0x7e)
            .map(char::from)
            .filter(|ch| !ch.is_ascii_uppercase() && !matches!(ch, '/' | '>' | '='))
            .collect();
        let name = |mut n: usize| {
            let (mut len, mut block) = (1, alphabet.len());
            while n >= block {
                n -= block;
                len += 1;
                block *= alphabet.len();
            }
            let mut name = vec![alphabet[0]; len];
            for slot in name.iter_mut().rev() {
                *slot = alphabet[n % alphabet.len()];
                n /= alphabet.len();
            }
            name.into_iter().collect::<String>()
        };
        let mut document = String::from("<meta");
        let mut count = 0;
        loop {
            let next = name(count);
            if document.len() + next.len() + 2 > size {
                break;
            }
            document.push(' ');
            document.push_str(&next);
            count += 1;
        }
        document.push('>');
        (document, count)
    }

    /// html5ever compares each new attribute name with the earlier ones in
    /// the tag, so this is quadratic; it is bounded by the document cap and
    /// the content worker's five-second CPU and wall limits. Timed only with
    /// optimised dependencies (the release build the worker ships).
    #[test]
    #[cfg_attr(
        debug_assertions,
        ignore = "the dev profile builds html5ever at opt-level 0"
    )]
    fn one_tag_of_distinct_attribute_names_is_bounded_by_the_cap() {
        let (document, names) = shortest_distinct_names(MAX_DOCUMENT_BYTES);
        assert!(document.len() <= MAX_DOCUMENT_BYTES);
        assert!(names > 66_000, "{names}");
        let started = Instant::now();
        assert_eq!(extract(&document), Err(Unavailable::NoMetadata));
        let elapsed = started.elapsed();
        eprintln!("{names} distinct attribute names in one tag: {elapsed:?}");
        assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
    }

    #[test]
    fn arbitrary_bytes_never_panic_and_stay_bounded() {
        // A deterministic byte soup biased towards markup.
        let alphabet =
            b"<>/!-=\"' \n\tmetapropertycontentog:titleimagescript&#x;0129\xc3\xa9\xff\0";
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
                assert!(found.is_bounded(), "{found:?}");
            }
        }
    }
}
