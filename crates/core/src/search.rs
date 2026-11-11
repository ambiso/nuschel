// SPDX-License-Identifier: EUPL-1.2
//! Message search as people type it: words, "exact phrases" and `from:name`.
//! Storage turns a query into its full-text syntax; the desktop highlights
//! the same tokens in the results.

use crate::HistoryEntry;
use serde::{Deserialize, Serialize};
use std::ops::Range;

/// The longest query storage accepts, in bytes.
pub const MAX_QUERY_BYTES: usize = 512;

/// A parsed search query. Every phrase must match.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    /// Tokens that must appear in this order; a word is the run of its tokens.
    pub phrases: Vec<Vec<String>>,
    /// The last phrase's last token also matches longer words: it is still
    /// being typed.
    pub prefix: bool,
    /// The lowercased start of the author's name (`from:ada`, `from:"ada l"`).
    pub from: Option<String>,
}

impl Query {
    pub fn parse(text: &str) -> Self {
        let mut query = Self::default();
        let mut rest = text.trim_start();
        while !rest.is_empty() {
            if rest
                .get(..5)
                .is_some_and(|start| start.eq_ignore_ascii_case("from:"))
            {
                let (name, after) = match &rest[5..] {
                    name if name.starts_with(char::is_whitespace) => ("", name),
                    name => {
                        let (name, _, _, after) = next_word(name);
                        (name, after)
                    }
                };
                rest = after.trim_start();
                let name = name.trim().to_lowercase();
                if !name.is_empty() {
                    query.from = Some(name);
                }
                query.prefix = false;
                continue;
            }
            let (word, _, open, after) = next_word(rest);
            rest = after.trim_start();
            // Still being typed: nothing (not even a space) follows.
            let open = open && !word.ends_with(char::is_whitespace);
            let lowercase = word.to_lowercase();
            let phrase: Vec<String> = tokens(&lowercase).map(|(_, token)| token).collect();
            if !phrase.is_empty() {
                query.phrases.push(phrase);
                query.prefix = open;
            }
        }
        query
    }

    pub fn is_empty(&self) -> bool {
        self.phrases.is_empty() && self.from.is_none()
    }

    /// Whether a lowercased token of a message matches one of the query's.
    pub fn matches(&self, token: &str) -> bool {
        let last = self.phrases.len().wrapping_sub(1);
        self.phrases.iter().enumerate().any(|(index, phrase)| {
            phrase.iter().enumerate().any(|(position, word)| {
                word == token
                    || (self.prefix
                        && index == last
                        && position + 1 == phrase.len()
                        && token.starts_with(word.as_str()))
            })
        })
    }

    /// Byte ranges of the words in `text` that match the query.
    pub fn highlights(&self, text: &str) -> Vec<Range<usize>> {
        tokens(text)
            .filter(|(_, token)| self.matches(token))
            .map(|(range, _)| range)
            .collect()
    }
}

/// The next word or "quoted phrase" of `text`, whether it was quoted,
/// whether it runs to the end unfinished (an unclosed quote does), and what
/// follows.
fn next_word(text: &str) -> (&str, bool, bool, &str) {
    if let Some(quoted) = text.strip_prefix('"') {
        match quoted.find('"') {
            Some(end) => (&quoted[..end], true, false, &quoted[end + 1..]),
            None => (quoted, true, true, ""),
        }
    } else {
        let end = text.find(char::is_whitespace).unwrap_or(text.len());
        (&text[..end], false, end == text.len(), &text[end..])
    }
}

/// Words as full-text search sees them: runs of letters and digits,
/// lowercased, with their byte ranges in `text`.
pub fn tokens(text: &str) -> impl Iterator<Item = (Range<usize>, String)> + '_ {
    let mut chars = text.char_indices().peekable();
    std::iter::from_fn(move || {
        let (start, _) = chars.find(|(_, c)| c.is_alphanumeric())?;
        let mut end = text.len();
        while let Some(&(index, c)) = chars.peek() {
            if !c.is_alphanumeric() {
                end = index;
                break;
            }
            chars.next();
        }
        Some((start..end, text[start..end].to_lowercase()))
    })
}

/// Where the next page of results starts: after the last result shown,
/// scored as of the first page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchCursor {
    pub now_ms: i64,
    pub score: i64,
    pub id: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchPage {
    /// Best match first; each body is the message's visible text, without
    /// inline images.
    pub hits: Vec<HistoryEntry>,
    /// More results follow from here; None means these were the last.
    pub next: Option<SearchCursor>,
    /// Older messages are still being added to the search index.
    pub indexing: bool,
}

#[cfg(test)]
mod tests {
    use super::{Query, tokens};

    fn phrases(query: &Query) -> Vec<Vec<&str>> {
        query
            .phrases
            .iter()
            .map(|phrase| phrase.iter().map(String::as_str).collect())
            .collect()
    }

    #[test]
    fn words_phrases_and_authors() {
        let query = Query::parse(r#"Deploy "the RUST server" from:Ada rel"#);
        assert_eq!(
            phrases(&query),
            [vec!["deploy"], vec!["the", "rust", "server"], vec!["rel"]]
        );
        assert!(query.prefix, "the last word is still being typed");
        assert_eq!(query.from.as_deref(), Some("ada"));
        assert!(!Query::parse("rel ").prefix, "a space ends the word");
        assert!(!Query::parse(r#""exact phrase""#).prefix);
        assert!(
            Query::parse(r#""exact phr"#).prefix,
            "an open quote still types"
        );
        let author = Query::parse(r#"from:"Ada Love" hello"#);
        assert_eq!(author.from.as_deref(), Some("ada love"));
        assert_eq!(phrases(&author), [vec!["hello"]]);
        // Punctuation separates tokens and alone is no word.
        assert_eq!(phrases(&Query::parse("foo-bar !!! ")), [vec!["foo", "bar"]]);
        assert!(Query::parse(" \"\" from: ").is_empty());
    }

    #[test]
    fn highlights_follow_tokens() {
        let text = "Ärger mit dem Server, servers and SERVE";
        assert_eq!(
            tokens(text).map(|(_, token)| token).collect::<Vec<_>>(),
            ["ärger", "mit", "dem", "server", "servers", "and", "serve"]
        );
        let query = Query::parse("ärger serv");
        let found: Vec<&str> = query
            .highlights(text)
            .into_iter()
            .map(|range| &text[range])
            .collect();
        assert_eq!(found, ["Ärger", "Server", "servers", "SERVE"]);
        assert_eq!(Query::parse("serv ").highlights(text), []);
    }
}
