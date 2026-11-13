// SPDX-License-Identifier: EUPL-1.2
//! Markdown in messages (docs/markdown-rendering-plan.md, docs/desktop.md):
//! CommonMark through pulldown-cmark, with GFM strikethrough and the math
//! grammar of docs/math.md, parsed once per message in the content worker.
//! One deliberate variant: a block quote's `>` needs a space after it
//! ([`bare_quotes`]), so chat emoticons such as `>w<` stay as typed.
//!
//! One shared scan serves formulas and Markdown. Stage one parses without
//! math and masks, byte for byte (line breaks kept), what binds tighter than
//! emphasis, links and formulas: code (spans and blocks), raw inline HTML,
//! autolinks (all but their scheme, so they stay autolinks) and the bare URLs
//! the linkifier links. Stage two parses the masked text with math and
//! strikethrough: its formulas (checked against the text by
//! [`Token::new`]) and its structure. A formula refused there has its
//! delimiters masked and the text is parsed once more, so what it took in
//! is Markdown again (`$a `b` c$` shows the code span).
//!
//! The structure is translated into [`Piece`]s for [`crate::markup::expand`]:
//! the text with the syntax of every rendered construct hidden (delimiters,
//! markers, prefixes, fences, link destinations), the constructs opening and
//! closing, and code and math fences as blocks. Everything else is shown as
//! typed: raw HTML, images, reference links and their definitions, email
//! autolinks, entity references, and links to targets that are not allowed.
//! Line breaks stay as typed; only syntax-only lines (fence lines, setext
//! underlines) are drawn by their block. Legacy marked-up text is translated
//! on its visible text only: its directives stay where they are and are
//! never Markdown, though a construct may span one.

use crate::markup::{Construct, MAX_BLOCKS, MAX_LINKS, Piece, link_target};
use crate::math::{MAX_FORMULA_BYTES, MAX_FORMULAS, Token};
use nuschl_core::{ContentBlock, EMBED};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd};
use std::{borrow::Cow, ops::Range};

/// A message's text split into what is visible and the directives of
/// legacy marked-up text (`EMBED directive EMBED`), by the visible position
/// they stand at. Plain text has none.
pub struct Marked<'a> {
    pub visible: Cow<'a, str>,
    pub markers: Vec<(usize, &'a str)>,
}

impl<'a> Marked<'a> {
    pub fn new(source: &'a str) -> Self {
        if !source.contains(EMBED) {
            return Self {
                visible: source.into(),
                markers: Vec::new(),
            };
        }
        let mut visible = String::with_capacity(source.len());
        let mut markers = Vec::new();
        for (index, part) in source.split(EMBED).enumerate() {
            if index % 2 == 0 {
                visible.push_str(part);
            } else {
                markers.push((visible.len(), part));
            }
        }
        Self {
            visible: visible.into(),
            markers,
        }
    }

    /// Whether a directive stands strictly inside `range` of the visible text.
    fn crosses(&self, range: &Range<usize>) -> bool {
        let first = self.markers.partition_point(|(at, _)| *at <= range.start);
        self.markers
            .get(first)
            .is_some_and(|(at, _)| *at < range.end)
    }
}

/// The shared scan of one text: what stage one masked, and its findings.
pub struct Scan<'a> {
    text: &'a str,
    /// The text as both stages parse it, before masking ([`bare_quotes`]).
    quoted: Cow<'a, str>,
    masked: String,
    /// The bare URLs the linkifier links (`crate::bare_urls`).
    pub urls: Vec<(Range<usize>, String)>,
    /// Raw HTML, inline and block.
    pub html: Vec<Range<usize>>,
}

/// Nuschl's quote variant (docs/desktop.md): a run of `>` that stands where
/// a block quote's marker can (at a line's start, after indentation, list
/// markers and the markers of the quotes it is in) is one only when a space,
/// a tab or the line's end follows the run. So `>w<`, `>:3` and `>>x` stay
/// as typed, while `> q`, `>> q` and a `>` alone quote; CommonMark has no
/// such rule. Returns `text` with each bare run made literal: its bytes
/// become `%`, punctuation like `>` but without meaning in Markdown (inline,
/// `>` only closes a tag or an autolink), so every offset stays. Every line
/// is read so, code and HTML blocks too: a `%` there is still their text,
/// and what is shown is always taken from `text`. A tag or `<!` declaration
/// whose closing `>` starts a line then does not close there; raw HTML is
/// shown as typed either way.
pub fn bare_quotes(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let space = |at: usize| matches!(bytes.get(at), Some(b' ' | b'\t'));
    let line_end = |at: usize| matches!(bytes.get(at), None | Some(b'\n' | b'\r'));
    let mut literal: Option<Vec<u8>> = None;
    let mut line = 0;
    while line < bytes.len() {
        let mut at = line;
        loop {
            while space(at) {
                at += 1;
            }
            match bytes.get(at) {
                Some(b'>') => {
                    let run = bytes[at..].iter().take_while(|byte| **byte == b'>').count();
                    if !space(at + run) && !line_end(at + run) {
                        literal.get_or_insert_with(|| bytes.to_vec())[at..at + run].fill(b'%');
                        break;
                    }
                    at += run;
                }
                Some(b'-' | b'+' | b'*') if space(at + 1) => at += 1,
                Some(b'0'..=b'9') => {
                    let digits = bytes[at..]
                        .iter()
                        .take_while(|byte| byte.is_ascii_digit())
                        .count();
                    if digits > 9
                        || !matches!(bytes.get(at + digits), Some(b'.' | b')'))
                        || !space(at + digits + 1)
                    {
                        break;
                    }
                    at += digits + 1;
                }
                _ => break,
            }
        }
        // A carriage return alone ends a line too, as the parser reads it.
        line = bytes[at..]
            .iter()
            .position(|byte| matches!(byte, b'\n' | b'\r'))
            .map_or(bytes.len(), |end| at + end + 1);
    }
    match literal {
        // Only ASCII bytes were replaced, by ASCII.
        Some(literal) => String::from_utf8(literal).map_or(Cow::Borrowed(text), Cow::Owned),
        None => Cow::Borrowed(text),
    }
}

/// Stage one: `text` parsed without math, with what binds tighter than
/// emphasis and formulas masked.
pub fn scan(text: &str) -> Scan<'_> {
    let quoted = bare_quotes(text);
    let mut masked = quoted.as_bytes().to_vec();
    let mask = |masked: &mut Vec<u8>, range: Range<usize>| {
        for byte in &mut masked[range] {
            if !matches!(*byte, b'\n' | b'\r') {
                *byte = b'x';
            }
        }
    };
    let mut html = Vec::new();
    let mut schemes = Vec::new();
    let mut code_block = false;
    for (event, range) in Parser::new(&quoted).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(_)) => code_block = true,
            Event::End(TagEnd::CodeBlock) => code_block = false,
            Event::Text(_) if code_block => mask(&mut masked, range),
            // Inside the delimiters: backticks stay.
            Event::Code(_) => {
                let written = &text[range.clone()];
                let ticks = written.len() - written.trim_start_matches('`').len();
                if written.len() >= 2 * ticks {
                    mask(&mut masked, range.start + ticks..range.end - ticks);
                }
            }
            // Angle brackets stay. Block HTML holds no inline syntax.
            Event::InlineHtml(_) => {
                if range.len() >= 2 {
                    mask(&mut masked, range.start + 1..range.end - 1);
                }
                html.push(range);
            }
            Event::Html(_) => html.push(range),
            Event::Start(Tag::Link {
                link_type: LinkType::Autolink,
                ..
            }) if range.len() >= 2 => {
                let inner = range.start + 1..range.end - 1;
                let scheme = text[inner.clone()].find(':').map_or(0, |colon| colon + 1);
                mask(&mut masked, inner.start + scheme..inner.end);
                schemes.push(inner.start..inner.start + scheme);
            }
            Event::Start(Tag::Link {
                link_type: LinkType::Email,
                ..
            }) if range.len() >= 2 => mask(&mut masked, range.start + 1..range.end - 1),
            _ => {}
        }
    }
    let urls = crate::bare_urls(text);
    for (range, _) in &urls {
        mask(&mut masked, range.clone());
    }
    // An autolink's URL is a bare URL too: its scheme stays, so it stays an
    // autolink.
    for scheme in schemes {
        masked[scheme.clone()].copy_from_slice(&text.as_bytes()[scheme]);
    }
    // Every byte of a masked character was replaced, so this is UTF-8.
    let masked = String::from_utf8(masked).unwrap_or_else(|_| "x".repeat(text.len()));
    Scan {
        text,
        quoted,
        masked,
        urls,
        html,
    }
}

fn options() -> Options {
    Options::ENABLE_MATH | Options::ENABLE_STRIKETHROUGH
}

impl Scan<'_> {
    /// Stage two: the formula tokens, in order. `refused` refuses more of
    /// them (those that cross a directive of legacy text). A refused token's
    /// delimiters are masked and the text parsed again, once.
    fn formulas(&mut self, refused: impl Fn(&Range<usize>) -> bool) -> Vec<Token> {
        let mut again = Vec::new();
        let tokens = self.tokens(&refused, Some(&mut again));
        if again.is_empty() {
            return tokens;
        }
        let mut bytes = std::mem::take(&mut self.masked).into_bytes();
        for at in again {
            // `$` is punctuation; so is `%`, which means nothing in Markdown.
            bytes[at] = b'%';
        }
        self.masked = String::from_utf8(bytes).unwrap_or_else(|_| "x".repeat(self.text.len()));
        self.tokens(&refused, None)
    }

    fn tokens(
        &self,
        refused: &impl Fn(&Range<usize>) -> bool,
        mut again: Option<&mut Vec<usize>>,
    ) -> Vec<Token> {
        let mut tokens = Vec::new();
        for (event, range) in Parser::new_ext(&self.masked, options()).into_offset_iter() {
            let (tex, display) = match &event {
                Event::InlineMath(tex) => (tex, false),
                Event::DisplayMath(tex) => (tex, true),
                _ => continue,
            };
            // Against the text as parsed: a literal `>` is the formula's own.
            let token = Token::new(&self.quoted, range.clone(), tex, display);
            match token.filter(|_| !refused(&range)) {
                Some(token) => tokens.push(token),
                None => {
                    if let Some(again) = again.as_deref_mut() {
                        let delimiter = if display { 2 } else { 1 };
                        if range.len() >= 2 * delimiter {
                            again.extend(range.start..range.start + delimiter);
                            again.extend(range.end - delimiter..range.end);
                        }
                    }
                }
            }
        }
        tokens
    }
}

/// The formula tokens of a text (`crate::math::tokens`).
pub(crate) fn tokens(text: &str) -> Vec<Token> {
    scan(text).formulas(|_| false)
}

/// What a hidden range of the text is (for the fuzz invariants).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Syntax {
    /// Emphasis, strong emphasis or strikethrough delimiters, code-span backticks.
    Delimiter,
    /// The space CommonMark strips inside a code span.
    CodeSpace,
    /// A backslash escape's or hard break's backslash.
    Escape,
    /// Block quote markers and list item indentation at a line's start.
    Prefix,
    /// A list item's marker, with the spaces around it.
    Marker,
    /// An ATX heading's `#` runs, with their spaces.
    Heading,
    /// A setext heading's underline, with the line break before it.
    Setext,
    /// A thematic break's line.
    Rule,
    /// A link's `[`, or `](destination "title")`.
    Link,
    /// An autolink's angle brackets.
    Autolink,
    /// What a code block shows of its lines no further: fence lines and
    /// indentation (code blocks only).
    Fence,
}

/// A block a construct makes: code (highlighted by the renderer) or a
/// display formula from a `math` fence.
#[derive(Clone)]
pub enum Block {
    Code {
        language: Option<String>,
        body: String,
    },
    Math {
        source: String,
        tex: String,
    },
}

/// A point in the text where a construct opens or closes, or a rule stands.
#[derive(Clone)]
enum Point {
    Open(Construct),
    Close,
    Rule,
    /// An open refused after all (a link whose target is not allowed).
    Void,
}

#[derive(Clone, Copy)]
enum Container {
    Quote,
    /// A list item whose content starts this many columns into it.
    Item(usize),
}

/// What an `End` event closes.
enum Frame {
    Nothing,
    List,
    Image,
    /// A construct that closes at the end of its range (quotes, items).
    Container,
    /// A construct that closes at this position (styles, autolinks).
    CloseAt(usize),
    Heading(Range<usize>),
    /// An inline link: its range and its open point.
    Link(Range<usize>, usize),
}

/// The translation of a text's structure.
#[derive(Clone)]
pub struct Translation {
    /// What is hidden; ranges may overlap (a prefix in a setext underline).
    pub hidden: Vec<(Range<usize>, Syntax)>,
    points: Vec<(usize, Point)>,
    /// Code and math blocks over the ranges they replace, with the ranges
    /// of the text they show.
    pub blocks: Vec<(Range<usize>, Block, Vec<Range<usize>>)>,
    pub tokens: Vec<Token>,
    /// The text stage two parsed.
    pub masked: String,
}

type CodeBlock = (Range<usize>, Option<String>, Vec<Range<usize>>);

struct Translate<'t> {
    text: &'t str,
    /// The text as parsed before masking ([`Scan`]).
    quoted: &'t str,
    marked: &'t Marked<'t>,
    urls: &'t [(Range<usize>, String)],
    hidden: Vec<(Range<usize>, Syntax)>,
    points: Vec<(usize, Point)>,
    blocks: Vec<(Range<usize>, Block, Vec<Range<usize>>)>,
    containers: Vec<(Range<usize>, Container)>,
    frames: Vec<Frame>,
    /// Open lists: the next number of an ordered one.
    lists: Vec<Option<u64>>,
    /// Inside an image: shown as typed, nothing hidden.
    inert: usize,
    /// The end of the last event that shows text: a backslash at or after it
    /// before punctuation is an escape.
    shown_end: usize,
    /// A list item whose marker line's content is not reached yet: its start
    /// and the end of its marker line.
    item: Option<(usize, usize)>,
    /// The first and the end of the last content of the open heading.
    heading: Option<(Option<usize>, usize)>,
    /// The end of the open link's content so far.
    link: usize,
    /// The code block being read: its range, language and content ranges.
    code: Option<CodeBlock>,
    links: usize,
    fences: usize,
    /// Blocks (and the text between them) so far, against [`MAX_BLOCKS`].
    pieces: usize,
    /// Where the text's line breaks and its legacy `link:` directives are.
    newlines: Vec<usize>,
    legacy_links: Vec<usize>,
}

/// The end of the line `at` is in, its line break (and a carriage return
/// before it) excluded, from the text's line breaks (`newlines`): searching
/// the text again for each list item is quadratic in a long line of them.
fn line_end(text: &str, newlines: &[usize], at: usize) -> usize {
    let end = newlines
        .get(newlines.partition_point(|newline| *newline < at))
        .map_or(text.len(), |newline| *newline);
    if end > at && text.as_bytes()[end - 1] == b'\r' {
        end - 1
    } else {
        end
    }
}

impl Translate<'_> {
    fn hide(&mut self, range: Range<usize>, syntax: Syntax) {
        if range.start < range.end {
            self.hidden.push((range, syntax));
        }
    }

    fn point(&mut self, at: usize, point: Point) {
        self.points.push((at, point));
    }

    /// Where the spaces before `at` on its line begin: a block construct's
    /// indentation (up to three spaces) is its syntax, not text of its own.
    fn indentation(&self, at: usize) -> usize {
        let bytes = self.text.as_bytes();
        let mut start = at;
        while start > 0 && matches!(bytes[start - 1], b' ' | b'\t') {
            start -= 1;
        }
        start
    }

    /// Links before `at` that legacy directives or the linkifier may make:
    /// at least what `expand` counts first, so a link accepted here has room
    /// there.
    fn links_before(&self, at: usize) -> usize {
        let urls = self.urls.partition_point(|(range, _)| range.start < at);
        let legacy = self
            .legacy_links
            .partition_point(|position| *position <= at);
        urls + legacy + self.links
    }

    /// The target of a link at `at` to `url` if it is rendered (and then
    /// counted): an allowed target, within the limit. A directive of legacy
    /// text in its syntax stays where it is: stock clients' copy of a Nuschl
    /// message links the URL in `[text](URL)` (`nuschl_legacy_html`), and
    /// that link is the Markdown link's destination.
    fn accept_link(&mut self, url: &str, at: usize) -> Option<String> {
        if self.links >= MAX_LINKS || self.links_before(at) >= MAX_LINKS {
            return None;
        }
        let url = link_target(url)?;
        self.links += 1;
        Some(url)
    }

    fn event(&mut self, event: Event<'_>, range: Range<usize>) {
        let end_of_item = matches!(event, Event::End(TagEnd::Item));
        if let Some((start, end)) = self.item.take() {
            // The content of a list item's marker line starts here, after
            // the marker and the spaces after it.
            let line = &self.text.as_bytes()[start..end];
            let mut marker = line.iter().take_while(|byte| **byte == b' ').count();
            marker += line[marker..]
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            marker += usize::from(marker < line.len());
            marker += line[marker..]
                .iter()
                .take_while(|byte| matches!(byte, b' ' | b'\t'))
                .count();
            let content = if end_of_item {
                start + marker
            } else {
                range.start.clamp(start, start + marker)
            };
            self.hide(self.indentation(start)..content, Syntax::Marker);
        }
        if self.code.is_some() {
            match event {
                Event::Text(_) => {
                    if let Some((_, _, content)) = &mut self.code {
                        content.push(range);
                    }
                }
                Event::End(_) => {
                    self.frames.pop();
                    self.end_code();
                }
                _ => {}
            }
            return;
        }
        if !matches!(event, Event::End(_))
            && let Some((first, last)) = &mut self.heading
        {
            first.get_or_insert(range.start);
            *last = (*last).max(range.start);
        }
        match event {
            Event::Start(tag) => self.start(tag, range),
            Event::End(_) => self.end(range),
            Event::Text(_) => {
                let at = range.start;
                if self.inert == 0
                    && at > self.shown_end
                    && self.text.as_bytes()[at - 1] == b'\\'
                    && self
                        .text
                        .as_bytes()
                        .get(at)
                        .is_some_and(u8::is_ascii_punctuation)
                {
                    self.hide(at - 1..at, Syntax::Escape);
                }
                self.shown(&range);
            }
            Event::Code(_) => {
                if self.inert == 0 {
                    self.code_span(&range);
                }
                self.shown(&range);
            }
            Event::HardBreak => {
                if self.inert == 0 && self.text.as_bytes().get(range.start) == Some(&b'\\') {
                    self.hide(range.start..range.start + 1, Syntax::Escape);
                }
                self.shown(&range);
            }
            Event::Rule => {
                let start = self.indentation(range.start);
                self.hide(
                    start..line_end(self.text, &self.newlines, range.start),
                    Syntax::Rule,
                );
                self.point(range.start, Point::Rule);
                self.shown_end = self.shown_end.max(range.end);
            }
            _ => self.shown(&range),
        }
    }

    fn shown(&mut self, range: &Range<usize>) {
        self.shown_end = self.shown_end.max(range.end);
        self.link = self.link.max(range.end);
        if let Some((_, last)) = &mut self.heading {
            *last = (*last).max(range.end);
        }
    }

    fn code_span(&mut self, range: &Range<usize>) {
        let written = &self.text[range.clone()];
        let ticks = written.len() - written.trim_start_matches('`').len();
        if ticks == 0 || written.len() < 2 * ticks {
            return;
        }
        let inner = range.start + ticks..range.end - ticks;
        self.hide(range.start..inner.start, Syntax::Delimiter);
        self.hide(inner.end..range.end, Syntax::Delimiter);
        // CommonMark strips one space on each side of content that begins
        // and ends with one (a line break counts as a space there, but is kept).
        let content = self.text[inner.clone()].as_bytes();
        let space = |byte: &u8| matches!(byte, b' ' | b'\n');
        let (mut start, mut end) = (inner.start, inner.end);
        if content.first().is_some_and(space)
            && content.last().is_some_and(space)
            && !content.iter().all(space)
        {
            if content[0] == b' ' {
                self.hide(start..start + 1, Syntax::CodeSpace);
                start += 1;
            }
            if content[content.len() - 1] == b' ' {
                self.hide(end - 1..end, Syntax::CodeSpace);
                end -= 1;
            }
        }
        self.point(start, Point::Open(Construct::Code));
        self.point(end.max(start), Point::Close);
    }

    fn start(&mut self, tag: Tag<'_>, range: Range<usize>) {
        let frame = match tag {
            Tag::Heading { level, .. } => {
                self.hide(self.indentation(range.start)..range.start, Syntax::Heading);
                self.heading = Some((None, range.start));
                let level = match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    _ => 3,
                };
                self.point(range.start, Point::Open(Construct::Heading(level)));
                Frame::Heading(range)
            }
            Tag::BlockQuote(_) => {
                let bytes = self.text.as_bytes();
                let mut at = range.start;
                while at < range.end && bytes[at] == b' ' {
                    at += 1;
                }
                if bytes.get(at) == Some(&b'>') {
                    at += 1;
                    if matches!(bytes.get(at), Some(b' ' | b'\t')) {
                        at += 1;
                    }
                    self.hide(self.indentation(range.start)..at, Syntax::Prefix);
                }
                self.containers.push((range.clone(), Container::Quote));
                self.point(range.start, Point::Open(Construct::Quote));
                Frame::Container
            }
            Tag::List(start) => {
                self.lists.push(start.map(|start| start.min(1_000_000)));
                Frame::List
            }
            Tag::Item => {
                let depth = self.lists.len().clamp(1, 4) as u8;
                let marker = match self.lists.last_mut() {
                    Some(Some(number)) => {
                        let marker = format!("{number}.");
                        *number = number.saturating_add(1);
                        marker
                    }
                    _ => ["•", "◦", "▪"][usize::from(depth - 1) % 3].to_owned(),
                };
                let end = line_end(self.text, &self.newlines, range.start).min(range.end);
                self.item = Some((range.start, end));
                let indent = self.indent(range.start, end);
                self.containers
                    .push((range.clone(), Container::Item(indent)));
                self.point(range.start, Point::Open(Construct::Item { depth, marker }));
                Frame::Container
            }
            Tag::CodeBlock(kind) => {
                // The info string as typed (the parsed one is masked).
                let language = match kind {
                    CodeBlockKind::Fenced(_) => {
                        let line = &self.text
                            [range.start..line_end(self.text, &self.newlines, range.start)];
                        line.trim_start()
                            .trim_start_matches(['`', '~'])
                            .split_whitespace()
                            .next()
                            .map(|value| value.chars().take(64).collect())
                    }
                    CodeBlockKind::Indented => None,
                };
                // Its first line's indentation is the block's.
                let start = self.indentation(range.start);
                self.code = Some((start..range.end, language, Vec::new()));
                Frame::Nothing
            }
            Tag::Emphasis | Tag::Strong | Tag::Strikethrough if self.inert == 0 => {
                let (construct, width) = match tag {
                    Tag::Emphasis => (Construct::Emphasis, 1),
                    Tag::Strong => (Construct::Strong, 2),
                    _ => (Construct::Strike, 2),
                };
                let written = &self.text.as_bytes()[range.clone()];
                // `~~text~~` only: a single tilde stays as typed.
                let leading = written.iter().take_while(|byte| **byte == b'~').count();
                let trailing = written
                    .iter()
                    .rev()
                    .take_while(|byte| **byte == b'~')
                    .count();
                let literal = construct == Construct::Strike && (leading != 2 || trailing != 2);
                if literal || range.len() < 2 * width {
                    Frame::Nothing
                } else {
                    self.hide(range.start..range.start + width, Syntax::Delimiter);
                    self.hide(range.end - width..range.end, Syntax::Delimiter);
                    self.point(range.start + width, Point::Open(construct));
                    Frame::CloseAt(range.end - width)
                }
            }
            Tag::Link {
                link_type: LinkType::Inline,
                ..
            } if self.inert == 0 => {
                let open = self.points.len();
                self.link = range.start + 1;
                self.point(range.start + 1, Point::Void);
                Frame::Link(range, open)
            }
            Tag::Link {
                link_type: LinkType::Autolink,
                ..
            } if self.inert == 0 && range.len() >= 2 => {
                let inner = range.start + 1..range.end - 1;
                match self.accept_link(&self.text[inner.clone()], range.start) {
                    Some(url) => {
                        self.hide(range.start..inner.start, Syntax::Autolink);
                        self.hide(inner.end..range.end, Syntax::Autolink);
                        self.point(inner.start, Point::Open(Construct::Link(url)));
                        Frame::CloseAt(inner.end)
                    }
                    None => Frame::Nothing,
                }
            }
            Tag::Image { .. } => {
                self.inert += 1;
                Frame::Image
            }
            // Reference links (and their definitions), email autolinks and
            // everything else stay as typed.
            _ => Frame::Nothing,
        };
        self.frames.push(frame);
    }

    /// Columns from a list item's start to its content on the lines after
    /// its first: its marker and the spaces after it, or one space when the
    /// marker line is blank or what follows is indented code.
    fn indent(&self, start: usize, end: usize) -> usize {
        let line = &self.text.as_bytes()[start..end];
        let mut marker = line.iter().take_while(|byte| **byte == b' ').count();
        marker += match line.get(marker) {
            Some(b'-' | b'+' | b'*') => 1,
            _ => {
                line[marker..]
                    .iter()
                    .take_while(|byte| byte.is_ascii_digit())
                    .count()
                    + 1
            }
        };
        let marker = marker.min(line.len());
        let mut spaces = 0;
        for byte in &line[marker..] {
            match byte {
                b' ' => spaces += 1,
                b'\t' => spaces += 4 - (marker + spaces) % 4,
                _ => break,
            }
        }
        let blank = line[marker..]
            .iter()
            .all(|byte| matches!(byte, b' ' | b'\t' | b'\r'));
        if blank || spaces == 0 || spaces >= 5 {
            marker + 1
        } else {
            marker + spaces
        }
    }

    fn end(&mut self, range: Range<usize>) {
        let Some(frame) = self.frames.pop() else {
            return;
        };
        let heading = matches!(frame, Frame::Heading(_));
        match frame {
            Frame::Nothing => {}
            Frame::List => {
                self.lists.pop();
            }
            Frame::Image => self.inert = self.inert.saturating_sub(1),
            Frame::Container => self.point(range.end, Point::Close),
            Frame::CloseAt(at) => self.point(at, Point::Close),
            Frame::Heading(range) => self.end_heading(range),
            Frame::Link(range, open) => self.end_link(range, open),
        }
        self.link = self.link.max(range.end);
        if !heading && let Some((_, last)) = &mut self.heading {
            *last = (*last).max(range.end);
        }
    }

    fn end_heading(&mut self, range: Range<usize>) {
        let (first, last) = self.heading.take().unwrap_or((None, range.start));
        let written = &self.text[range.clone()];
        let lines = written.strip_suffix('\n').unwrap_or(written);
        let lines = lines.trim_end_matches('\r');
        let end = range.start + lines.len();
        match lines.rfind('\n') {
            // ATX: `#`s and spaces before, spaces and `#`s after (a closing
            // run only after a space), never what the parse found as content.
            None => {
                let opening = lines.len() - lines.trim_start_matches(' ').len();
                let opening = opening + lines[opening..].len()
                    - lines[opening..].trim_start_matches('#').len();
                let opening = opening + lines[opening..].len()
                    - lines[opening..].trim_start_matches([' ', '\t']).len();
                let content = (range.start + opening).min(first.unwrap_or(end));
                let body = lines[..lines.trim_end_matches([' ', '\t']).len()].trim_end_matches('#');
                let closing = if body.len() <= opening || body.ends_with([' ', '\t']) {
                    body.trim_end_matches([' ', '\t']).len()
                } else {
                    lines.trim_end_matches([' ', '\t']).len()
                };
                let closing = (range.start + closing).max(content).max(last.min(end));
                self.hide(range.start..content, Syntax::Heading);
                self.hide(closing..end, Syntax::Heading);
            }
            // Setext: the underline is the heading's, with the line break before it.
            Some(underline) => {
                let underline = range.start + underline;
                self.hide(last.clamp(range.start, underline)..end, Syntax::Setext);
            }
        }
        self.point(range.end, Point::Close);
    }

    /// An inline link: rendered when its destination is allowed, else shown
    /// as typed, brackets and all.
    fn end_link(&mut self, range: Range<usize>, open: usize) {
        let bytes = self.text.as_bytes();
        let from = self.link.clamp(range.start + 1, range.end);
        let bracket = bytes[from..range.end]
            .iter()
            .position(|byte| *byte == b']')
            .map(|at| from + at)
            .filter(|bracket| {
                bytes.get(bracket + 1) == Some(&b'(') && bytes.get(range.end - 1) == Some(&b')')
            });
        let Some(bracket) = bracket.filter(|bracket| bracket + 2 < range.end) else {
            return;
        };
        let inside = self.text[bracket + 2..range.end - 1].trim_start();
        let destination = match inside.strip_prefix('<') {
            Some(rest) => rest.split('>').next().unwrap_or_default(),
            None => inside.split(char::is_whitespace).next().unwrap_or_default(),
        };
        // A destination is read as typed: one with escapes or entity
        // references is not linked.
        if destination.contains(['\\', '&']) {
            return;
        }
        let Some(url) = self.accept_link(destination, range.start) else {
            return;
        };
        self.points[open].1 = Point::Open(Construct::Link(url));
        self.hide(range.start..range.start + 1, Syntax::Link);
        self.hide(bracket..range.end, Syntax::Link);
        self.point(bracket, Point::Close);
    }

    fn end_code(&mut self) {
        let Some((range, language, mut content)) = self.code.take() else {
            return;
        };
        let text = self.text;
        let mut end = range.end;
        // A block that runs to the end of its container or of the message
        // (indented code, an unclosed fence) takes in the line break after
        // its last line: that one is between it and what follows.
        if text[..end].ends_with('\n') {
            end -= 1;
            if let Some(last) = content.last_mut().filter(|last| last.end > end) {
                last.end = end;
                if Range::<usize>::is_empty(last) {
                    content.pop();
                }
            }
        }
        // As many as there may be blocks, counting text between them; past
        // that, the block stays in the text around it.
        let gap = self.blocks.last().map_or(0, |(last, ..)| last.end) < range.start;
        if self.pieces + usize::from(gap) + 1 > MAX_BLOCKS {
            return;
        }
        self.pieces += usize::from(gap) + 1;
        let body: String = content.iter().map(|part| &text[part.clone()]).collect();
        let whole = &text[range.start..end];
        let indent = whole.len() - whole.trim_start_matches([' ', '\t']).len();
        let written = whole[indent..].trim_end_matches(['\n', '\r']);
        let formula = range.start + indent..range.start + indent + written.len();
        let math = language.as_deref() == Some("math")
            && self.fences < MAX_FORMULAS
            && !body.trim().is_empty()
            && body.len() <= MAX_FORMULA_BYTES
            && !self.marked.crosses(&formula);
        if math {
            self.fences += 1;
            let block = Block::Math {
                source: written.to_owned(),
                tex: body,
            };
            self.blocks
                .push((range.start..formula.end, block, vec![formula]));
        } else {
            let block = Block::Code { language, body };
            self.blocks.push((range.start..end, block, content));
        }
    }

    /// Container prefixes on the lines after a container's first: block
    /// quote markers and list item indentation, as far as a line has them
    /// (a lazy continuation line has none), outside code and math blocks.
    /// Read in the text as parsed, where a literal `>` is no marker.
    fn prefixes(&mut self) {
        let text = self.quoted.as_bytes();
        let mut containers = std::mem::take(&mut self.containers).into_iter().peekable();
        let mut open: Vec<(Range<usize>, Container)> = Vec::new();
        let blocks: Vec<Range<usize>> = self
            .blocks
            .iter()
            .map(|(range, ..)| range.clone())
            .collect();
        let mut block = 0;
        for (newline, _) in self.text.match_indices('\n') {
            let line = newline + 1;
            while let Some(container) = containers.next_if(|(range, _)| range.start < line) {
                while open
                    .last()
                    .is_some_and(|(outer, _)| outer.end <= container.0.start)
                {
                    open.pop();
                }
                open.push(container);
            }
            while open.last().is_some_and(|(range, _)| range.end <= line) {
                open.pop();
            }
            while blocks.get(block).is_some_and(|range| range.end <= line) {
                block += 1;
            }
            if open.is_empty() || blocks.get(block).is_some_and(|range| range.start < line) {
                continue;
            }
            let mut at = line;
            for (_, container) in &open {
                if matches!(text.get(at), None | Some(b'\n')) {
                    break;
                }
                match *container {
                    Container::Quote => {
                        let mut quote = at;
                        while quote < at + 3 && text.get(quote) == Some(&b' ') {
                            quote += 1;
                        }
                        if text.get(quote) != Some(&b'>') {
                            break;
                        }
                        quote += 1;
                        if matches!(text.get(quote), Some(b' ' | b'\t')) {
                            quote += 1;
                        }
                        at = quote;
                    }
                    Container::Item(indent) => {
                        let (mut columns, mut item) = (0, at);
                        while columns < indent {
                            match text.get(item) {
                                Some(b' ') => columns += 1,
                                Some(b'\t') => columns += 4 - columns % 4,
                                _ => break,
                            }
                            item += 1;
                        }
                        let blank = matches!(text.get(item), None | Some(b'\n' | b'\r'));
                        if columns < indent && !blank {
                            break;
                        }
                        at = item;
                    }
                }
            }
            if at > line {
                self.hidden.push((line..at, Syntax::Prefix));
            }
        }
    }
}

/// Whether a text is shown as typed: a file offer, whose file name must not
/// change (`nuschl_core::transfer::parse_offer`).
pub fn literal(visible: &str) -> bool {
    nuschl_core::transfer::parse_offer(visible).is_some()
}

/// The structure of a text (the visible text of legacy marked-up text).
pub fn translate(marked: &Marked<'_>) -> Translation {
    let text: &str = &marked.visible;
    let mut scan = scan(text);
    let tokens = scan.formulas(|range| marked.crosses(range));
    let mut walk = Translate {
        text,
        quoted: &scan.quoted,
        marked,
        urls: &scan.urls,
        hidden: Vec::new(),
        points: Vec::new(),
        blocks: Vec::new(),
        containers: Vec::new(),
        frames: Vec::new(),
        lists: Vec::new(),
        inert: 0,
        shown_end: 0,
        item: None,
        heading: None,
        link: 0,
        code: None,
        links: 0,
        fences: 0,
        pieces: 0,
        newlines: text.match_indices('\n').map(|(at, _)| at).collect(),
        legacy_links: marked
            .markers
            .iter()
            .filter(|(_, directive)| directive.starts_with("link:"))
            .map(|(at, _)| *at)
            .collect(),
    };
    for (event, range) in Parser::new_ext(&scan.masked, options()).into_offset_iter() {
        walk.event(event, range);
    }
    walk.prefixes();
    let mut hidden = walk.hidden;
    hidden.sort_by_key(|(range, _)| range.start);
    let (points, blocks) = (walk.points, walk.blocks);
    Translation {
        hidden,
        points,
        blocks,
        tokens,
        masked: scan.masked,
    }
}

/// The pieces [`crate::markup::expand`] takes: `marked`'s visible text with
/// the hidden ranges left out and the formulas in it, its directives where
/// they stand, the constructs where they open and close, and `made` (the
/// blocks of `translation.blocks`, in order) in place of their ranges.
/// Directives inside a code block are left out: code shows text only.
pub(crate) fn pieces(
    marked: &Marked<'_>,
    translation: Translation,
    made: Vec<ContentBlock>,
) -> (Vec<Piece>, Vec<usize>) {
    let text: &str = &marked.visible;
    let mut hidden: Vec<Range<usize>> = Vec::with_capacity(translation.hidden.len());
    for (range, _) in translation.hidden {
        match hidden.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => hidden.push(range),
        }
    }
    let mut points = translation.points;
    points.sort_by_key(|(at, _)| *at);
    let blocks: Vec<Range<usize>> = translation
        .blocks
        .iter()
        .map(|(range, ..)| range.clone())
        .collect();
    let mut made = made.into_iter();
    let tokens = translation.tokens;
    let markers = &marked.markers;
    // Each piece with where it starts in the visible text.
    let mut out: Vec<(usize, Piece)> = Vec::new();
    let mut shown = String::new();
    let mut shown_start = 0;
    let mut found: Vec<Token> = Vec::new();
    let flush = |out: &mut Vec<(usize, Piece)>,
                 shown: &mut String,
                 found: &mut Vec<Token>,
                 start: usize| {
        if !shown.is_empty() {
            out.push((
                start,
                Piece::Text(std::mem::take(shown), std::mem::take(found)),
            ));
        }
    };
    let (mut point, mut marker, mut skip, mut block, mut token) = (0, 0, 0, 0, 0);
    let mut at = 0;
    loop {
        // Constructs and directives here (inside hidden text too), in order:
        // constructs closing, then directives, then constructs opening.
        let next_point = points.get(point).filter(|(position, _)| *position <= at);
        let next_marker = markers.get(marker).filter(|(position, _)| *position <= at);
        let point_first = match (next_point, next_marker) {
            (Some(_), None) => Some(true),
            (None, Some(_)) => Some(false),
            (Some((p, kind)), Some((m, _))) => {
                Some(p < m || (p == m && matches!(kind, Point::Close | Point::Void)))
            }
            (None, None) => None,
        };
        match point_first {
            Some(true) => {
                match &points[point].1 {
                    Point::Void => {}
                    Point::Open(construct) => {
                        flush(&mut out, &mut shown, &mut found, shown_start);
                        out.push((points[point].0, Piece::Open(construct.clone())));
                    }
                    Point::Close => {
                        flush(&mut out, &mut shown, &mut found, shown_start);
                        out.push((points[point].0, Piece::Close));
                    }
                    Point::Rule => {
                        flush(&mut out, &mut shown, &mut found, shown_start);
                        out.push((points[point].0, Piece::Rule));
                    }
                }
                point += 1;
                continue;
            }
            Some(false) => {
                flush(&mut out, &mut shown, &mut found, shown_start);
                out.push((
                    markers[marker].0,
                    Piece::Marker(markers[marker].1.to_owned()),
                ));
                marker += 1;
                continue;
            }
            None => {}
        }
        if let Some(range) = blocks.get(block).filter(|range| range.start <= at) {
            flush(&mut out, &mut shown, &mut found, shown_start);
            if let Some(made) = made.next() {
                out.push((range.start, Piece::Block(made)));
            }
            at = at.max(range.end);
            block += 1;
            while points
                .get(point)
                .is_some_and(|(position, _)| *position < at)
            {
                point += 1;
            }
            while markers
                .get(marker)
                .is_some_and(|(position, _)| *position < at)
            {
                marker += 1;
            }
            continue;
        }
        if at >= text.len() {
            break;
        }
        while hidden.get(skip).is_some_and(|range| range.end <= at) {
            skip += 1;
        }
        let in_hidden = hidden.get(skip).is_some_and(|range| range.start <= at);
        let mut next = text.len();
        for position in [
            points.get(point).map(|(position, _)| *position),
            markers.get(marker).map(|(position, _)| *position),
            blocks.get(block).map(|range| range.start),
            hidden
                .get(skip)
                .map(|range| if in_hidden { range.end } else { range.start }),
        ]
        .into_iter()
        .flatten()
        {
            next = next.min(position);
        }
        if !in_hidden {
            while tokens
                .get(token)
                .is_some_and(|formula| formula.range.start < at)
            {
                token += 1;
            }
            while let Some(formula) = tokens
                .get(token)
                .filter(|formula| formula.range.end <= next)
            {
                let offset = shown.len() + formula.range.start - at;
                found.push(Token {
                    range: offset..offset + formula.range.len(),
                    display: formula.display,
                });
                token += 1;
            }
            if shown.is_empty() {
                shown_start = at;
            }
            shown.push_str(&text[at..next]);
        }
        at = next;
    }
    flush(&mut out, &mut shown, &mut found, shown_start);
    out.into_iter().map(|(at, piece)| (piece, at)).unzip()
}

/// The message from piece `index` on as typed, for when the block budget
/// runs out there ([`crate::markup::expand`]): its visible text with nothing
/// hidden and no formulas, and its directives where they stand (those
/// already given before the piece are not given again).
pub(crate) fn rest(
    marked: &Marked<'_>,
    pieces: &[Piece],
    positions: &[usize],
    index: usize,
) -> Vec<Piece> {
    let text: &str = &marked.visible;
    let Some(&start) = positions.get(index) else {
        return Vec::new();
    };
    let mut given = pieces[..index]
        .iter()
        .zip(&positions[..index])
        .filter(|(piece, at)| matches!(piece, Piece::Marker(_)) && **at == start)
        .count();
    let mut out = Vec::new();
    let mut at = start;
    for (position, directive) in marked.markers.iter().filter(|(at, _)| *at >= start) {
        if *position == start && given > 0 {
            given -= 1;
            continue;
        }
        if *position > at {
            out.push(Piece::Text(text[at..*position].to_owned(), Vec::new()));
            at = *position;
        }
        out.push(Piece::Marker((*directive).to_owned()));
    }
    if at < text.len() {
        out.push(Piece::Text(text[at..].to_owned(), Vec::new()));
    }
    out
}

/// What the content worker made of a message's syntax, for the fuzz
/// invariants (docs/markdown-rendering-plan.md, "Fuzz invariant").
#[doc(hidden)]
pub struct Layout {
    /// The text Markdown is read in: the message without directives.
    pub visible: String,
    /// What is hidden, and as what. Code blocks hide what they do not show
    /// (as [`Syntax::Fence`]).
    pub hidden: Vec<(Range<usize>, Syntax)>,
    /// Code and math blocks: their range and the ranges of what they show.
    pub blocks: Vec<(Range<usize>, Vec<Range<usize>>)>,
    pub tokens: Vec<Token>,
    pub urls: Vec<Range<usize>>,
    pub html: Vec<Range<usize>>,
    /// The text the structure was parsed in (stage two).
    pub masked: String,
}

/// The layout `render` reads `source` with (fuzzing only).
#[doc(hidden)]
pub fn layout(source: &str) -> Layout {
    let marked = Marked::new(source);
    let visible = marked.visible.clone().into_owned();
    let stage_one = scan(&visible);
    let urls = stage_one
        .urls
        .iter()
        .map(|(range, _)| range.clone())
        .collect();
    let html = stage_one.html.clone();
    if literal(&visible) {
        let masked = visible.clone();
        return Layout {
            visible,
            hidden: Vec::new(),
            blocks: Vec::new(),
            tokens: Vec::new(),
            urls,
            html,
            masked,
        };
    }
    let translation = translate(&marked);
    // Where the block budget ran out, as `render` finds it: from there on
    // the message is shown as typed, nothing hidden.
    let made: Vec<ContentBlock> = translation
        .blocks
        .iter()
        .map(|(_, block, _)| match block {
            Block::Code { .. } => ContentBlock::Code {
                language: None,
                runs: Vec::new(),
            },
            Block::Math { source, tex } => ContentBlock::Math {
                source: source.clone(),
                tex: tex.clone(),
            },
        })
        .collect();
    let fences = made
        .iter()
        .filter(|block| matches!(block, ContentBlock::Math { .. }))
        .count();
    let (laid, positions) = pieces(&marked, translation.clone(), made);
    let cut = std::cell::Cell::new(usize::MAX);
    crate::markup::expand(&laid, MAX_FORMULAS.saturating_sub(fences), |index| {
        cut.set(positions.get(index).copied().unwrap_or(usize::MAX));
        rest(&marked, &laid, &positions, index)
    });
    let cut = cut.get();
    let mut hidden: Vec<(Range<usize>, Syntax)> = translation
        .hidden
        .into_iter()
        .filter(|(range, _)| range.start < cut)
        .map(|(range, syntax)| (range.start..range.end.min(cut), syntax))
        .collect();
    let mut blocks = Vec::new();
    for (range, _, shown) in translation
        .blocks
        .into_iter()
        .filter(|(range, ..)| range.start < cut)
    {
        let mut at = range.start;
        for part in &shown {
            if part.start > at {
                hidden.push((at..part.start, Syntax::Fence));
            }
            at = at.max(part.end);
        }
        if range.end > at {
            hidden.push((at..range.end, Syntax::Fence));
        }
        blocks.push((range, shown));
    }
    hidden.sort_by_key(|(range, _)| range.start);
    Layout {
        visible,
        hidden,
        blocks,
        tokens: translation
            .tokens
            .into_iter()
            .filter(|token| token.range.start < cut)
            .collect(),
        urls,
        html,
        masked: translation.masked,
    }
}

/// The fuzz invariants of a message and the document `render` made of it
/// (docs/markdown-rendering-plan.md, "Fuzz invariant"), for the fuzz targets
/// and the property tests; the error says which one failed.
///
/// 1. The document's visible text (text blocks, code, formulas as written)
///    is the text with exactly the hidden ranges removed, compared without
///    line breaks.
/// 2. Every hidden range is the syntax of a construct a separate parse of the
///    same (masked) text reports there, and holds only that syntax.
/// 3. No hidden range touches a formula, what a code block shows, a bare URL
///    (but in a link's destination) or raw HTML (but a container's prefix).
/// 4. Formulas are shown as written, in order.
/// 5. Links have allowed targets, at most `MAX_URL_BYTES` long and at most
///    `MAX_LINKS` of them; plain text makes no image.
/// 6. Text without Markdown syntax characters renders to itself.
/// 7. Lines: the breaks are within their budget, a zero only beside a display
///    formula, no text block begins or ends with a line break, and every line
///    break of the text is in the document (in text, code, a formula, a
///    break, or between two blocks) or on a syntax-only line (hidden).
///
/// Legacy marked-up text (with directives) is held to 1 to 5, and to 7 with
/// the document allowed more line breaks: images and legacy blocks on one
/// line of the text are drawn on lines of their own.
#[doc(hidden)]
pub fn check(source: &str, document: &nuschl_core::MessageDocument) -> Result<(), String> {
    use nuschl_core::MessageDocument;
    let layout = layout(source);
    let text = layout.visible.as_str();
    let legacy = source.contains(EMBED);
    let fail = |what: String| Err::<(), String>(what);
    let mut hidden: Vec<Range<usize>> = Vec::new();
    for (range, syntax) in &layout.hidden {
        if range.end > text.len()
            || !text.is_char_boundary(range.start)
            || !text.is_char_boundary(range.end)
        {
            return fail(format!("hidden {range:?} is not in the text"));
        }
        let written = &text[range.clone()];
        let only = |allowed: &str| written.chars().all(|c| allowed.contains(c));
        let syntax_only = match syntax {
            Syntax::Delimiter => only("*_~`"),
            Syntax::CodeSpace => written == " ",
            Syntax::Escape => written == "\\",
            Syntax::Prefix => only(" \t>"),
            Syntax::Marker => only("0123456789.)-+* \t"),
            Syntax::Heading => only("# \t"),
            Syntax::Setext => only(" \t\r\n>=-") && written.matches('\n').count() == 1,
            Syntax::Rule => only("-*_ \t"),
            Syntax::Link => written == "[" || (written.starts_with("](") && written.ends_with(')')),
            Syntax::Autolink => written == "<" || written == ">",
            Syntax::Fence => true,
        };
        if !syntax_only {
            return fail(format!("hidden {written:?} is not {syntax:?} syntax"));
        }
        // 3.
        let meets = |other: &Range<usize>| range.start < other.end && other.start < range.end;
        if let Some(token) = layout.tokens.iter().find(|token| meets(&token.range)) {
            return fail(format!(
                "hidden {written:?} in the formula at {:?}",
                token.range
            ));
        }
        if layout.blocks.iter().flat_map(|(_, shown)| shown).any(meets) {
            return fail(format!("hidden {written:?} in what a code block shows"));
        }
        if !matches!(syntax, Syntax::Link | Syntax::Fence) && layout.urls.iter().any(meets) {
            return fail(format!("hidden {written:?} in a bare URL"));
        }
        if !matches!(syntax, Syntax::Prefix | Syntax::Fence) && layout.html.iter().any(meets) {
            return fail(format!("hidden {written:?} in raw HTML"));
        }
        match hidden.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => hidden.push(range.clone()),
        }
    }
    // 2: inside a construct of its kind, as a separate parse reports it.
    let mut constructs: Vec<(Range<usize>, &[Syntax])> = Vec::new();
    for (event, range) in Parser::new_ext(&layout.masked, options()).into_offset_iter() {
        let kinds: &[Syntax] = match event {
            Event::Start(Tag::Emphasis | Tag::Strong | Tag::Strikethrough) => &[Syntax::Delimiter],
            Event::Code(_) => &[Syntax::Delimiter, Syntax::CodeSpace],
            Event::Start(Tag::BlockQuote(_)) => &[Syntax::Prefix],
            Event::Start(Tag::Item) => &[Syntax::Prefix, Syntax::Marker],
            Event::Start(Tag::Heading { .. }) => &[Syntax::Heading, Syntax::Setext],
            Event::Rule => &[Syntax::Rule],
            Event::Start(Tag::Link { .. }) => &[Syntax::Link, Syntax::Autolink],
            Event::Start(Tag::CodeBlock(_)) => &[Syntax::Fence],
            _ => continue,
        };
        constructs.push((range, kinds));
    }
    for (range, syntax) in &layout.hidden {
        // A block's first line's indentation is the block's syntax too.
        let indentation = |construct: &Range<usize>| {
            !matches!(
                syntax,
                Syntax::Delimiter | Syntax::CodeSpace | Syntax::Link | Syntax::Autolink
            ) && range.start < construct.start
                && text[range.start..construct.start]
                    .bytes()
                    .all(|byte| matches!(byte, b' ' | b'\t'))
        };
        let inside = |(construct, kinds): &&(Range<usize>, &[Syntax])| {
            kinds.contains(syntax)
                && (construct.start <= range.start || indentation(construct))
                && range.end <= construct.end
        };
        if *syntax == Syntax::Escape {
            // Before punctuation (an escape) or a line break (a hard break),
            // in any block of text (a tight list item's too).
            let next = text.as_bytes().get(range.end);
            if !next
                .is_some_and(|byte| byte.is_ascii_punctuation() || matches!(byte, b'\n' | b'\r'))
            {
                return fail(format!("hidden backslash at {range:?} escapes nothing"));
            }
        } else if !constructs.iter().any(|construct| inside(&construct)) {
            return fail(format!(
                "hidden {range:?} ({syntax:?}) is in no such construct"
            ));
        }
    }
    // 1.
    let images = document
        .blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::Image { .. }));
    if legacy && images {
        return Ok(());
    }
    if images {
        return fail("plain text made an image".into());
    }
    let mut expected = String::with_capacity(text.len());
    let mut at = 0;
    for range in &hidden {
        expected.push_str(&text[at..range.start.max(at)]);
        at = at.max(range.end);
    }
    expected.push_str(&text[at.min(text.len())..]);
    let mut shown = String::with_capacity(text.len());
    for block in &document.blocks {
        match block {
            ContentBlock::Text { text, .. } => shown.push_str(text),
            ContentBlock::Code { runs, .. } => {
                runs.iter().for_each(|run| shown.push_str(&run.text))
            }
            ContentBlock::Math { source, .. } => shown.push_str(source),
            _ => {}
        }
    }
    let lines = |text: &str| text.replace(['\n', '\r'], "");
    if !legacy && lines(&shown) != lines(&expected) {
        return fail(format!("shows {shown:?}, not {expected:?}"));
    }
    // 4 and 5.
    let mut links = 0;
    let mut formulas = Vec::new();
    for block in &document.blocks {
        match block {
            ContentBlock::Text { text, spans, .. } => {
                let mut last_link: Option<(usize, &str)> = None;
                for span in spans {
                    let written = text.get(span.start..span.end).unwrap_or_default();
                    if let Some(tex) = &span.math {
                        if written != format!("${tex}$") {
                            return fail(format!("formula {written:?} for {tex:?}"));
                        }
                        formulas.push(written);
                        if let Some((end, _)) = &mut last_link
                            && *end == span.start
                        {
                            *end = span.end;
                        }
                        continue;
                    }
                    let Some(url) = &span.link else { continue };
                    if !nuschl_core::allowed_link(url) || url.len() > crate::MAX_URL_BYTES {
                        return fail(format!("link to {url:?}"));
                    }
                    let continues =
                        last_link.is_some_and(|(end, last)| end == span.start && last == url);
                    if !continues {
                        links += 1;
                    }
                    last_link = Some((span.end, url));
                }
            }
            ContentBlock::Math { source, tex } if source.starts_with("$$") => {
                if *source != format!("$${tex}$$") {
                    return fail(format!("display formula {source:?} for {tex:?}"));
                }
                formulas.push(source);
            }
            _ => {}
        }
    }
    if links > MAX_LINKS {
        return fail(format!("{links} links"));
    }
    if !legacy {
        let mut tokens = layout.tokens.iter().map(|token| &text[token.range.clone()]);
        for formula in &formulas {
            if !tokens.any(|token| token == *formula) {
                return fail(format!("formula {formula:?} out of order"));
            }
        }
    }
    // 6.
    let syntax = |c: char| "\\`*_~[]()<>!#&$=+-.:|\t".contains(c);
    let indented = text.split('\n').any(|line| line.starts_with("    "));
    if !legacy && !text.contains(syntax) && !indented {
        let blocks: Vec<&ContentBlock> = document
            .blocks
            .iter()
            .filter(|block| !block.is_break())
            .collect();
        let trimmed = text.trim_matches(['\n', '\r']);
        if blocks != [&ContentBlock::text(trimmed)] {
            return fail(format!("plain text {text:?} became {blocks:?}"));
        }
    }
    // 7.
    if !MessageDocument::breaks_valid(document) {
        return fail("breaks out of their budget".into());
    }
    let content: Vec<usize> = (0..document.blocks.len())
        .filter(|index| !document.blocks[*index].is_break())
        .collect();
    let mut newlines = 0;
    for (index, block) in document.blocks.iter().enumerate() {
        match block {
            ContentBlock::Text { text, .. } => {
                if text.starts_with(['\n', '\r']) || text.ends_with(['\n', '\r']) {
                    return fail(format!(
                        "text block {text:?} begins or ends with a line break"
                    ));
                }
                newlines += text.matches('\n').count();
            }
            ContentBlock::Code { runs, .. } => {
                newlines += runs
                    .iter()
                    .map(|run| run.text.matches('\n').count())
                    .sum::<usize>()
            }
            ContentBlock::Math { source, .. } => newlines += source.matches('\n').count(),
            ContentBlock::Image { .. } => {}
            ContentBlock::Break { count } => {
                let math = |at: Option<usize>| {
                    at.and_then(|at| document.blocks.get(at))
                        .is_some_and(|block| matches!(block, ContentBlock::Math { .. }))
                };
                if *count == 0 && !(math(index.checked_sub(1)) || math(index.checked_add(1))) {
                    return fail("a zero break beside no display formula".into());
                }
                newlines += *count as usize;
            }
        }
    }
    for pair in content.windows(2) {
        if pair[1] == pair[0] + 1 {
            newlines += 1;
        }
    }
    let syntax_lines: usize = hidden
        .iter()
        .map(|range| text[range.clone()].matches('\n').count())
        .sum();
    let typed = text.matches('\n').count() - syntax_lines;
    if newlines != typed && !(legacy && newlines > typed) {
        return fail(format!("{newlines} line breaks shown for {typed} typed"));
    }
    // Lines before the first block that shows text (a rule, a code block or
    // an empty item draws its first line itself).
    let first_text = document
        .blocks
        .iter()
        .find(|block| !block.is_break())
        .is_some_and(|block| match block {
            ContentBlock::Text { text, .. } => !text.is_empty(),
            ContentBlock::Math { .. } => true,
            _ => false,
        });
    if !legacy && first_text {
        let leading = expected.len() - expected.trim_start_matches(['\n', '\r']).len();
        let leading = expected[..leading].matches('\n').count();
        let first = match document.blocks.first() {
            Some(ContentBlock::Break { count }) => *count as usize,
            _ => 0,
        };
        if first != leading {
            return fail(format!("{first} line breaks first for {leading} typed"));
        }
    }
    Ok(())
}

/// The grammar as a table: input → blocks, each as `role:text` with its
/// spans written in place as `{flags|text}` (b bold, i italic, u underline,
/// s strike, c code, m formula, `a=URL` link), `code(language):text`,
/// `math:source` or `break N`.
#[cfg(test)]
mod spec {
    use crate::ContentRenderer;
    use nuschl_core::{ContentBlock, EMBED, MessageDocument, TextRole};

    pub(super) fn describe(document: &MessageDocument) -> Vec<String> {
        document
            .blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text, spans, role } => {
                    let role = match role {
                        TextRole::Paragraph => "p".to_owned(),
                        TextRole::Heading(level) => format!("h{level}"),
                        TextRole::Item { depth, marker } => format!("item{depth} {marker}"),
                        TextRole::Quote => "quote".to_owned(),
                        TextRole::Rule => return "rule".to_owned(),
                    };
                    let mut shown = String::new();
                    let mut at = 0;
                    for span in spans {
                        shown.push_str(&text[at..span.start]);
                        let mut flags = String::new();
                        for (on, flag) in [
                            (span.style.bold, 'b'),
                            (span.style.italic, 'i'),
                            (span.style.underline, 'u'),
                            (span.style.strike, 's'),
                            (span.style.code, 'c'),
                            (span.math.is_some(), 'm'),
                        ] {
                            if on {
                                flags.push(flag);
                            }
                        }
                        if let Some(url) = &span.link {
                            flags.push_str(&format!("a={url}"));
                        }
                        shown.push_str(&format!("{{{flags}|{}}}", &text[span.start..span.end]));
                        at = span.end;
                    }
                    shown.push_str(&text[at..]);
                    format!("{role}:{shown}")
                }
                ContentBlock::Code { language, runs } => format!(
                    "code({}):{}",
                    language.as_deref().unwrap_or(""),
                    runs.iter().map(|run| run.text.as_str()).collect::<String>()
                ),
                ContentBlock::Math { source, .. } => format!("math:{source}"),
                ContentBlock::Image { .. } => "image".to_owned(),
                ContentBlock::Break { count } => format!("break {count}"),
            })
            .collect()
    }

    type Case<'a> = (&'a str, &'a [&'a str]);

    fn check(cases: &[Case], html: bool) {
        let renderer = ContentRenderer::default();
        let mut failed = Vec::new();
        for (input, expected) in cases {
            let source = if html {
                renderer.legacy_plain_text(input).unwrap()
            } else {
                (*input).to_owned()
            };
            let document = renderer.render(&source).unwrap();
            let found = describe(&document);
            if found != *expected {
                failed.push(format!(
                    "{input:?}\n  found    {found:?}\n  expected {expected:?}"
                ));
            }
            if let Err(error) = super::check(&source, &document) {
                failed.push(format!("{input:?}: invariant: {error}"));
            }
        }
        assert!(failed.is_empty(), "\n{}", failed.join("\n"));
    }

    #[test]
    fn inline() {
        check(
            &[
                // Emphasis, CommonMark's flanking rules (D11).
                ("**b**", &["p:{b|b}"]),
                ("*i*", &["p:{i|i}"]),
                ("_i_", &["p:{i|i}"]),
                ("***bi***", &["p:{bi|bi}"]),
                (
                    "snake_case and snake_case_name",
                    &["p:snake_case and snake_case_name"],
                ),
                ("__init__", &["p:{b|init}"]),
                ("2 * 3 * 4", &["p:2 * 3 * 4"]),
                ("2*3*4", &["p:2{i|3}4"]),
                ("**unclosed", &["p:**unclosed"]),
                ("a * b", &["p:a * b"]),
                // Strikethrough: two tildes only (D6).
                ("~~s~~", &["p:{s|s}"]),
                ("~s~", &["p:~s~"]),
                // Code spans: backticks and one stripped space hidden.
                ("`**x**`", &["p:{c|**x**}"]),
                ("`` a`b ``", &["p:{c|a`b}"]),
                ("x `a\nb` y", &["p:x {c|a\nb} y"]),
                // Escapes and hard breaks.
                ("\\*x\\*", &["p:*x*"]),
                ("a\\\nb", &["p:a\nb"]),
                ("a  \nb", &["p:a  \nb"]),
                ("C:\\path", &["p:C:\\path"]),
                // Entity references stay as typed (D8).
                ("&amp; &lt;b&gt;", &["p:&amp; &lt;b&gt;"]),
                // Formulas bind tighter than emphasis, code tighter than both.
                ("$*x*$", &["p:{m|$*x*$}"]),
                ("*$x$*", &["p:{im|$x$}"]),
                ("$a_1 * b_2$", &["p:{m|$a_1 * b_2$}"]),
                ("$x `y` z$", &["p:$x {c|y} z$"]),
                ("`$x$`", &["p:{c|$x$}"]),
                ("$$x$$", &["math:$$x$$"]),
            ],
            false,
        );
    }

    #[test]
    fn links_images_and_html() {
        check(
            &[
                ("[t](https://a.example)", &["p:{a=https://a.example|t}"]),
                (
                    "[t *e*](https://a.example)",
                    &["p:{a=https://a.example|t }{ia=https://a.example|e}"],
                ),
                (
                    "[t](https://a.example \"title\")",
                    &["p:{a=https://a.example|t}"],
                ),
                (
                    "[t](<https://a.example/a b>)",
                    &["p:[t](<{a=https://a.example/a|https://a.example/a} b>)"],
                ),
                // Not allowed: the whole construct stays as typed.
                ("[t](javascript:x)", &["p:[t](javascript:x)"]),
                ("[t](mailto:a@b.example)", &["p:[t](mailto:a@b.example)"]),
                (
                    "[t](https://a.example/\\_x)",
                    &["p:[t]({a=https://a.example/\\_x|https://a.example/\\_x})"],
                ),
                (
                    "<https://a.example>",
                    &["p:{a=https://a.example|https://a.example}"],
                ),
                ("<a@b.example>", &["p:<a@b.example>"]),
                (
                    "https://a.example/_x_ y",
                    &["p:{a=https://a.example/_x_|https://a.example/_x_} y"],
                ),
                ("[$x$](https://a.example)", &["p:{m|$x$}"]),
                // Images are never fetched: shown as typed (a bare URL in one
                // is linked, as anywhere).
                (
                    "![i](https://a.example/p.png)",
                    &["p:![i]({a=https://a.example/p.png|https://a.example/p.png})"],
                ),
                ("![*a*](u)", &["p:![*a*](u)"]),
                // Reference links and their definitions stay as typed (D7).
                (
                    "[r]\n\n[r]: https://a.example",
                    &["p:[r]\n\n[r]: {a=https://a.example|https://a.example}"],
                ),
                ("[t][r]", &["p:[t][r]"]),
                // Raw HTML is text.
                ("<b>x</b>", &["p:<b>x</b>"]),
                ("<b>*x*</b>", &["p:<b>{i|x}</b>"]),
                ("<div>\n*x*\n</div>", &["p:<div>\n*x*\n</div>"]),
                (
                    "<span title='*'>a*</span>",
                    &["p:<span title='*'>a*</span>"],
                ),
            ],
            false,
        );
    }

    #[test]
    fn blocks() {
        check(
            &[
                ("# h", &["h1:h"]),
                ("## h ##", &["h2:h"]),
                ("#tag", &["p:#tag"]),
                ("###### h6", &["h3:h6"]),
                ("# *a*", &["h1:{i|a}"]),
                ("foo\n---", &["h2:foo"]),
                ("Title\nmore\n===", &["h1:Title\nmore"]),
                ("---", &["rule"]),
                ("a\n\n***\nb", &["p:a", "break 2", "rule", "p:b"]),
                ("- a\n- b", &["item1 •:a", "item1 •:b"]),
                ("* a\n+ b", &["item1 •:a", "item1 •:b"]),
                (
                    "1. a\n1. b\n1. c",
                    &["item1 1.:a", "item1 2.:b", "item1 3.:c"],
                ),
                ("3) a", &["item1 3.:a"]),
                (
                    "- a\n  - b\n    - c\n      - d\n        - e",
                    &[
                        "item1 •:a",
                        "item2 ◦:b",
                        "item3 ▪:c",
                        "item4 •:d",
                        "item4 •:e",
                    ],
                ),
                ("- a\n\n  more\n- b", &["item1 •:a\n\nmore", "item1 •:b"]),
                (
                    "- a\n  - b\n\n  c",
                    &["item1 •:a", "item2 ◦:b", "break 2", "item1 :c"],
                ),
                ("- - a", &["item2 ◦:a"]),
                ("-\n- b", &["item1 •:", "item1 •:b"]),
                ("> q\n> r", &["quote:q\nr"]),
                ("> q\n>\n> r", &["quote:q\n\nr"]),
                ("> q\nlazy", &["quote:q\nlazy"]),
                ("> - item", &["item1 •:item"]),
                ("> > deep", &["quote:deep"]),
                ("- > q", &["quote:q"]),
                ("    code\n    more", &["code():code\nmore"]),
                ("```rust\nfn x() {}\n```", &["code(rust):fn x() {}\n"]),
                ("~~~\n*x*\n~~~", &["code():*x*\n"]),
                ("> ```\n> x\n> ```", &["code():x\n"]),
                (
                    "- a\n\n  ```\n  x\n  ```",
                    &["item1 •:a", "break 2", "code():x\n"],
                ),
                ("```math\nx^2\n```", &["math:```math\nx^2\n```"]),
                ("a\n   b", &["p:a\n   b"]),
                ("   a", &["p:   a"]),
            ],
            false,
        );
    }

    /// Nuschl's quote variant (issue #190): a `>` run quotes only when a
    /// space, a tab or the line's end follows it, so chat emoticons such as
    /// `>w<` and `>:3` stay as typed. Everything else is CommonMark's.
    #[test]
    fn quotes_need_a_space_after_the_marker() {
        check(
            &[
                // Bare: as typed. Spaced: a quote.
                (">test1", &["p:>test1"]),
                ("> test2", &["quote:test2"]),
                (">test1\n> test2", &["p:>test1", "quote:test2"]),
                (">w<", &["p:>w<"]),
                (">:3", &["p:>:3"]),
                ("hi >w< >:3", &["p:hi >w< >:3"]),
                (">a\n>b", &["p:>a\n>b"]),
                (">---", &["p:>---"]),
                ("># h", &["p:># h"]),
                ("   >x", &["p:   >x"]),
                ("  > q", &["quote:q"]),
                (">\tq", &["quote:q"]),
                // Escaped: as typed, the backslash hidden.
                ("\\>test1", &["p:>test1"]),
                ("\\> test2", &["p:> test2"]),
                // A `>` alone is an empty quote line (shown empty), as in
                // CommonMark: a blank line inside a quote stays in it.
                (">", &["p:"]),
                ("> q\n>\n> r", &["quote:q\n\nr"]),
                // Nested: the whole run decides; each nested quote by its own.
                (">>x", &["p:>>x"]),
                (">> x", &["quote:x"]),
                ("> >x", &["quote:>x"]),
                // A literal line after a quote line continues it lazily.
                ("> q\n>w<", &["quote:q\n>w<"]),
                // In list items.
                ("- >w<", &["item1 •:>w<"]),
                ("1. >:3", &["item1 1.:>:3"]),
                ("> - >x", &["item1 •:>x"]),
                // Code is untouched.
                ("`>x`", &["p:{c|>x}"]),
                ("> `>x`", &["quote:{c|>x}"]),
                ("```\n>w<\n> q\n```", &["code():>w<\n> q\n"]),
                ("> ```\n> >x\n> ```", &["code():>x\n"]),
                ("    >x", &["code():>x"]),
                // A formula keeps a literal `>` that starts its second line.
                ("$a\n>b$", &["p:{m|$a\n>b$}"]),
            ],
            false,
        );
        check(
            &[
                // Explicit HTML quotes stay quotes; stock Mumble's escaped
                // `>` follows the Markdown rule.
                ("<blockquote>said</blockquote>", &["quote:said"]),
                ("&gt;w&lt;", &["p:>w<"]),
                ("&gt; q", &["quote:q"]),
                ("&gt;test1<br/>&gt; test2", &["p:>test1", "quote:test2"]),
            ],
            true,
        );
    }

    /// The line rows: blocks and the breaks that must appear (none means one
    /// line break between blocks).
    #[test]
    fn lines() {
        check(
            &[
                ("# T\ntext", &["h1:T", "p:text"]),
                ("# T\n\ntext", &["h1:T", "break 2", "p:text"]),
                ("# T\n\n\n\ntext", &["h1:T", "break 4", "p:text"]),
                ("text\n\n# T", &["p:text", "break 2", "h1:T"]),
                ("- a\n- b", &["item1 •:a", "item1 •:b"]),
                ("- a\n\n- b", &["item1 •:a", "break 2", "item1 •:b"]),
                ("- a\n\n\n- b", &["item1 •:a", "break 3", "item1 •:b"]),
                ("- a\nb", &["item1 •:a\nb"]),
                ("> q\n\n> r", &["quote:q", "break 2", "quote:r"]),
                ("```\nx\n```\nafter", &["code():x\n", "p:after"]),
                (
                    "```\nx\n```\n\nafter",
                    &["code():x\n", "break 2", "p:after"],
                ),
                ("a\n```\nx\n```", &["p:a", "code():x\n"]),
                (
                    "```\nx\n```\n\n```\ny\n```",
                    &["code():x\n", "break 2", "code():y\n"],
                ),
                (
                    "so $$x$$ and",
                    &["p:so ", "break 0", "math:$$x$$", "break 0", "p: and"],
                ),
                ("$$x$$\n\ntext", &["math:$$x$$", "break 2", "p:text"]),
                ("---\n\ntext", &["rule", "break 2", "p:text"]),
                ("Title\n===\n\ntext", &["h1:Title", "break 2", "p:text"]),
                ("\n\nhello", &["break 2", "p:hello"]),
                ("hello\n\n", &["p:hello", "break 2"]),
                ("a\n\n\nb", &["p:a\n\n\nb"]),
            ],
            false,
        );
        check(
            &[
                ("<p>a</p><h1>T</h1><p>b</p>", &["p:a", "h1:T", "p:b"]),
                // The fence's own line break and the two `<br/>`s: two blank lines.
                ("<pre>x</pre><br/><br/>b", &["code():x\n", "break 3", "p:b"]),
                // The stock copy's break before a leading fence is not the sender's.
                ("<br/>```rust<br/>x<br/>```", &["code(rust):x\n"]),
            ],
            true,
        );
    }

    #[test]
    fn legacy_text_keeps_its_directives() {
        let link = format!(
            "{EMBED}link:https://a.example/*x*{EMBED}see{EMBED}/link{EMBED} *a{EMBED}b{EMBED}b{EMBED}/b{EMBED}*"
        );
        check(
            &[
                (
                    link.as_str(),
                    &["p:{a=https://a.example/*x*|see} {i|a}{bi|b}"],
                ),
                // Typed U+FFFC: what is between two is a directive, never Markdown.
                ("x \u{FFFC}**y**\u{FFFC} *z*", &["p:x  {i|z}"]),
            ],
            false,
        );
        check(
            &[
                // The stock copy of a Nuschl message links the URL in a
                // Markdown link's destination; the link is still rendered.
                (
                    "[t](<a href=\"https://a.example\">https://a.example</a>) &lt;<a href=\"https://b.example\">https://b.example</a>&gt;",
                    &["p:{a=https://a.example|t} {a=https://b.example|https://b.example}"],
                ),
                ("<b>**</b>a**", &["p:{b|a}"]),
                ("<p>1. First</p>", &["item1 1.:First"]),
                ("<b>bold</b> and *stars*", &["p:{b|bold} and {i|stars}"]),
                ("<i>x</i>_y_", &["p:{i|x}_y_"]),
                ("$x<b>y</b>$", &["p:$x{b|y}$"]),
            ],
            true,
        );
    }

    #[test]
    fn file_offers_are_shown_as_typed() {
        let offer = "📎 a_x_ *b*.pdf (2.1 MB) — receive with magic-wormhole: wormhole receive 7-guitarist-revenge";
        let renderer = ContentRenderer::default();
        let document = renderer.render(offer).unwrap();
        assert_eq!(describe(&document), [format!("p:{offer}")]);
        assert!(nuschl_core::transfer::offer_in(&document).is_some());
    }

    /// Hostile Markdown at the size limit renders in linear time: no list
    /// item, link, line or construct looks at the whole text again.
    #[test]
    fn hostile_markdown_takes_linear_time() {
        let renderer = ContentRenderer::default();
        for unit in [
            "> ",
            "- ",
            "1. ",
            "> - ",
            "*",
            "**a",
            "_a",
            "[",
            "[a](",
            "](",
            "`",
            "``a",
            "\\*",
            "$",
            "$$",
            "~~",
            "<",
            "<a@b.example>",
            "<https://b.example>",
            "[t](https://a.example) ",
            "    ",
            "\n- \n",
            ">\n",
            ">w<\n",
            "- a\n  ",
            "#",
            "# a\n",
            "a\n===\n",
            "---\n",
            "```\n",
            "~~~\n",
            "\n",
            "![a](b)",
            "&amp;",
            "\u{FFFC}link:https://a.example\u{FFFC}https://b.example ",
        ] {
            let source = unit.repeat(nuschl_core::MAX_CONTENT_BYTES / unit.len());
            let started = std::time::Instant::now();
            let document = renderer.render(&source).unwrap();
            assert!(
                started.elapsed() < std::time::Duration::from_secs(3),
                "{unit:?}: {:?}",
                started.elapsed()
            );
            assert!(document.breaks_valid(), "{unit:?}");
            let blocks = document
                .blocks
                .iter()
                .filter(|block| !block.is_break())
                .count();
            assert!(
                blocks <= crate::markup::MAX_BLOCKS,
                "{unit:?}: {blocks} blocks"
            );
        }
    }

    /// One budget for every block, whatever makes it (headings, list
    /// items, quotes, rules, code, formulas, images): past it, the rest of
    /// the message is its source as typed, nothing left out or reordered.
    /// The review's message: 512 headings and 256 code blocks in 4,608 bytes
    /// made 768 blocks, which the browser's codec refused and the desktop
    /// drew.
    #[test]
    fn every_block_counts_against_one_budget() {
        let renderer = ContentRenderer::default();
        let review = "# a\n".repeat(512) + &"```\nx\n```\n".repeat(256);
        let mixed = "# h\n\n- i\n  - j\n\n$$x$$\n\n```\nc\n```\n\n> q\n\n---\n\n".repeat(160);
        let html = "<h2>h</h2><pre>x</pre><ul><li>a</li></ul><hr/>".repeat(300);
        let sources = [
            review.clone(),
            mixed,
            "- a\n- b\n".repeat(600),
            "x\n\n# h\n".repeat(600),
            "```math\nx\n```\n".repeat(40) + &"```\nc\n```\n".repeat(600),
            "a $$x$$ b\n".repeat(40) + &"> q\n\nz\n\n".repeat(600),
            renderer.legacy_plain_text(&html).unwrap(),
        ];
        for source in &sources {
            let document = renderer.render(source).unwrap();
            let blocks = document
                .blocks
                .iter()
                .filter(|block| !block.is_break())
                .count();
            let start: String = source.chars().take(40).collect();
            assert!(
                blocks <= crate::markup::MAX_BLOCKS,
                "{start:?}: {blocks} blocks"
            );
            assert!(document.within_limits(), "{start:?}");
            // Nothing is left out or reordered: the text shown is the
            // message's, less exactly the syntax hidden before the budget ran
            // out (`check`, invariant 1), and the last line is there as typed.
            if let Err(error) = super::check(source, &document) {
                panic!("{start:?}: {error}");
            }
        }
        let document = renderer.render(&review).unwrap();
        let Some(ContentBlock::Text { text, .. }) =
            document.blocks.iter().rev().find(|block| !block.is_break())
        else {
            panic!("the last block is the rest of the message, as text");
        };
        assert!(
            text.ends_with("```\nx\n```"),
            "{:?}",
            &text[text.len().saturating_sub(40)..]
        );
    }

    /// The fuzz invariants over generated messages, as the fuzz targets
    /// check them, here without a fuzzer.
    #[test]
    fn invariants_hold_for_generated_messages() {
        const PIECES: &[&str] = &[
            "a",
            "word ",
            " ",
            "\n",
            "\n\n",
            "*",
            "**",
            "_",
            "__",
            "~~",
            "~",
            "`",
            "``",
            "```",
            "```rust\n",
            "~~~",
            "$",
            "$$",
            "\\",
            "[",
            "]",
            "(",
            ")",
            "](https://a.example)",
            "](javascript:x)",
            "<https://b.example>",
            "<a@b.example>",
            "https://c.example/x_y",
            "![",
            "> ",
            ">",
            "- ",
            "* ",
            "+ ",
            "1. ",
            "2) ",
            "# ",
            "## ",
            "#",
            "---",
            "===",
            "    ",
            "\t",
            "  ",
            "<b>",
            "</b>",
            "<div>",
            "&amp;",
            "&#42;",
            "x^2",
            "é",
            "🦀",
            "\u{FFFC}",
            "\u{FFFC}b\u{FFFC}",
            "\u{FFFC}/b\u{FFFC}",
            "\r\n",
            "|",
            "\"t\"",
        ];
        let renderer = ContentRenderer::default();
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut failed = Vec::new();
        for _ in 0..10_000 {
            let mut source = String::new();
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            for _ in 0..(state % 28) {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                source.push_str(PIECES[(state % PIECES.len() as u64) as usize]);
            }
            for text in [source.clone(), renderer.legacy_plain_text(&source).unwrap()] {
                let document = renderer.render(&text).unwrap();
                if let Err(error) = super::check(&text, &document) {
                    failed.push(format!("{text:?}: {error}\n  {:?}", describe(&document)));
                }
            }
            if failed.len() > 20 {
                break;
            }
        }
        assert!(failed.is_empty(), "\n{}", failed.join("\n"));
    }
}
