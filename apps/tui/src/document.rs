// SPDX-License-Identifier: EUPL-1.2
//! Messages as terminal text. The content worker already turned untrusted
//! input into a `MessageDocument`; this lays it out as lines of styled
//! spans. Images and display formulas become placeholders with what they
//! say in words (the formula's source; an image's size, since the worker
//! keeps no alternative text), and links are numbered so they can be listed
//! and opened. Every string from a server goes through [`clean`] first: no
//! control characters or direction overrides reach the terminal.
use nuschel_core::{ContentBlock, MessageDocument, TextRole};
use ratatui::{
    style::{Modifier, Style},
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

/// A message's content as lines, its links appended to `links` (numbered
/// from `links.len() + 1` in the text).
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
    let mut out = Vec::new();
    for block in &document.blocks {
        match block {
            ContentBlock::Text { text, spans, role } => {
                let (prefix, style) = match role {
                    TextRole::Paragraph => (String::new(), Style::default()),
                    TextRole::Heading(level) => (
                        format!("{} ", "#".repeat(usize::from((*level).clamp(1, 3)))),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    TextRole::Item { depth, marker } => (
                        format!(
                            "{}{} ",
                            "  ".repeat(usize::from(depth.saturating_sub(1)).min(8)),
                            clean(marker)
                        ),
                        Style::default(),
                    ),
                    TextRole::Quote => {
                        ("> ".into(), Style::default().add_modifier(Modifier::ITALIC))
                    }
                    TextRole::Rule => {
                        out.push(Line::from("────────"));
                        continue;
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
                let code: String = runs.iter().map(|run| run.text.as_str()).collect();
                for line in clean_with(&code, true).trim_end_matches('\n').split('\n') {
                    out.push(Line::from(format!("  {line}")));
                }
            }
            ContentBlock::Image { width, height, .. } => out.push(Line::styled(
                format!("[image, {width}×{height}]"),
                Style::default().add_modifier(Modifier::DIM),
            )),
            ContentBlock::Math { source, .. } => {
                out.push(Line::from(vec![
                    Span::styled("[formula] ", Style::default().add_modifier(Modifier::DIM)),
                    Span::raw(clean(source)),
                ]));
            }
        }
    }
    if out.is_empty() {
        out.push(Line::from(""));
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
    use nuschel_core::Span as TextSpan;

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
    fn long_lines_wrap_at_spaces_with_an_indent() {
        let wrapped = wrap(Line::from("one two three four five six"), 12, 2);
        let text: Vec<String> = wrapped.iter().map(plain).collect();
        assert_eq!(text, ["one two ", "  three four", "  five six"]);
        // A word longer than the line is cut.
        let long = wrap(Line::from("abcdefghijklmnopqrstuvwxyz"), 10, 0);
        assert_eq!(long.iter().map(plain).collect::<Vec<_>>()[0], "abcdefghij");
    }
}
