// SPDX-License-Identifier: EUPL-1.2
//! Formulas are tokens of the one Markdown grammar that also finds fenced
//! code: pulldown-cmark with `Options::ENABLE_MATH` (docs/math.md has the
//! grammar). There is no other pass over the text. A token is kept only if
//! the bytes between its delimiters are exactly its LaTeX, so what the math
//! worker renders is what was typed; the text around it, the wire and stock
//! clients' copy are unchanged.

use nuschel_core::EMBED;
use std::ops::Range;

/// Formulas per message; past them, the delimiters stay text.
pub const MAX_FORMULAS: usize = 32;
/// The longest formula, in bytes between its delimiters
/// (`nuschel_math::latex::MAX_SOURCE_BYTES`).
pub const MAX_FORMULA_BYTES: usize = 4096;

/// A formula token: all of it, delimiters included, and whether it is a
/// display formula (`$$…$$`) or inline (`$…$`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub range: Range<usize>,
    pub display: bool,
}

impl Token {
    /// From the parser's `InlineMath`/`DisplayMath` event over `range` of
    /// `source`, whose content is `tex`. Refused, so the delimiters stay
    /// text: a token whose content differs from the bytes between its
    /// delimiters (the parser drops block quote and list markers from the
    /// continuation lines of a formula), that crosses a marker of legacy
    /// HTML, holds a backtick (LaTeX has none; nothing that could be a code
    /// span becomes math), is empty or too long.
    pub fn new(source: &str, range: Range<usize>, tex: &str, display: bool) -> Option<Self> {
        let delimiter = if display { "$$" } else { "$" };
        let inner = source
            .get(range.clone())?
            .strip_prefix(delimiter)?
            .strip_suffix(delimiter)?;
        let accepted = inner == tex
            && !inner.contains([EMBED, '`'])
            && !inner.trim().is_empty()
            && inner.len() <= MAX_FORMULA_BYTES;
        accepted.then_some(Self { range, display })
    }

    /// The LaTeX between the delimiters of this token in `text`.
    pub fn tex<'a>(&self, text: &'a str) -> &'a str {
        let delimiter = if self.display { 2 } else { 1 };
        &text[self.range.start + delimiter..self.range.end - delimiter]
    }

    /// The token in a part of the text that starts at `start`.
    pub fn shifted(&self, start: usize) -> Self {
        Self {
            range: self.range.start - start..self.range.end - start,
            display: self.display,
        }
    }
}

/// The formula tokens of a text, in order, before the limit on their number.
///
/// CommonMark ranks code spans, autolinks and raw HTML above every other
/// inline construct, but pulldown-cmark 0.13 lets a `$` before a code span
/// pair with a `$` inside it (`$a `$x$` b`). So the text is parsed in that
/// order: first without math, and what is code (spans and blocks), an
/// autolink or HTML there has its content masked, byte for byte, keeping
/// line breaks; then the masked text is parsed with math. No `$` inside those
/// is seen by the second parse, and a token that would take in masked bytes
/// differs from the text and is refused (`Token::new`).
pub fn tokens(source: &str) -> Vec<Token> {
    use pulldown_cmark::{Event, LinkType, Options, Parser, Tag, TagEnd};
    let mut masked = source.as_bytes().to_vec();
    let mut mask = |range: std::ops::Range<usize>| {
        for byte in &mut masked[range] {
            if !matches!(*byte, b'\n' | b'\r') {
                *byte = b'x';
            }
        }
    };
    let mut code_block = false;
    for (event, range) in Parser::new(source).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(_)) => code_block = true,
            Event::End(TagEnd::CodeBlock) => code_block = false,
            Event::Text(_) if code_block => mask(range),
            // Inside the delimiters: backticks and angle brackets stay.
            Event::Code(_) => {
                let text = &source[range.clone()];
                let ticks = text.len() - text.trim_start_matches('`').len();
                if text.len() >= 2 * ticks {
                    mask(range.start + ticks..range.end - ticks);
                }
            }
            Event::InlineHtml(_) | Event::Html(_) if range.len() >= 2 => {
                mask(range.start + 1..range.end - 1)
            }
            Event::Start(Tag::Link {
                link_type: LinkType::Autolink | LinkType::Email,
                ..
            }) if range.len() >= 2 => mask(range.start + 1..range.end - 1),
            _ => {}
        }
    }
    let Ok(masked) = String::from_utf8(masked) else {
        return Vec::new();
    };
    Parser::new_ext(&masked, Options::ENABLE_MATH)
        .into_offset_iter()
        .filter_map(|(event, range)| match event {
            Event::InlineMath(tex) => Token::new(source, range, &tex, false),
            Event::DisplayMath(tex) => Token::new(source, range, &tex, true),
            _ => None,
        })
        .collect()
}

/// The grammar as a table: input → the formulas found in it, as written
/// (delimiters included), with their LaTeX, inline (`i`) or display (`d`).
#[cfg(test)]
mod spec {
    use crate::ContentRenderer;
    use nuschel_core::{ContentBlock, MessageDocument};

    fn found(document: &MessageDocument) -> Vec<(String, String, char)> {
        let mut found = Vec::new();
        for block in &document.blocks {
            match block {
                ContentBlock::Text { text, spans, .. } => {
                    for span in spans {
                        if let Some(tex) = &span.math {
                            found.push((text[span.start..span.end].to_owned(), tex.clone(), 'i'));
                        }
                    }
                }
                ContentBlock::Math { source, tex } => {
                    found.push((source.clone(), tex.clone(), 'd'))
                }
                _ => {}
            }
        }
        found
    }

    /// An input and the formulas expected in it.
    type Case<'a> = (&'a str, &'a [(&'a str, &'a str, char)]);

    fn check(cases: &[Case], html: bool) {
        let renderer = ContentRenderer::default();
        let mut failed = Vec::new();
        for (input, expected) in cases {
            let document = if html {
                renderer.render(&renderer.legacy_plain_text(input).unwrap())
            } else {
                renderer.render(input)
            }
            .unwrap();
            let found = found(&document);
            let expected: Vec<(String, String, char)> = expected
                .iter()
                .map(|(written, tex, kind)| ((*written).into(), (*tex).into(), *kind))
                .collect();
            if found != expected {
                failed.push(format!(
                    "{input:?}\n  found    {found:?}\n  expected {expected:?}"
                ));
            }
        }
        assert!(failed.is_empty(), "\n{}", failed.join("\n"));
    }

    #[test]
    fn plain_messages() {
        check(
            &[
                // Delimiters.
                ("$x$", &[("$x$", "x", 'i')]),
                ("a $x^2$ b", &[("$x^2$", "x^2", 'i')]),
                (
                    "$$\\sum_i x_i$$",
                    &[("$$\\sum_i x_i$$", "\\sum_i x_i", 'd')],
                ),
                ("a $$x$$ b", &[("$$x$$", "x", 'd')]),
                ("$$\nx\n$$", &[("$$\nx\n$$", "\nx\n", 'd')]),
                ("```math\nx^2\n```", &[("```math\nx^2\n```", "x^2\n", 'd')]),
                // Prices and other dollars.
                ("$5 and $10", &[]),
                ("It costs $5 and $10.", &[]),
                ("$5, $10, $15", &[]),
                ("between 5$ and 10$", &[]),
                ("a lone $ sign", &[]),
                ("costs $5 and x$", &[("$5 and x$", "5 and x", 'i')]),
                ("costs \\$5 and x$", &[]),
                ("$x$5", &[("$x$", "x", 'i')]),
                // Spaces inside the delimiters.
                ("$ x$", &[]),
                ("$x $", &[]),
                ("$ x $", &[]),
                // Escapes: a dollar, and inside a formula, kept as typed.
                ("\\$x\\$", &[]),
                ("\\$5 and $x$", &[("$x$", "x", 'i')]),
                ("$a \\$ b$", &[("$a \\$ b$", "a \\$ b", 'i')]),
                (
                    "$\\{x\\} \\_ \\\\$",
                    &[("$\\{x\\} \\_ \\\\$", "\\{x\\} \\_ \\\\", 'i')],
                ),
                // Emphasis never reaches into a formula, nor breaks it.
                (
                    "$a_1 * b_2 * c$",
                    &[("$a_1 * b_2 * c$", "a_1 * b_2 * c", 'i')],
                ),
                ("*$x$*", &[("$x$", "x", 'i')]),
                (
                    "_$x_1$_ and $y_2$",
                    &[("$x_1$", "x_1", 'i'), ("$y_2$", "y_2", 'i')],
                ),
                ("**$a**b$**", &[("$a**b$", "a**b", 'i')]),
                // Code binds tighter: spans, fences, indented code.
                ("`$x$`", &[]),
                ("``$$x$$``", &[]),
                (
                    "$x$ and `$y$` and $z$",
                    &[("$x$", "x", 'i'), ("$z$", "z", 'i')],
                ),
                ("$a `b$ c`", &[]),
                ("```\n$x$\n```", &[]),
                ("~~~\n$$x$$\n~~~", &[]),
                ("    $x$", &[]),
                ("$`x`$", &[]),
                ("$a `$x$` b", &[]),
                ("$$y `$x$` b", &[]),
                ("$a `$x$` b $c$", &[("$c$", "c", 'i')]),
                ("$a <span title=\"$\">x</span> $b$", &[("$b$", "b", 'i')]),
                ("$a <https://x.example/$> b$", &[]),
                // Braces: a $ inside a group belongs to the formula.
                ("${a}$", &[("${a}$", "{a}", 'i')]),
                (
                    "$\\text{costs $5}$",
                    &[("$\\text{costs $5}$", "\\text{costs $5}", 'i')],
                ),
                ("$\\frac{1}{2}$", &[("$\\frac{1}{2}$", "\\frac{1}{2}", 'i')]),
                ("$}$", &[]),
                // Line breaks: inline within a paragraph, never across one.
                ("$a\nb$", &[("$a\nb$", "a\nb", 'i')]),
                ("$a\n\nb$", &[]),
                ("$$a\n\nb$$", &[]),
                ("a $x$\n$y$", &[("$x$", "x", 'i'), ("$y$", "y", 'i')]),
                ("ends with $", &[]),
                // Unclosed and empty.
                ("$x", &[]),
                ("$$x", &[]),
                ("$$", &[]),
                ("$$$$", &[]),
                ("$a$b$", &[("$a$", "a", 'i')]),
                // Lists and quotes: a formula within a line counts; one whose
                // continuation lines carry markers does not (its bytes are not
                // its LaTeX).
                ("- $x$\n- $y$", &[("$x$", "x", 'i'), ("$y$", "y", 'i')]),
                ("> $x$ and $$y$$", &[("$x$", "x", 'i'), ("$$y$$", "y", 'd')]),
                ("- $a\n  b$", &[]),
                ("> $$\n> x\n> $$", &[]),
                // Links, autolinks and raw HTML.
                ("[$x$](https://a.example)", &[("$x$", "x", 'i')]),
                ("[a](https://a.example/$x$)", &[]),
                ("<https://a.example/$x$>", &[]),
                ("<span>$x$</span>", &[("$x$", "x", 'i')]),
                ("<div>\n$x$\n</div>", &[]),
            ],
            false,
        );
    }

    /// Stock clients' HTML: the same grammar over the text the HTML becomes,
    /// where an element's boundary ends a formula and code and links hold none.
    #[test]
    fn legacy_html() {
        check(
            &[
                ("<p>$x$</p>", &[("$x$", "x", 'i')]),
                ("$a &lt; b$", &[("$a < b$", "a < b", 'i')]),
                ("<b>$x$</b> $y$", &[("$x$", "x", 'i'), ("$y$", "y", 'i')]),
                ("$x<b>y</b>$", &[]),
                ("<code>$x$</code>", &[]),
                ("<tt>$$x$$</tt>", &[]),
                ("<a href=\"https://a.example\">$x$</a>", &[]),
                ("<pre>$x$</pre>", &[]),
                ("$$a<br/>b$$", &[("$$a\nb$$", "a\nb", 'd')]),
                ("It costs $5 and $10.", &[]),
                (
                    "<img src=\"data:image/png;base64,$x$\" /> $y$",
                    &[("$y$", "y", 'i')],
                ),
            ],
            true,
        );
    }

    /// Nothing changes the text: a document's text, formulas as written,
    /// is the message's.
    #[test]
    fn formulas_keep_the_text_as_typed() {
        let renderer = ContentRenderer::default();
        for input in [
            "Area $\\pi r^2$ and $$\\int_0^1 x\\,dx$$ done, $5 and $10.",
            "$a_1 * b_2$ `$code$` \\$ $$x$$$$y$$",
        ] {
            let document = renderer.render(input).unwrap();
            let text: String = document
                .blocks
                .iter()
                .map(|block| match block {
                    ContentBlock::Text { text, .. } => text.as_str(),
                    ContentBlock::Math { source, .. } => source.as_str(),
                    _ => "",
                })
                .collect();
            assert_eq!(text, input);
        }
    }
}
