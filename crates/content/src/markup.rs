// SPDX-License-Identifier: EUPL-1.2
//! Legacy Mumble rich text (Qt's HTML subset) to marked-up text, and that
//! text to document blocks.
//!
//! The HTML walk keeps what stock Mumble shows, minus anything that would
//! load a resource: text with its line and paragraph breaks, inline styles,
//! links, headings, lists, quotes, rules, preformatted code, table rows and
//! inline `data:` images. It writes them as markers, `EMBED directive EMBED`,
//! into the text the history stores, so a stored message renders the same
//! again and its visible text (every other `EMBED` part) stays searchable.
//! [`expand`] turns markers into blocks and spans. Plain messages pass
//! through it too, so it trusts nothing in them.
//!
//! Directives: `b` `i` `u` `s` `code` `color:rrggbb` `link:URL` open a style
//! that `/b` … `/link` close; `h1`–`h3` and `item:DEPTH:MARKER` make the line
//! they start a heading or list item; `quote` … `/quote` quote what is
//! between; `hr` is a rule; `data:image/…` is an image.

use crate::{
    MAX_DOCUMENT_PIXELS_BYTES, MAX_IMAGE_HEIGHT, MAX_IMAGE_WIDTH, MAX_IMAGES, data_uri,
    decode_image,
};
use html5ever::{
    Attribute,
    tendril::{StrTendril, TendrilSink},
    tokenizer::{BufferQueue, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer},
};
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use nuschel_core::{ContentBlock, EMBED, MAX_CONTENT_BYTES, Span, Style, TextRole};
use std::{borrow::Cow, cell::RefCell};

/// Links per message, and the longest link target kept.
pub const MAX_LINKS: usize = 128;
pub const MAX_URL_BYTES: usize = 2048;
/// Styled parts and blocks per message; past them text stays plain and in
/// the block it is in.
const MAX_SPANS: usize = 2048;
const MAX_BLOCKS: usize = 512;
/// Styles open at once, list nesting shown, and the longest style attribute read.
const MAX_OPEN: usize = 16;
const MAX_LIST_DEPTH: usize = 4;
const MAX_STYLE_BYTES: usize = 512;
/// Nested deeper than this, HTML is read as plain text: building its tree
/// takes time that grows with the square of the depth.
const MAX_HTML_DEPTH: usize = 1024;

/// Elements whose content is never shown.
const HIDDEN: &[&str] = &[
    "script", "style", "head", "title", "iframe", "object", "embed", "svg", "math", "template",
    "noscript",
];
/// Elements where whitespace with a line break is formatting of the markup.
const NO_TEXT: &[&str] = &[
    "html", "body", "ul", "ol", "table", "thead", "tbody", "tfoot", "tr",
];

fn marker(text: &mut String, directive: &str) {
    text.push(EMBED);
    text.push_str(directive);
    text.push(EMBED);
}

/// Text with every marker removed.
pub fn without_markers(text: &str) -> Cow<'_, str> {
    if text.contains(EMBED) {
        text.split(EMBED).step_by(2).collect::<String>().into()
    } else {
        text.into()
    }
}

/// A link target if it may be one: http, https or mumble, without control,
/// invisible or whitespace characters, and not too long.
pub fn link_target(raw: &str) -> Option<String> {
    if raw.len() > 4 * MAX_URL_BYTES {
        return None;
    }
    let invisible = |c: char| {
        c.is_control()
            || c == EMBED
            || matches!(
                c,
                '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}'
            )
    };
    let url: String = raw.chars().filter(|&c| !invisible(c)).collect();
    let url = url.trim();
    (url.len() <= MAX_URL_BYTES
        && !url.contains(char::is_whitespace)
        && nuschel_core::allowed_link(url))
    .then(|| url.to_owned())
}

/// `#rgb`, `#rrggbb`, `rgb(r, g, b)` or a basic colour name.
fn parse_color(value: &str) -> Option<[u8; 3]> {
    let value = value.trim().to_ascii_lowercase();
    if let Some(hex) = value.strip_prefix('#') {
        let digits = hex
            .chars()
            .map(|c| c.to_digit(16).map(|digit| digit as u8))
            .collect::<Option<Vec<u8>>>()?;
        return match digits[..] {
            [r, g, b] => Some([r * 17, g * 17, b * 17]),
            [r1, r2, g1, g2, b1, b2] => Some([r1 * 16 + r2, g1 * 16 + g2, b1 * 16 + b2]),
            _ => None,
        };
    }
    if let Some(channels) = value
        .strip_prefix("rgb(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        let channels = channels
            .split(',')
            .map(|channel| channel.trim().parse::<u8>().ok())
            .collect::<Option<Vec<u8>>>()?;
        return <[u8; 3]>::try_from(channels).ok();
    }
    Some(match value.as_str() {
        "black" => [0, 0, 0],
        "white" => [255, 255, 255],
        "gray" | "grey" => [128, 128, 128],
        "silver" => [192, 192, 192],
        "red" => [255, 0, 0],
        "maroon" => [128, 0, 0],
        "orange" => [255, 165, 0],
        "yellow" => [255, 255, 0],
        "olive" => [128, 128, 0],
        "lime" => [0, 255, 0],
        "green" => [0, 128, 0],
        "aqua" | "cyan" => [0, 255, 255],
        "teal" => [0, 128, 128],
        "blue" => [0, 0, 255],
        "navy" => [0, 0, 128],
        "fuchsia" | "magenta" => [255, 0, 255],
        "purple" => [128, 0, 128],
        "pink" => [255, 192, 203],
        _ => return None,
    })
}

/// The inline styles an element sets: by its tag, a `<font color>`, and the
/// few CSS properties Qt writes (weight, style, decoration, colour, family).
fn inline_styles(tag: &str, attr: impl Fn(&str) -> Option<String>) -> Vec<String> {
    let mut styles = Vec::new();
    let mut add = |style: &str| styles.push(style.to_owned());
    match tag {
        "b" | "strong" => add("b"),
        "i" | "em" | "cite" | "dfn" | "var" => add("i"),
        "u" | "ins" => add("u"),
        "s" | "strike" | "del" => add("s"),
        "code" | "tt" | "kbd" | "samp" => add("code"),
        _ => {}
    }
    if tag == "font"
        && let Some([r, g, b]) = attr("color").and_then(|color| parse_color(&color))
    {
        add(&format!("color:{r:02x}{g:02x}{b:02x}"));
    }
    let css = attr("style").filter(|css| css.len() <= MAX_STYLE_BYTES);
    for declaration in css.iter().flat_map(|css| css.split(';')) {
        let Some((property, value)) = declaration.split_once(':') else {
            continue;
        };
        let value = value.trim().to_ascii_lowercase();
        match property.trim().to_ascii_lowercase().as_str() {
            "font-weight"
                if value == "bold"
                    || value == "bolder"
                    || value.parse::<u32>().is_ok_and(|weight| weight >= 600) =>
            {
                add("b")
            }
            "font-style" if value == "italic" || value == "oblique" => add("i"),
            "text-decoration" | "text-decoration-line" => {
                if value.contains("underline") {
                    add("u");
                }
                if value.contains("line-through") {
                    add("s");
                }
            }
            "color" => {
                if let Some([r, g, b]) = parse_color(&value) {
                    add(&format!("color:{r:02x}{g:02x}{b:02x}"));
                }
            }
            "font-family" if value.contains("mono") || value.contains("courier") => add("code"),
            _ => {}
        }
    }
    styles
}

/// The longest run of backticks in an element's text, so a fence around it
/// can be longer.
fn longest_backticks(node: &Handle) -> usize {
    let (mut longest, mut run) = (0, 0);
    let mut nodes = vec![node.clone()];
    while let Some(node) = nodes.pop() {
        if let NodeData::Text { contents } = &node.data {
            for c in contents.borrow().chars() {
                run = if c == '`' { run + 1 } else { 0 };
                longest = longest.max(run);
            }
        }
        nodes.extend(node.children.borrow().iter().rev().cloned());
    }
    longest
}

/// What to write when an element ends.
#[derive(Default)]
struct Close {
    /// Directives to close, in the order they opened.
    markers: Vec<String>,
    fence: Option<String>,
    block: bool,
    list: bool,
    row: bool,
    pre: bool,
}

enum Step {
    Visit(Handle),
    Close(Close),
}

/// The HTML walk's output and the context it is in.
struct Walk {
    text: String,
    /// Whether the visible text ends a line (markers do not count).
    line_start: bool,
    links: usize,
    /// Open lists: whether numbered, and the last number given.
    lists: Vec<(bool, u32)>,
    /// Cells so far in each open table row.
    rows: Vec<usize>,
    /// Open `<pre>` elements: only the outermost becomes a code fence.
    pre: usize,
}

impl Walk {
    fn marker(&mut self, directive: &str) {
        marker(&mut self.text, directive);
    }

    fn visible(&mut self, text: impl IntoIterator<Item = char>) {
        let before = self.text.len();
        self.text.extend(text.into_iter().filter(|c| *c != EMBED));
        if self.text.len() > before {
            self.line_start = self.text.ends_with('\n');
        }
    }

    fn block_break(&mut self) {
        if !self.line_start {
            self.visible(['\n']);
        }
    }

    /// Write what an element starts with; returns what it ends with.
    fn open(&mut self, tag: &str, attrs: &[Attribute], node: &Handle) -> Close {
        let attr = |name: &str| {
            attrs
                .iter()
                .find(|attribute| &*attribute.name.local == name)
                .map(|attribute| attribute.value.to_string())
        };
        let mut close = Close::default();
        match tag {
            "br" => self.visible(['\n']),
            "img" => match attr("src").filter(|source| data_uri(source).is_some()) {
                // Only inline data: images are kept; remote URLs are never fetched.
                Some(source) => {
                    let source: String = source.chars().filter(|c| !c.is_whitespace()).collect();
                    self.marker(&source.replace(EMBED, ""));
                    self.line_start = false;
                }
                None => self.visible("[image]".chars()),
            },
            "hr" => {
                self.block_break();
                self.marker("hr");
                self.visible(['\n']);
            }
            "p" | "div" | "center" | "address" | "dl" | "dt" | "dd" | "table" | "thead"
            | "tbody" | "tfoot" | "caption" | "figure" | "figcaption" => {
                self.block_break();
                close.block = true;
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.block_break();
                self.marker(match tag {
                    "h1" => "h1",
                    "h2" => "h2",
                    _ => "h3",
                });
                close.block = true;
            }
            "ul" | "ol" => {
                self.block_break();
                let ordered = tag == "ol";
                let start = attr("start")
                    .and_then(|start| start.trim().parse::<u32>().ok())
                    .filter(|_| ordered)
                    .unwrap_or(1)
                    .min(1_000_000);
                self.lists.push((ordered, start.saturating_sub(1)));
                close.list = true;
                close.block = true;
            }
            "li" => {
                self.block_break();
                let depth = self.lists.len().clamp(1, MAX_LIST_DEPTH);
                let marker = match self.lists.last_mut() {
                    Some((true, number)) => {
                        *number = number.saturating_add(1);
                        format!("{number}.")
                    }
                    _ => ["•", "◦", "▪"][(depth - 1) % 3].to_owned(),
                };
                self.marker(&format!("item:{depth}:{marker}"));
                close.block = true;
            }
            "blockquote" => {
                self.block_break();
                self.marker("quote");
                close.markers.push("/quote".into());
                close.block = true;
            }
            "pre" => {
                self.block_break();
                if self.pre == 0 {
                    let fence = "`".repeat(longest_backticks(node).max(2) + 1);
                    self.visible(fence.chars().chain(['\n']));
                    close.fence = Some(fence);
                }
                self.pre += 1;
                close.pre = true;
                close.block = true;
            }
            "tr" => {
                self.block_break();
                self.rows.push(0);
                close.row = true;
                close.block = true;
            }
            "td" | "th" => {
                if let Some(cells) = self.rows.last_mut() {
                    *cells += 1;
                    if *cells > 1 {
                        self.visible(" │ ".chars());
                    }
                }
                if tag == "th" {
                    self.marker("b");
                    close.markers.push("/b".into());
                }
            }
            "a" if self.links < MAX_LINKS => {
                if let Some(url) = attr("href").and_then(|href| link_target(&href)) {
                    self.links += 1;
                    self.marker(&format!("link:{url}"));
                    close.markers.push("/link".into());
                }
            }
            _ => {}
        }
        for style in inline_styles(tag, attr) {
            let name = style.split(':').next().unwrap_or_default();
            close.markers.push(format!("/{name}"));
            self.marker(&style);
        }
        close
    }

    fn close(&mut self, close: Close) {
        for directive in close.markers.iter().rev() {
            self.marker(directive);
        }
        if let Some(fence) = close.fence {
            self.block_break();
            self.visible(fence.chars().chain(['\n']));
        }
        self.pre -= usize::from(close.pre);
        if close.list {
            self.lists.pop();
        }
        if close.row {
            self.rows.pop();
        }
        if close.block {
            self.block_break();
        }
    }
}

/// Roughly how deep elements nest, and never much less than the parser's
/// tree: start tags, void ones aside, less the end tags that close the
/// innermost one. An end tag that closes nothing (`<b></i>`) leaves the
/// parser's elements open, and it looks through all of them for a match:
/// counting it as a close let repeated ones build a tree deep enough to take
/// longer than the content worker's five seconds.
fn nesting(source: &str) -> usize {
    const VOID: &[&[u8]] = &[
        b"area", b"base", b"br", b"col", b"embed", b"hr", b"img", b"input", b"link", b"meta",
        b"param", b"source", b"track", b"wbr",
    ];
    // Longer names never match an end tag (and are read no further).
    const MAX_NAME: usize = 64;
    let mut open: Vec<&[u8]> = Vec::new();
    let mut deepest = 0;
    for (at, _) in source.match_indices('<') {
        let rest = &source.as_bytes()[at + 1..];
        let (end, rest) = match rest.split_first() {
            Some((b'/', rest)) => (true, rest),
            _ => (false, rest),
        };
        let rest = &rest[..rest.len().min(MAX_NAME + 1)];
        let name = rest
            .iter()
            .position(|byte| byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>'))
            .map_or(rest, |length| &rest[..length]);
        if !name.first().is_some_and(u8::is_ascii_alphabetic) {
            continue;
        }
        if end {
            if name.len() <= MAX_NAME
                && open
                    .last()
                    .is_some_and(|last| last.eq_ignore_ascii_case(name))
            {
                open.pop();
            }
        } else if !VOID.iter().any(|void| void.eq_ignore_ascii_case(name)) {
            open.push(name);
            deepest = deepest.max(open.len());
        }
    }
    deepest
}

/// The text of HTML too deep to build a tree for, from its tokens alone:
/// characters, with line breaks for breaks and blocks.
fn token_text(source: &str) -> String {
    #[derive(Default)]
    struct Sink {
        text: RefCell<String>,
        hidden: RefCell<Option<String>>,
    }
    impl TokenSink for Sink {
        type Handle = ();

        fn process_token(&self, token: Token, _: u64) -> TokenSinkResult<()> {
            let mut text = self.text.borrow_mut();
            let mut hidden = self.hidden.borrow_mut();
            match token {
                Token::CharacterTokens(characters) if hidden.is_none() => {
                    text.extend(characters.chars().filter(|c| *c != EMBED));
                }
                Token::TagToken(tag) => {
                    let name = tag.name.as_ref();
                    match (tag.kind, hidden.as_deref()) {
                        (TagKind::StartTag, None) if HIDDEN.contains(&name) => {
                            *hidden = Some(name.to_owned())
                        }
                        (TagKind::EndTag, Some(open)) if open == name => *hidden = None,
                        (_, None) if name == "br" => text.push('\n'),
                        (_, None)
                            if NEWLINE_TAGS.contains(&name)
                                && !(text.is_empty() || text.ends_with('\n')) =>
                        {
                            text.push('\n')
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            TokenSinkResult::Continue
        }
    }
    const NEWLINE_TAGS: &[&str] = &[
        "p",
        "div",
        "li",
        "tr",
        "pre",
        "blockquote",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "hr",
    ];
    let tokenizer = Tokenizer::new(Sink::default(), Default::default());
    let input = BufferQueue::default();
    input.push_back(StrTendril::from(source));
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    let text = tokenizer.sink.text.take();
    text.trim_end_matches('\n').to_owned()
}

/// Legacy HTML to marked-up text; see the module documentation.
pub fn legacy_text(source: &str) -> Result<String, crate::Error> {
    if source.len() > MAX_CONTENT_BYTES {
        return Err(crate::Error::TooLarge);
    }
    if nesting(source) > MAX_HTML_DEPTH {
        let text = token_text(source);
        return if text.len() > MAX_CONTENT_BYTES {
            Err(crate::Error::TooLarge)
        } else {
            Ok(text)
        };
    }
    let dom = html5ever::parse_document(RcDom::default(), Default::default()).one(source);
    let mut walk = Walk {
        text: String::new(),
        line_start: true,
        links: 0,
        lists: Vec::new(),
        rows: Vec::new(),
        pre: 0,
    };
    let mut steps = vec![Step::Visit(dom.document.clone())];
    while let Some(step) = steps.pop() {
        let node = match step {
            Step::Visit(node) => node,
            Step::Close(close) => {
                walk.close(close);
                continue;
            }
        };
        let mut formatting = false;
        match &node.data {
            NodeData::Text { contents } => walk.visible(contents.borrow().chars()),
            NodeData::Element { name, attrs, .. } => {
                let tag = name.local.as_ref();
                if HIDDEN.contains(&tag) {
                    continue;
                }
                formatting = NO_TEXT.contains(&tag);
                let close = walk.open(tag, &attrs.borrow(), &node);
                steps.push(Step::Close(close));
            }
            _ => {}
        }
        // Markers may take the text past the limit; past four times it, stop.
        if walk.text.len() > 4 * MAX_CONTENT_BYTES {
            return Err(crate::Error::TooLarge);
        }
        for child in node.children.borrow().iter().rev() {
            let layout = |text: &str| text.contains('\n') && text.trim_ascii().is_empty();
            if formatting
                && matches!(&child.data, NodeData::Text { contents } if layout(&contents.borrow()))
            {
                continue;
            }
            steps.push(Step::Visit(child.clone()));
        }
    }
    // The last block's line break ends nothing.
    let mut text = walk.text;
    text.truncate(text.trim_end_matches('\n').len());
    // Too much markup for the limit: the text alone, if that fits.
    if text.len() > MAX_CONTENT_BYTES {
        text = without_markers(&text).into_owned();
    }
    if text.len() > MAX_CONTENT_BYTES {
        return Err(crate::Error::TooLarge);
    }
    Ok(text)
}

/// Bare http, https and mumble URLs in `text` outside existing links.
fn bare_links(text: &str, spans: &[Span], count: &mut usize) -> Vec<(usize, usize, String)> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut from = 0;
    for (colon, _) in text.match_indices("://") {
        if *count >= MAX_LINKS {
            break;
        }
        if colon < from {
            continue;
        }
        let Some(start) = ["https", "http", "mumble"].iter().find_map(|scheme| {
            let start = colon.checked_sub(scheme.len())?;
            bytes[start..colon]
                .eq_ignore_ascii_case(scheme.as_bytes())
                .then_some(start)
        }) else {
            continue;
        };
        // The scheme starts a word: "xhttp://" is no link.
        if text[..start]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric)
        {
            continue;
        }
        let mut end = text[colon..]
            .find(|c: char| {
                c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | '"' | '`')
            })
            .map_or(text.len(), |length| colon + length);
        from = end;
        // Sentence punctuation after a URL is not part of it, nor is a closing
        // bracket that has no opening one inside it. Brackets are counted once:
        // counting again for every character trimmed is quadratic in the URL.
        let url = &text[start..end];
        let unclosed =
            |open, close| url.matches(open).count() as isize - url.matches(close).count() as isize;
        let (mut parens, mut brackets) = (unclosed('(', ')'), unclosed('[', ']'));
        while let Some(last) = text[start..end].chars().next_back() {
            match last {
                ')' if parens < 0 => parens += 1,
                ']' if brackets < 0 => brackets += 1,
                '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' => {}
                _ => break,
            }
            end -= last.len_utf8();
        }
        let linked = |span: &Span| span.link.is_some() && span.start < end && start < span.end;
        if spans.iter().any(linked) {
            continue;
        }
        if let Some(url) = link_target(&text[start..end]) {
            *count += 1;
            found.push((start, end, url));
        }
    }
    found
}

/// `spans` with `links` (sorted, disjoint, apart from linked spans) laid
/// over them: spans are split where a link begins or ends.
fn with_links(spans: Vec<Span>, links: Vec<(usize, usize, String)>) -> Vec<Span> {
    if links.is_empty() {
        return spans;
    }
    let mut cuts: Vec<usize> = spans
        .iter()
        .flat_map(|span| [span.start, span.end])
        .chain(links.iter().flat_map(|(start, end, _)| [*start, *end]))
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    let (mut span, mut link) = (0, 0);
    let mut out = Vec::with_capacity(spans.len() + 2 * links.len());
    for pair in cuts.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        while spans
            .get(span)
            .is_some_and(|candidate| candidate.end <= start)
        {
            span += 1;
        }
        while links
            .get(link)
            .is_some_and(|candidate| candidate.1 <= start)
        {
            link += 1;
        }
        let styled = spans.get(span).filter(|candidate| candidate.start <= start);
        let url = links
            .get(link)
            .filter(|candidate| candidate.0 <= start)
            .map(|candidate| candidate.2.clone());
        let mut piece = Span {
            start,
            end,
            style: styled.map(|span| span.style).unwrap_or_default(),
            link: styled.and_then(|span| span.link.clone()),
        };
        if piece.link.is_none() {
            piece.link = url;
        }
        if !piece.style.is_plain() || piece.link.is_some() {
            out.push(piece);
        }
    }
    out
}

enum Open {
    Bold,
    Italic,
    Underline,
    Strike,
    Code,
    Color(Option<[u8; 3]>),
    Link(Option<String>),
}

/// Blocks being assembled from marked-up text.
struct Expand {
    out: Vec<ContentBlock>,
    text: String,
    spans: Vec<Span>,
    /// Where the current run of one style began.
    run: usize,
    open: Vec<Open>,
    /// The current block's role: a heading or list item for one line, or a quote.
    role: TextRole,
    quotes: usize,
    styled: usize,
    links: usize,
    images: usize,
    budget: usize,
}

impl Expand {
    fn base(&self) -> TextRole {
        if self.quotes > 0 {
            TextRole::Quote
        } else {
            TextRole::Paragraph
        }
    }

    fn style(&self) -> (Style, Option<String>) {
        let mut style = Style::default();
        let mut link = None;
        for open in &self.open {
            match open {
                Open::Bold => style.bold = true,
                Open::Italic => style.italic = true,
                Open::Underline => style.underline = true,
                Open::Strike => style.strike = true,
                Open::Code => style.code = true,
                Open::Color(color) => style.color = color.or(style.color),
                Open::Link(url) => link = url.clone().or(link),
            }
        }
        (style, link)
    }

    /// End the current run of one style.
    fn cut(&mut self) {
        let end = self.text.len();
        if self.run < end && self.styled < MAX_SPANS {
            let (style, link) = self.style();
            if !style.is_plain() || link.is_some() {
                self.styled += 1;
                self.spans.push(Span {
                    start: self.run,
                    end,
                    style,
                    link,
                });
            }
        }
        self.run = end;
    }

    fn open(&mut self, open: Open) {
        self.cut();
        if self.open.len() < MAX_OPEN {
            self.open.push(open);
        }
    }

    fn close(&mut self, matches: impl Fn(&Open) -> bool) {
        self.cut();
        if let Some(index) = self.open.iter().rposition(matches) {
            self.open.remove(index);
        }
    }

    fn end_block(&mut self) {
        self.cut();
        let base = self.base();
        let role = std::mem::replace(&mut self.role, base);
        self.run = 0;
        // Line breaks between blocks are spacing the layout gives already.
        let spacing = self.text.trim_matches(['\n', '\r']).is_empty()
            && matches!(role, TextRole::Paragraph | TextRole::Quote);
        if spacing && self.spans.is_empty() {
            self.text.clear();
            return;
        }
        let text = std::mem::take(&mut self.text);
        let spans = std::mem::take(&mut self.spans);
        let links = bare_links(&text, &spans, &mut self.links);
        let spans = with_links(spans, links);
        self.out.push(ContentBlock::Text { text, spans, role });
    }

    /// Start a block of its own, unless there are too many already.
    fn block(&mut self, role: TextRole) -> bool {
        if self.out.len() >= MAX_BLOCKS {
            return false;
        }
        self.end_block();
        self.role = role;
        true
    }

    fn visible(&mut self, mut part: &str) {
        // A heading or list item is one line.
        while matches!(self.role, TextRole::Heading(_) | TextRole::Item { .. })
            && let Some(newline) = part.find('\n')
        {
            self.text.push_str(&part[..newline]);
            self.end_block();
            part = &part[newline + 1..];
        }
        self.text.push_str(part);
    }

    fn image(&mut self, uri: &str) {
        let decoded = (self.images < MAX_IMAGES)
            .then(|| data_uri(uri))
            .flatten()
            .and_then(|bytes| decode_image(&bytes, MAX_IMAGE_WIDTH, MAX_IMAGE_HEIGHT))
            .filter(|image| match image {
                ContentBlock::Image { width, height, .. } => {
                    (*width * *height * 4) as usize <= self.budget
                }
                _ => false,
            });
        let Some(mut image) = decoded else {
            self.visible("[image]");
            return;
        };
        if let ContentBlock::Image {
            width,
            height,
            source,
            ..
        } = &mut image
        {
            self.budget -= (*width * *height * 4) as usize;
            *source = Some(uri.chars().filter(|c| !c.is_whitespace()).collect());
        }
        self.images += 1;
        self.end_block();
        self.out.push(image);
    }

    fn markup(&mut self, text: &str) {
        for (index, part) in text.split(EMBED).enumerate() {
            if index % 2 == 0 {
                self.visible(part);
                continue;
            }
            let (name, argument) = part.split_once(':').unwrap_or((part, ""));
            match name {
                "b" => self.open(Open::Bold),
                "i" => self.open(Open::Italic),
                "u" => self.open(Open::Underline),
                "s" => self.open(Open::Strike),
                "code" => self.open(Open::Code),
                "color" => {
                    let hex = |at: usize| u8::from_str_radix(argument.get(at..at + 2)?, 16).ok();
                    let color = (argument.len() == 6)
                        .then(|| Some([hex(0)?, hex(2)?, hex(4)?]))
                        .flatten();
                    self.open(Open::Color(color));
                }
                "link" => {
                    let url = (self.links < MAX_LINKS)
                        .then(|| link_target(argument))
                        .flatten();
                    self.links += usize::from(url.is_some());
                    self.open(Open::Link(url));
                }
                "/b" => self.close(|open| matches!(open, Open::Bold)),
                "/i" => self.close(|open| matches!(open, Open::Italic)),
                "/u" => self.close(|open| matches!(open, Open::Underline)),
                "/s" => self.close(|open| matches!(open, Open::Strike)),
                "/code" => self.close(|open| matches!(open, Open::Code)),
                "/color" => self.close(|open| matches!(open, Open::Color(_))),
                "/link" => self.close(|open| matches!(open, Open::Link(_))),
                "h1" | "h2" | "h3" => {
                    self.block(TextRole::Heading(name.as_bytes()[1] - b'0'));
                }
                "item" => {
                    let (depth, marker) = argument.split_once(':').unwrap_or(("1", "•"));
                    let depth = depth
                        .parse::<u8>()
                        .unwrap_or(1)
                        .clamp(1, MAX_LIST_DEPTH as u8);
                    let marker = marker.chars().take(8).collect();
                    self.block(TextRole::Item { depth, marker });
                }
                "quote" if self.out.len() < MAX_BLOCKS => {
                    self.end_block();
                    self.quotes += 1;
                    self.role = self.base();
                }
                "/quote" if self.out.len() < MAX_BLOCKS => {
                    self.end_block();
                    self.quotes = self.quotes.saturating_sub(1);
                    self.role = self.base();
                }
                "hr" => {
                    if self.block(TextRole::Rule) {
                        self.end_block();
                    }
                }
                "data" => self.image(part),
                _ => {}
            }
        }
    }
}

/// The blocks `render` found, with markers expanded: text blocks get spans
/// and roles (and links for bare URLs), images and rules become blocks of
/// their own, code passes through. Unknown markers are dropped.
pub fn expand(blocks: Vec<ContentBlock>) -> Vec<ContentBlock> {
    let mut expand = Expand {
        out: Vec::with_capacity(blocks.len()),
        text: String::new(),
        spans: Vec::new(),
        run: 0,
        open: Vec::new(),
        role: TextRole::Paragraph,
        quotes: 0,
        styled: 0,
        links: 0,
        images: 0,
        budget: MAX_DOCUMENT_PIXELS_BYTES,
    };
    for block in blocks {
        match block {
            ContentBlock::Text { text, .. } => expand.markup(&text),
            other => {
                expand.end_block();
                expand.out.push(other);
            }
        }
    }
    expand.end_block();
    if expand.out.is_empty() {
        expand.out.push(ContentBlock::text(""));
    }
    expand.out
}

#[cfg(test)]
mod tests {
    use super::{MAX_LINKS, MAX_URL_BYTES};
    use crate::{ContentRenderer, Error};
    use nuschel_core::{ContentBlock, EMBED, MAX_CONTENT_BYTES, MessageDocument, Style, TextRole};

    /// (visible text, [(linked text, url)]) of each text block; neighbouring
    /// spans of one link (differently styled) count as one.
    fn links(document: &MessageDocument) -> Vec<(String, Vec<(String, String)>)> {
        document
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, spans, .. } => {
                    let mut found: Vec<(usize, usize, String)> = Vec::new();
                    for span in spans {
                        let Some(url) = &span.link else { continue };
                        match found.last_mut() {
                            Some((_, end, last)) if *end == span.start && last == url => {
                                *end = span.end
                            }
                            _ => found.push((span.start, span.end, url.clone())),
                        }
                    }
                    let found = found
                        .into_iter()
                        .map(|(start, end, url)| (text[start..end].to_owned(), url))
                        .collect();
                    Some((text.clone(), found))
                }
                _ => None,
            })
            .collect()
    }

    fn html(renderer: &ContentRenderer, source: &str) -> Result<MessageDocument, Error> {
        renderer.render(&renderer.legacy_plain_text(source)?)
    }

    #[test]
    fn anchors_and_bare_urls_become_links_with_allowed_schemes_only() -> Result<(), Error> {
        let renderer = ContentRenderer::new()?;
        let document = html(
            &renderer,
            "see <a href=\"https://example.com/a?b=1&amp;c\">the <b>docs</b></a>, \
             <a href='javascript:alert(1)'>js</a> <a href='file:///etc/passwd'>file</a> \
             <a href='data:text/html,x'>data</a> <a href=' MUMBLE://host/ch '>join</a>",
        )?;
        assert_eq!(
            links(&document),
            [(
                "see the docs, js file data join".to_owned(),
                vec![
                    (
                        "the docs".to_owned(),
                        "https://example.com/a?b=1&c".to_owned()
                    ),
                    ("join".to_owned(), "MUMBLE://host/ch".to_owned()),
                ]
            )]
        );
        // Bare URLs in plain messages (and in old history) too, without
        // trailing punctuation or an unmatched closing bracket.
        let document = renderer.render(
            "Read https://en.wikipedia.org/wiki/Rust_(language). (or http://a.example/x) \
             xhttp://no.example ftp://no.example mumble://ok.example:64738/",
        )?;
        let urls: Vec<String> = links(&document)[0]
            .1
            .iter()
            .map(|(_, url)| url.clone())
            .collect();
        assert_eq!(
            urls,
            [
                "https://en.wikipedia.org/wiki/Rust_(language)",
                "http://a.example/x",
                "mumble://ok.example:64738/"
            ]
        );
        Ok(())
    }

    #[test]
    fn hostile_links_are_capped_and_cleaned() -> Result<(), Error> {
        let renderer = ContentRenderer::new()?;
        // Control and bidi characters are removed from targets; whitespace
        // inside one, or a target past the length cap, makes no link.
        let huge = format!("https://example.com/{}", "a".repeat(MAX_URL_BYTES));
        let document = html(
            &renderer,
            &format!(
                "<a href=\"https://exa\u{7}mple.com/\u{202E}x\">one</a> \
                 <a href=\"https://a b.example\">two</a> <a href=\"{huge}\">three</a>"
            ),
        )?;
        assert_eq!(
            links(&document)[0].1,
            [("one".to_owned(), "https://example.com/x".to_owned())]
        );
        // Many links: only the first MAX_LINKS are links, all text is kept.
        let many: String = (0..MAX_LINKS + 50)
            .map(|n| format!("<a href=\"https://e.example/{n}\">{n}</a> "))
            .collect();
        let document = html(&renderer, &many)?;
        let (text, found) = &links(&document)[0];
        assert_eq!(found.len(), MAX_LINKS);
        assert!(text.contains(&format!("{}", MAX_LINKS + 49)));
        // Nested and unclosed anchors and markers never yield overlapping or
        // out-of-range spans.
        for source in [
            "<a href='https://a.example'>a<a href='https://b.example'>b</a>c",
            "<a href='https://a.example'><p>para<a href='https://b.example'>x",
            "<b><a href='https://a.example'>unclosed",
            "text\u{FFFC}link:https://a.example\u{FFFC}more\u{FFFC}/link\u{FFFC}\u{FFFC}/link\u{FFFC}",
            "\u{FFFC}link:https://a.example\u{FFFC}",
        ] {
            for document in [html(&renderer, source)?, renderer.render(source)?] {
                for block in &document.blocks {
                    if let ContentBlock::Text { text, spans, .. } = block {
                        let mut end = 0;
                        for span in spans {
                            assert!(end <= span.start && span.start < span.end, "{source}");
                            assert!(
                                text.is_char_boundary(span.start)
                                    && text.is_char_boundary(span.end)
                            );
                            end = span.end;
                        }
                        assert!(end <= text.len() && !text.contains(EMBED), "{source}");
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn closing_brackets_after_a_url_take_linear_time() -> Result<(), Error> {
        // Brackets were counted again for every one trimmed: this took longer
        // than the content worker's five seconds, so the message never showed.
        let renderer = ContentRenderer::new()?;
        for close in [")", "]"] {
            let source = format!("see https://a.example/x{}", close.repeat(200_000));
            let started = std::time::Instant::now();
            let document = renderer.render(&source)?;
            assert!(
                started.elapsed() < std::time::Duration::from_secs(2),
                "{:?}",
                started.elapsed()
            );
            assert_eq!(
                links(&document)[0].1,
                [(
                    "https://a.example/x".to_owned(),
                    "https://a.example/x".to_owned()
                )]
            );
        }
        Ok(())
    }

    #[test]
    fn linked_urls_inside_a_fence_stay_plain_code() -> Result<(), Error> {
        let renderer = ContentRenderer::new()?;
        let document = html(
            &renderer,
            "```<br/>curl <a href=\"https://x.example/\">https://x.example/</a><br/>```",
        )?;
        let Some(ContentBlock::Code { runs, .. }) = document.blocks.first() else {
            panic!("{document:?}");
        };
        assert_eq!(
            runs.iter().map(|run| run.text.as_str()).collect::<String>(),
            "curl https://x.example/\n"
        );
        Ok(())
    }

    /// (text, role, [(styled text, style)]) of each text block.
    type Styled = (String, TextRole, Vec<(String, Style)>);

    fn styled(document: &MessageDocument) -> Vec<Styled> {
        document
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, spans, role } => Some((
                    text.clone(),
                    role.clone(),
                    spans
                        .iter()
                        .filter(|span| !span.style.is_plain())
                        .map(|span| (text[span.start..span.end].to_owned(), span.style))
                        .collect(),
                )),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn qt_and_mumble_formatting_becomes_styles() -> Result<(), Error> {
        let renderer = ContentRenderer::new()?;
        // As Qt's rich text editor writes it, and as Mumble's markdown does.
        let document = html(
            &renderer,
            "<p style=\"margin:0px;\"><span style=\" font-weight:600; color:#ff0000;\">bold red</span> \
             <i>it</i> <u>un</u> <s>st</s> <code>co</code> <font color=\"lime\">gr</font>\
             <span style=\"font-family:'Courier New'\">mo</span></p>",
        )?;
        let bold_red = Style {
            bold: true,
            color: Some([255, 0, 0]),
            ..Style::default()
        };
        let only = |change: fn(&mut Style)| {
            let mut style = Style::default();
            change(&mut style);
            style
        };
        assert_eq!(
            styled(&document),
            [(
                "bold red it un st co grmo".to_owned(),
                TextRole::Paragraph,
                vec![
                    ("bold red".to_owned(), bold_red),
                    ("it".to_owned(), only(|style| style.italic = true)),
                    ("un".to_owned(), only(|style| style.underline = true)),
                    ("st".to_owned(), only(|style| style.strike = true)),
                    ("co".to_owned(), only(|style| style.code = true)),
                    (
                        "gr".to_owned(),
                        only(|style| style.color = Some([0, 255, 0]))
                    ),
                    ("mo".to_owned(), only(|style| style.code = true)),
                ]
            )]
        );
        Ok(())
    }

    #[test]
    fn headings_lists_quotes_rules_and_tables_become_blocks() -> Result<(), Error> {
        let renderer = ContentRenderer::new()?;
        let document = html(
            &renderer,
            "<h1>Title</h1>\n<h5>Small</h5><ul>\n<li>one</li>\n<li>two<ul><li>deep</li></ul></li></ul>\
             <ol start=\"3\"><li>third</li><li>fourth</li></ol><blockquote>said<br/>twice</blockquote>\
             <hr/><table>\n<tr><th>A</th><td>B</td></tr>\n<tr><td>1</td><td>2</td></tr></table>after",
        )?;
        let roles: Vec<(String, TextRole)> = styled(&document)
            .into_iter()
            .map(|(text, role, _)| (text.trim_matches('\n').to_owned(), role))
            .collect();
        let item = |depth, marker: &str| TextRole::Item {
            depth,
            marker: marker.into(),
        };
        assert_eq!(
            roles,
            [
                ("Title".to_owned(), TextRole::Heading(1)),
                ("Small".to_owned(), TextRole::Heading(3)),
                ("one".to_owned(), item(1, "•")),
                ("two".to_owned(), item(1, "•")),
                ("deep".to_owned(), item(2, "◦")),
                ("third".to_owned(), item(1, "3.")),
                ("fourth".to_owned(), item(1, "4.")),
                ("said\ntwice".to_owned(), TextRole::Quote),
                (String::new(), TextRole::Rule),
                ("A │ B\n1 │ 2\nafter".to_owned(), TextRole::Paragraph),
            ]
        );
        Ok(())
    }

    #[test]
    fn preformatted_text_is_a_code_block_even_with_backticks_in_it() -> Result<(), Error> {
        let renderer = ContentRenderer::new()?;
        // Mumble's markdown sends ``` code as <pre>, with no language.
        let document = html(
            &renderer,
            "look:<pre>let a = 1;\n```\n<b>b</b>();</pre>done",
        )?;
        let kinds: Vec<String> = document
            .blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text, .. } => text.trim_matches('\n').to_owned(),
                ContentBlock::Code { language, runs } => format!(
                    "{}: {}",
                    language.as_deref().unwrap_or("code"),
                    runs.iter().map(|run| run.text.as_str()).collect::<String>()
                ),
                ContentBlock::Image { .. } => "image".into(),
            })
            .collect();
        assert_eq!(kinds, ["look:", "code: let a = 1;\n```\nb();\n", "done"]);
        Ok(())
    }

    #[test]
    fn unknown_and_resource_loading_markup_degrades_to_text() -> Result<(), Error> {
        let renderer = ContentRenderer::new()?;
        let document = html(
            &renderer,
            "<marquee>hi</marquee> <custom-tag>there</custom-tag><link rel=stylesheet href='https://x.example/a.css'>\
             <style>@import url(https://x.example/b.css)</style><iframe src='https://x.example'>frame</iframe>\
             <object data='https://x.example'>obj</object><svg><text>svg</text></svg>\
             <span style=\"background:url(https://x.example/c.png); font-size:90px\">big</span>\
             <img src='https://x.example/d.png'><noscript><img src='https://x.example/e.png'></noscript>",
        )?;
        assert_eq!(
            styled(&document),
            [("hi therebig[image]".to_owned(), TextRole::Paragraph, vec![])]
        );
        Ok(())
    }

    #[test]
    fn hostile_nesting_and_volume_stay_within_limits() -> Result<(), Error> {
        let renderer = ContentRenderer::new()?;
        for (open, close) in [
            ("<b>", "</b>"),
            ("<ul><li>", "</li></ul>"),
            ("<blockquote>", "</blockquote>"),
            ("<span style='color:red'>", "</span>"),
            ("<pre>", "</pre>"),
            ("<table><tr><td>", "</td></tr></table>"),
        ] {
            let source = format!("{}x{}", open.repeat(5_000), close.repeat(5_000));
            let document = html(&renderer, &source)?;
            assert!(
                document.blocks.len() <= 600,
                "{open}: {}",
                document.blocks.len()
            );
        }
        let many: String = (0..8_000)
            .map(|n| format!("<i>{n}</i><h2>h</h2>"))
            .collect();
        let document = html(&renderer, &many)?;
        let spans: usize = document
            .blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text { spans, .. } => spans.len(),
                _ => 0,
            })
            .sum();
        assert!(spans <= 2048 && document.blocks.len() <= 513, "{spans}");
        // Every visible character is still there, in one text or another.
        let text: String = document
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.contains("7999"));
        // Deeper than the tree is built for: the text alone, without its markup.
        let deep = format!(
            "{}deep <b>text</b>{}",
            "<div>".repeat(2_000),
            "</div>".repeat(2_000)
        );
        assert_eq!(
            styled(&html(&renderer, &deep)?),
            [("deep text".to_owned(), TextRole::Paragraph, vec![])]
        );
        let deep = format!("<script>x()</script>{}a<br>b", "<p>".repeat(2_000));
        assert_eq!(renderer.legacy_plain_text(&deep)?, "a\nb");
        // End tags that close nothing leave everything open: the parser took
        // longer than the worker's five seconds for the first at 256 KiB.
        for unit in ["<b></i>", "<em></x>", "<x-a></x-b>", "<div></span>"] {
            let source = format!("{}x", unit.repeat(MAX_CONTENT_BYTES / unit.len() - 1));
            let started = std::time::Instant::now();
            assert_eq!(renderer.legacy_plain_text(&source)?, "x");
            assert!(
                started.elapsed() < std::time::Duration::from_secs(2),
                "{unit}: {:?}",
                started.elapsed()
            );
        }
        // A huge style attribute is ignored rather than parsed.
        let css = format!(
            "<span style=\"{}font-weight:bold\">x</span>",
            "a:b;".repeat(1_000)
        );
        assert_eq!(styled(&html(&renderer, &css)?)[0].2, []);
        Ok(())
    }

    #[test]
    fn stored_markup_renders_the_same_and_reads_as_plain_text() -> Result<(), Error> {
        let renderer = ContentRenderer::new()?;
        let stored = renderer.legacy_plain_text(
            "<b>Hi</b> <a href='https://a.example'>you</a><ul><li>x</li></ul>",
        )?;
        // History stores the text and renders it again without the HTML.
        assert_eq!(
            renderer.render(&stored)?,
            html(
                &renderer,
                "<b>Hi</b> <a href='https://a.example'>you</a><ul><li>x</li></ul>"
            )?
        );
        // Search and previews read every other part: the visible text.
        let visible: String = stored.split(EMBED).step_by(2).collect();
        assert_eq!(visible, "Hi you\nx");
        Ok(())
    }
}
