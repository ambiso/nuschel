// SPDX-License-Identifier: EUPL-1.2
//! Run this parser in the content worker, never on the UI or audio thread.

use nuschel_core::{CodeRun, ContentBlock, MAX_CONTENT_BYTES, MessageDocument};
use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Theme, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("message exceeds the {MAX_CONTENT_BYTES}-byte content processing limit")]
    TooLarge,
    #[error("syntax highlighting failed: {0}")]
    Highlight(#[from] syntect::Error),
    #[error("the bundled highlighting theme is missing")]
    MissingTheme,
}

pub struct ContentRenderer {
    syntaxes: SyntaxSet,
    theme: Theme,
}

impl ContentRenderer {
    pub fn new() -> Result<Self, Error> {
        let mut themes = ThemeSet::load_defaults();
        Ok(Self {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            theme: themes
                .themes
                .remove("base16-ocean.dark")
                .ok_or(Error::MissingTheme)?,
        })
    }

    /// Ordinary message text is literal, including all newlines and blank lines.
    /// Only fenced code blocks are interpreted. HTML and embedded resources stay inert text.
    pub fn render(&self, source: &str) -> Result<MessageDocument, Error> {
        if source.len() > MAX_CONTENT_BYTES {
            return Err(Error::TooLarge);
        }
        let mut blocks = Vec::new();
        let mut consumed = 0;
        let mut code: Option<(usize, Option<String>, String)> = None;
        for (event, range) in Parser::new(source).into_offset_iter() {
            match event {
                Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) => {
                    let language = info
                        .split_whitespace()
                        .next()
                        .map(|value| value.chars().take(64).collect());
                    code = Some((range.start, language, String::new()));
                }
                Event::Text(text) if code.is_some() => {
                    if let Some((_, _, body)) = code.as_mut() {
                        body.push_str(&text);
                    }
                }
                Event::End(TagEnd::CodeBlock) => {
                    if let Some((start, language, body)) = code.take() {
                        if start > consumed {
                            blocks.push(ContentBlock::Text {
                                text: source[consumed..start].to_owned(),
                            });
                        }
                        let runs = self.highlight(&body, language.as_deref())?;
                        blocks.push(ContentBlock::Code { language, runs });
                        consumed = range.end;
                    }
                }
                _ => {}
            }
        }
        if consumed < source.len() || blocks.is_empty() {
            blocks.push(ContentBlock::Text {
                text: source[consumed..].to_owned(),
            });
        }
        Ok(MessageDocument { blocks })
    }

    fn highlight(&self, source: &str, language: Option<&str>) -> Result<Vec<CodeRun>, Error> {
        let syntax = language
            .and_then(|token| self.syntaxes.find_syntax_by_token(token))
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text());
        let mut highlighter = HighlightLines::new(syntax, &self.theme);
        let mut runs = Vec::new();
        for line in LinesWithEndings::from(source) {
            for (style, text) in highlighter.highlight_line(line, &self.syntaxes)? {
                runs.push(CodeRun {
                    text: text.to_owned(),
                    foreground: [
                        style.foreground.r,
                        style.foreground.g,
                        style.foreground.b,
                        style.foreground.a,
                    ],
                    bold: style.font_style.contains(FontStyle::BOLD),
                    italic: style.font_style.contains(FontStyle::ITALIC),
                });
            }
        }
        Ok(runs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_messages_keep_blank_lines_and_html_inert() -> Result<(), Error> {
        let source = "first\n\nsecond\n<script>never execute</script>";
        let result = ContentRenderer::new()?.render(source)?;
        assert_eq!(
            result.blocks,
            vec![ContentBlock::Text {
                text: source.to_owned()
            }]
        );
        Ok(())
    }

    #[test]
    fn rust_code_is_highlighted_and_retains_its_newlines() -> Result<(), Error> {
        let result = ContentRenderer::new()?.render("before\n```rust\nfn main() {}\n```\nafter")?;
        let Some(ContentBlock::Code { language, runs }) = result.blocks.get(1) else {
            panic!("expected a code block between the two text blocks");
        };
        assert_eq!(language.as_deref(), Some("rust"));
        assert_eq!(
            runs.iter().map(|run| run.text.as_str()).collect::<String>(),
            "fn main() {}\n"
        );
        assert!(
            runs.windows(2)
                .any(|pair| pair[0].foreground != pair[1].foreground)
        );
        Ok(())
    }

    #[test]
    fn rejects_large_messages_before_parsing() -> Result<(), Error> {
        assert!(matches!(
            ContentRenderer::new()?.render(&"x".repeat(MAX_CONTENT_BYTES + 1)),
            Err(Error::TooLarge)
        ));
        Ok(())
    }
}
