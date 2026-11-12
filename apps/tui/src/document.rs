// SPDX-License-Identifier: EUPL-1.2
//! Messages as terminal text. The content worker already turned untrusted
//! input into a `MessageDocument`; this lays it out as lines of styled
//! spans. Images and display formulas become placeholders with what they
//! say in words (the formula's source; an image's size, since the worker
//! keeps no alternative text), and links are numbered so they can be listed
//! and opened. Every string from a server goes through [`clean`] first: no
//! control characters or direction overrides reach the terminal.
use nuschl_core::{ContentBlock, MessageDocument, TextRole};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthChar;

/// Text safe to print: control characters (escape sequences, carriage
/// returns) and bidirectional overrides become spaces or go; tabs become
/// spaces. Line breaks are the caller's: they are kept only with `newlines`.
pub fn clean_with(text: &str, newlines: bool) -> String {
    text.chars()
        .filter_map(|c| match c {
            '\n' if newlines => Some('\n'),
            '\t' => Some(' '),
            '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' => None,
            c if c.is_control() => Some(' '),
            c => Some(c),
        })
        .collect()
}

/// [`clean_with`] on one line.
pub fn clean(text: &str) -> String {
    clean_with(text, false)
}

/// Blank lines a break may add (a message has at most this many lines anyway).
const MAX_BLANK_LINES: usize = 4096;

/// A message's content as lines, its links appended to `links` (numbered
/// from `links.len() + 1` in the text). The message keeps the lines it was
/// typed with: a break between blocks is `count - 1` empty lines (`count` at
/// either end), and a display formula written within a line (a break of 0
/// beside it) is drawn in that line, as written.
pub fn lines(
    document: &MessageDocument,
    source: &str,
    links: &mut Vec<String>,
) -> Vec<Line<'static>> {
    if document.blocks.is_empty() {
        return clean_with(source, true)
            .split('\n')
            .map(|line| Line::from(line.to_string()))
            .collect();
    }
    // Breaks asking for more line breaks than a message holds are malformed:
    // not drawn.
    let valid;
    let blocks = if document.breaks_valid() {
        &document.blocks
    } else {
        valid = document
            .blocks
            .iter()
            .filter(|block| !block.is_break())
            .cloned()
            .collect::<Vec<_>>();
        &valid
    };
    let content = |index: &usize| !blocks[*index].is_break();
    let first = (0..blocks.len()).find(content);
    let last = (0..blocks.len()).rev().find(content);
    let math = |index: Option<usize>| {
        index
            .and_then(|index| blocks.get(index))
            .is_some_and(|block| matches!(block, ContentBlock::Math { .. }))
    };
    let zero = |index: Option<usize>| {
        matches!(
            index.and_then(|index| blocks.get(index)),
            Some(ContentBlock::Break { count: 0 })
        )
    };
    let mut out: Vec<Line<'static>> = Vec::new();
    // The next block goes on in the last line.
    let mut join = false;
    for (index, block) in blocks.iter().enumerate() {
        if let ContentBlock::Break { count } = block {
            let count = (*count as usize).min(MAX_BLANK_LINES);
            let edge =
                first.is_none_or(|first| index < first) || last.is_none_or(|last| index > last);
            if count == 0 && !edge && (math(index.checked_sub(1)) || math(Some(index + 1))) {
                join = true;
                continue;
            }
            let blank = if edge { count } else { count.saturating_sub(1) };
            out.extend(std::iter::repeat_n(Line::from(""), blank));
            continue;
        }
        let mut lines = block_lines(
            block,
            zero(index.checked_sub(1)) || zero(Some(index + 1)),
            links,
        );
        if std::mem::take(&mut join)
            && let Some(previous) = out.last_mut()
            && !lines.is_empty()
        {
            let first = lines.remove(0);
            previous.spans.extend(first.spans);
        }
        out.extend(lines);
    }
    if out.is_empty() {
        out.push(Line::from(""));
    }
    out
}

/// One block's lines; `inline`: a display formula written within a line.
fn block_lines(block: &ContentBlock, inline: bool, links: &mut Vec<String>) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    match block {
        ContentBlock::Text { text, spans, role } => {
            let (prefix, style) = match role {
                TextRole::Paragraph => (String::new(), Style::default()),
                TextRole::Heading(level) => (
                    format!("{} ", "#".repeat(usize::from((*level).clamp(1, 3)))),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                // An item that goes on after a block inside it has no marker.
                TextRole::Item { depth, marker } => (
                    format!(
                        "{}{}",
                        "  ".repeat(usize::from(depth.saturating_sub(1)).min(8)),
                        if marker.is_empty() {
                            "  ".to_owned()
                        } else {
                            format!("{} ", clean(marker))
                        }
                    ),
                    Style::default(),
                ),
                TextRole::Quote => ("> ".into(), Style::default().add_modifier(Modifier::ITALIC)),
                TextRole::Rule => {
                    out.push(Line::from("────────"));
                    return out;
                }
            };
            let mut line: Vec<Span<'static>> = vec![Span::styled(prefix.clone(), style)];
            let mut push = |piece: &str, piece_style: Style, line: &mut Vec<Span<'static>>| {
                for (index, part) in clean_with(piece, true).split('\n').enumerate() {
                    if index > 0 {
                        out.push(Line::from(std::mem::take(line)));
                        // A continued list item or quote keeps its indent.
                        line.push(Span::styled(" ".repeat(prefix.chars().count()), style));
                    }
                    if !part.is_empty() {
                        line.push(Span::styled(part.to_string(), piece_style));
                    }
                }
            };
            let mut at = 0;
            for span in spans {
                let (Some(before), Some(inside)) =
                    (text.get(at..span.start), text.get(span.start..span.end))
                else {
                    continue;
                };
                push(before, style, &mut line);
                let mut piece_style = style;
                if span.style.bold {
                    piece_style = piece_style.add_modifier(Modifier::BOLD);
                }
                if span.style.italic {
                    piece_style = piece_style.add_modifier(Modifier::ITALIC);
                }
                if span.style.underline || span.link.is_some() {
                    piece_style = piece_style.add_modifier(Modifier::UNDERLINED);
                }
                if span.style.strike {
                    piece_style = piece_style.add_modifier(Modifier::CROSSED_OUT);
                }
                // Inline code: reverse video, so it reads apart from the text.
                if span.style.code {
                    piece_style = piece_style.add_modifier(Modifier::REVERSED);
                }
                push(inside, piece_style, &mut line);
                if let Some(link) = &span.link {
                    links.push(clean(link));
                    // The number says which link it is; the text may be
                    // the address itself or words.
                    line.push(Span::styled(
                        format!("[{}]", links.len()),
                        Style::default().add_modifier(Modifier::DIM),
                    ));
                }
                at = span.end;
            }
            if let Some(rest) = text.get(at..) {
                push(rest, style, &mut line);
            }
            out.push(Line::from(line));
        }
        ContentBlock::Code { language, runs } => {
            out.push(Line::styled(
                match language {
                    Some(language) => format!("[code: {}]", clean(language)),
                    None => "[code]".into(),
                },
                Style::default().add_modifier(Modifier::DIM),
            ));
            let mut code = Vec::new();
            let mut line = vec![Span::raw("  ")];
            for run in runs {
                let [r, g, b, _] = run.foreground;
                let mut style = Style::new().fg(Color::Rgb(r, g, b));
                if run.bold {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if run.italic {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                for (index, piece) in clean_with(&run.text, true).split('\n').enumerate() {
                    if index > 0 {
                        code.push(Line::from(std::mem::replace(
                            &mut line,
                            vec![Span::raw("  ")],
                        )));
                    }
                    if !piece.is_empty() {
                        line.push(Span::styled(piece.to_string(), style));
                    }
                }
            }
            code.push(Line::from(line));
            // The line break before the closing fence ends the last line.
            if code.len() > 1
                && code
                    .last()
                    .is_some_and(|line| plain(line).trim().is_empty())
            {
                code.pop();
            }
            out.extend(code);
        }
        ContentBlock::Image { width, height, .. } => out.push(Line::styled(
            format!("[image, {width}×{height}]"),
            Style::default().add_modifier(Modifier::DIM),
        )),
        // Within a line, as written there.
        ContentBlock::Math { source, .. } if inline => {
            out.extend(
                clean_with(source, true)
                    .split('\n')
                    .map(|line| Line::from(line.to_owned())),
            );
        }
        ContentBlock::Math { source, .. } => {
            out.push(Line::from(vec![
                Span::styled("[formula] ", Style::default().add_modifier(Modifier::DIM)),
                Span::raw(clean(source)),
            ]));
        }
        ContentBlock::Break { .. } => {}
    }
    out
}

/// The text of a line, without styles.
pub fn plain(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// `line` broken into lines of at most `width` columns, at a space where
/// one is near the end, else anywhere; following lines start with `indent`
/// spaces.
pub fn wrap(line: Line<'static>, width: usize, indent: usize) -> Vec<Line<'static>> {
    let width = width.max(indent + 8);
    let total: usize = line
        .spans
        .iter()
        .flat_map(|span| span.content.chars())
        .map(|c| c.width().unwrap_or(0))
        .sum();
    if total <= width {
        return vec![line];
    }
    // Characters with their styles, then cut.
    let cells: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|span| span.content.chars().map(move |c| (c, span.style)))
        .collect();
    let mut out = Vec::new();
    let mut start = 0;
    let mut first = true;
    while start < cells.len() {
        let room = if first { width } else { width - indent };
        let mut used = 0;
        let mut end = start;
        while end < cells.len() {
            let w = cells[end].0.width().unwrap_or(0);
            if used + w > room {
                break;
            }
            used += w;
            end += 1;
        }
        if end < cells.len() && cells[end].0 != ' ' {
            // Break after the last space in the second half of the line.
            if let Some(space) = (start + 1..end)
                .rev()
                .take((end - start) / 2)
                .find(|&k| cells[k].0 == ' ')
            {
                end = space + 1;
            }
        }
        if end == start {
            end = start + 1;
        }
        let mut spans: Vec<Span<'static>> = Vec::new();
        if !first {
            spans.push(Span::raw(" ".repeat(indent)));
        }
        let mut run = String::new();
        let mut style = cells[start].1;
        for &(c, cell_style) in &cells[start..end] {
            if cell_style != style && !run.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut run), style));
            }
            style = cell_style;
            run.push(c);
        }
        if !run.is_empty() {
            spans.push(Span::styled(run, style));
        }
        out.push(Line::from(spans));
        start = end;
        first = false;
        // The next line starts at a word.
        while start < cells.len() && cells[start].0 == ' ' {
            start += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuschl_core::Span as TextSpan;

    #[test]
    fn escapes_and_overrides_never_reach_the_terminal() {
        assert_eq!(clean("a\u{1b}[31mb\rc\u{202e}d\te"), "a [31mb cd e");
        assert_eq!(clean_with("one\ntwo", true), "one\ntwo");
        assert_eq!(clean("one\ntwo"), "one two");
    }

    #[test]
    fn links_are_numbered_and_images_and_formulas_are_described() {
        let document = MessageDocument {
            blocks: vec![
                ContentBlock::Text {
                    text: "see docs now".into(),
                    spans: vec![TextSpan {
                        start: 4,
                        end: 8,
                        link: Some("https://example.org/docs".into()),
                        ..Default::default()
                    }],
                    role: TextRole::Paragraph,
                },
                ContentBlock::Image {
                    width: 320,
                    height: 200,
                    rgba: String::new(),
                    source: None,
                },
                ContentBlock::Math {
                    source: "$$e^{i\\pi}+1=0$$".into(),
                    tex: "e^{i\\pi}+1=0".into(),
                },
            ],
        };
        let mut links = vec!["https://earlier.example".to_string()];
        let lines = lines(&document, "", &mut links);
        let text: Vec<String> = lines.iter().map(plain).collect();
        assert_eq!(
            text,
            [
                "see docs[2] now",
                "[image, 320×200]",
                "[formula] $$e^{i\\pi}+1=0$$"
            ]
        );
        assert_eq!(links[1], "https://example.org/docs");
    }

    #[test]
    fn syntax_colors_survive_multiline_runs_and_controls_remain_inert() {
        let document = MessageDocument {
            blocks: vec![ContentBlock::Code {
                language: Some("rust".into()),
                runs: vec![
                    nuschl_core::CodeRun {
                        text: "fn main() {\n".into(),
                        foreground: [195, 70, 200, 255],
                        bold: true,
                        italic: false,
                    },
                    nuschl_core::CodeRun {
                        text: "  \x1b]52;nope\x07\n}".into(),
                        foreground: [60, 160, 210, 255],
                        bold: false,
                        italic: true,
                    },
                ],
            }],
        };
        let lines = lines(&document, "", &mut Vec::new());
        assert!(
            lines[1]
                .spans
                .iter()
                .any(|span| span.style.fg == Some(Color::Rgb(195, 70, 200)))
        );
        assert!(
            lines[2]
                .spans
                .iter()
                .any(|span| span.style.fg == Some(Color::Rgb(60, 160, 210)))
        );
        assert!(
            lines
                .iter()
                .all(|line| !plain(line).contains(['\x1b', '\x07']))
        );
        assert!(plain(&lines[3]).ends_with('}'));
    }

    fn text(text: &str, role: TextRole) -> ContentBlock {
        ContentBlock::Text {
            text: text.into(),
            spans: Vec::new(),
            role,
        }
    }

    fn shown(blocks: Vec<ContentBlock>) -> Vec<String> {
        lines(&MessageDocument { blocks }, "", &mut Vec::new())
            .iter()
            .map(plain)
            .collect()
    }

    /// Rows of the line table (docs/markdown-rendering-plan.md, "Lines"), as
    /// the content worker renders them: the screen has the message's lines.
    #[test]
    fn breaks_keep_the_lines_of_the_message() {
        let code = |text: &str| ContentBlock::Code {
            language: None,
            runs: vec![nuschl_core::CodeRun {
                text: text.into(),
                foreground: [200, 200, 200, 255],
                bold: false,
                italic: false,
            }],
        };
        let item = |text_: &str| {
            text(
                text_,
                TextRole::Item {
                    depth: 1,
                    marker: "•".into(),
                },
            )
        };
        let breaks = |count| ContentBlock::Break { count };
        // `# T\n\n\n\ntext`
        assert_eq!(
            shown(vec![
                text("T", TextRole::Heading(1)),
                breaks(4),
                ContentBlock::text("text")
            ]),
            ["# T", "", "", "", "text"]
        );
        // `# T\ntext`
        assert_eq!(
            shown(vec![
                text("T", TextRole::Heading(1)),
                ContentBlock::text("text")
            ]),
            ["# T", "text"]
        );
        // `- a\n\n- b` and `- a\nb`
        assert_eq!(
            shown(vec![item("a"), breaks(2), item("b")]),
            ["• a", "", "• b"]
        );
        assert_eq!(shown(vec![item("a\nb")]), ["• a", "  b"]);
        // ```` ```\nx\n```\nafter ````: no line after the fence.
        assert_eq!(
            shown(vec![code("x\n"), ContentBlock::text("after")]),
            ["[code]", "  x", "after"]
        );
        // ```` ```\nx\n```\n\nafter ````, and a blank last line of code.
        assert_eq!(
            shown(vec![code("x\n\n"), breaks(2), ContentBlock::text("after")]),
            ["[code]", "  x", "  ", "", "after"]
        );
        // `\n\nhello`, `hello\n\n` and `a\n\n\nb`.
        assert_eq!(
            shown(vec![breaks(2), ContentBlock::text("hello")]),
            ["", "", "hello"]
        );
        assert_eq!(
            shown(vec![ContentBlock::text("hello"), breaks(2)]),
            ["hello", "", ""]
        );
        assert_eq!(
            shown(vec![ContentBlock::text("a\n\n\nb")]),
            ["a", "", "", "b"]
        );
        // `> q\n\n> r` and `---\n\ntext`.
        assert_eq!(
            shown(vec![
                text("q", TextRole::Quote),
                breaks(2),
                text("r", TextRole::Quote)
            ]),
            ["> q", "", "> r"]
        );
        assert_eq!(
            shown(vec![
                text("", TextRole::Rule),
                breaks(2),
                ContentBlock::text("text")
            ]),
            ["────────", "", "text"]
        );
        // An item that goes on after a block in it keeps its indent, no marker.
        assert_eq!(
            shown(vec![
                item("a"),
                breaks(2),
                text(
                    "c",
                    TextRole::Item {
                        depth: 1,
                        marker: String::new()
                    }
                )
            ]),
            ["• a", "", "  c"]
        );
    }

    /// Breaks asking for more line breaks than a message holds are not drawn.
    #[test]
    fn breaks_over_their_budget_are_not_drawn() {
        let breaks = |count| ContentBlock::Break { count };
        let shown = shown(vec![
            ContentBlock::text("a"),
            breaks(262_144),
            ContentBlock::text("b"),
            breaks(262_144),
            ContentBlock::text("c"),
            breaks(u32::MAX),
        ]);
        assert_eq!(shown, ["a", "b", "c"]);
    }

    #[test]
    fn a_display_formula_within_a_line_is_drawn_in_it() {
        let math = |source: &str| ContentBlock::Math {
            source: source.into(),
            tex: source.trim_matches('$').into(),
        };
        let zero = || ContentBlock::Break { count: 0 };
        assert_eq!(
            shown(vec![
                ContentBlock::text("so "),
                zero(),
                math("$$x$$"),
                zero(),
                ContentBlock::text(" and"),
                math("$$y$$"),
            ]),
            ["so $$x$$ and", "[formula] $$y$$"]
        );
    }

    #[test]
    fn inline_code_has_a_look_of_its_own() {
        let document = MessageDocument {
            blocks: vec![ContentBlock::Text {
                text: "run ls now".into(),
                spans: vec![TextSpan {
                    start: 4,
                    end: 6,
                    style: nuschl_core::Style {
                        code: true,
                        ..Default::default()
                    },
                    ..Default::default()
                }],
                role: TextRole::Paragraph,
            }],
        };
        let lines = lines(&document, "", &mut Vec::new());
        let code = lines[0]
            .spans
            .iter()
            .find(|span| span.content == "ls")
            .unwrap();
        assert!(code.style.add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn long_lines_wrap_at_spaces_with_an_indent() {
        let wrapped = wrap(Line::from("one two three four five six"), 12, 2);
        let text: Vec<String> = wrapped.iter().map(plain).collect();
        assert_eq!(text, ["one two ", "  three four", "  five six"]);
        // A word longer than the line is cut.
        let long = wrap(Line::from("abcdefghijklmnopqrstuvwxyz"), 10, 0);
        assert_eq!(long.iter().map(plain).collect::<Vec<_>>()[0], "abcdefghij");
    }
}
