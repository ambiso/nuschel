// SPDX-License-Identifier: EUPL-1.2
//! Message search in local history (Ctrl+F): full text in the open
//! conversation, its server, or every server. What a search covers, how its
//! results are named (where, by whom, a snippet around the match), and
//! where a chosen result's message is now are decided here; a UI keeps the
//! field and the list, and asks for the result's page
//! ([`Client::show_message`]).
use crate::{Client, Conversation, model::TextTarget, names, server::ServerState, timeline::Page};
use nuschel_core::{HistoryEntry, search::Query};
use std::{collections::HashMap, ops::Range};

/// Servers one search covers (the storage worker's limit).
pub const MAX_SERVERS: usize = 64;
/// Bytes of a result's text shown around its first match.
pub const SNIPPET: usize = 240;

/// What a search covers, from the conversation it was started in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Conversation,
    Server,
    All,
}

/// One result, as a UI lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub server: String,
    /// The conversation's storage key on that server.
    pub storage_key: String,
    /// Its local row id, and its seq on a Nuschel server (whose timeline
    /// counts in seqs).
    pub id: i64,
    pub seq: Option<i64>,
    /// `#channel`, `@ person`, or where it was once.
    pub place: String,
    pub server_name: String,
    /// Your nickname for someone online under the author's name, else the
    /// name it was sent under.
    pub author: String,
    pub observed_at_ms: i64,
    /// The text on one line around its first match, and where the query's
    /// words are in it (byte ranges).
    pub snippet: String,
    pub highlights: Vec<Range<usize>>,
}

/// Where a result's message is now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Located {
    /// In conversation `position` of the server, where its timeline knows
    /// it as `id`.
    Found { position: usize, id: i64 },
    /// In an earlier direct conversation with someone who is online again
    /// as `session`: open it (the UI's open direct), then locate again.
    Reopen { session: u32 },
}

/// What to tell the user when a result's direct conversation cannot open.
pub const OPENS_ONLINE: &str = "That conversation opens once the person is online again.";

impl Client {
    /// The history identities and the conversation (storage key) a search
    /// from `origin` covers.
    pub fn search_scope(
        &self,
        origin: &Conversation,
        scope: Scope,
    ) -> Result<(Vec<String>, Option<String>), &'static str> {
        let unknown = "Connect to this server once to search its history.";
        let server = self
            .servers
            .iter()
            .find(|server| server.id == origin.server)
            .ok_or(unknown)?;
        match scope {
            Scope::Conversation => {
                let channel = server
                    .channels
                    .iter()
                    .find(|channel| channel.id == origin.channel)
                    .ok_or(unknown)?;
                Ok((
                    vec![server.history_identity().ok_or(unknown)?],
                    Some(channel.storage_key.clone()),
                ))
            }
            Scope::Server => Ok((vec![server.history_identity().ok_or(unknown)?], None)),
            Scope::All => {
                let servers: Vec<String> = self
                    .servers
                    .iter()
                    .filter_map(|server| server.history_identity())
                    .collect();
                if servers.len() > MAX_SERVERS {
                    return Err("Search covers at most 64 servers at once.");
                }
                Ok((servers, None))
            }
        }
    }

    /// A page's results as a UI lists them; results of servers no longer
    /// saved are left out.
    pub fn search_hits(&self, hits: Vec<HistoryEntry>, query: &str) -> Vec<Hit> {
        let query = Query::parse(query);
        let identities: HashMap<String, &ServerState> = self
            .servers
            .iter()
            .filter_map(|server| Some((server.history_identity()?, server)))
            .collect();
        let names = &self.preferences;
        let mut out = Vec::with_capacity(hits.len());
        for hit in hits {
            let Some(server) = identities.get(&hit.message.server) else {
                continue;
            };
            let key = hit.message.channel;
            let channel = server
                .channels
                .iter()
                .find(|channel| channel.storage_key == key);
            let peer = channel.and_then(|channel| channel.direct_peer.as_ref());
            let place = match (channel, peer) {
                (Some(channel), _) if matches!(channel.target, TextTarget::Channel(_)) => {
                    format!("#{}", channel.name)
                }
                (_, Some(peer)) => format!("@ {}", names::display_name(peer, names)),
                (Some(channel), None) => channel.name.clone(),
                (None, _) if key.starts_with("direct:") => "Earlier direct conversation".into(),
                (None, _) => "Earlier channel".into(),
            };
            let (snippet, highlights) = snippet(&hit.message.body, &query);
            let author = names::author(&hit.message.author, server.users.values(), peer);
            let author = names::author_name(&hit.message.author, author, names).to_string();
            out.push(Hit {
                server: server.id.clone(),
                storage_key: key,
                id: hit.id,
                seq: hit.seq,
                place,
                server_name: server.name.clone(),
                author,
                observed_at_ms: hit.message.observed_at_ms,
                snippet,
                highlights,
            });
        }
        out
    }

    /// Where stored message `id` (`seq` on a Nuschel server) of conversation
    /// `storage_key` on server `index` is now. Fails with what to tell the
    /// user.
    pub fn locate(
        &self,
        index: usize,
        storage_key: &str,
        (id, seq): (i64, Option<i64>),
    ) -> Result<Located, &'static str> {
        let server = self.servers.get(index).ok_or(OPENS_ONLINE)?;
        let Some(position) = server
            .channels
            .iter()
            .position(|channel| channel.storage_key == storage_key)
        else {
            // An earlier direct conversation, with someone who is online again.
            return storage_key
                .strip_prefix("direct:")
                .and_then(|hash| {
                    server
                        .users
                        .values()
                        .find(|user| user.certificate_hash.as_deref() == Some(hash))
                })
                .map(|user| Located::Reopen {
                    session: user.session,
                })
                .ok_or(OPENS_ONLINE);
        };
        // A server-history timeline counts in seqs: a kept message is found
        // there by its seq, and pages come from the server.
        let id = match (server.archived(&server.channels[position]), seq) {
            (true, Some(seq)) => seq,
            (true, None) => return Err("That message is not in the server's history."),
            (false, _) => id,
        };
        Ok(Located::Found { position, id })
    }

    /// Show message `id` of the open conversation (`position` on server
    /// `index`): the reader stops following the newest message, and unless
    /// it is loaded, the history around it is asked for (it shows with
    /// [`Update::PageShown`](crate::Update::PageShown)). Returns whether it
    /// is loaded already.
    pub fn show_message(&mut self, index: usize, position: usize, id: i64) -> Result<bool, String> {
        self.focus.following = false;
        let loaded = self.servers[index].channels[position]
            .timeline
            .messages
            .iter()
            .any(|message| message.id == Some(id));
        if !loaded {
            self.load_page(index, position, Page::Around(id))?;
        }
        Ok(loaded)
    }
}

/// The largest character boundary of `text` at or below `index`.
fn boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// A result's text on one line, from a little before its first match, and
/// where the query's words are in it.
pub fn snippet(text: &str, query: &Query) -> (String, Vec<Range<usize>>) {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let highlights = query.highlights(&line);
    let first = highlights.first().map_or(0, |range| range.start);
    let start = if first > SNIPPET / 4 {
        // At a word boundary, about 60 bytes ahead of the match.
        let from = boundary(&line, first - SNIPPET / 4);
        line[from..first]
            .find(' ')
            .map_or(from, |space| from + space + 1)
    } else {
        0
    };
    let mut end = boundary(&line, start + SNIPPET);
    if end < line.len()
        && let Some(space) = line[start..end].rfind(' ')
        && start + space > first
    {
        end = start + space;
    }
    let ellipsis = if start > 0 { "…" } else { "" };
    let shift = ellipsis.len();
    let mut snippet = format!("{ellipsis}{}", &line[start..end]);
    if end < line.len() {
        snippet.push('…');
    }
    let highlights = highlights
        .into_iter()
        .filter(|range| range.start >= start && range.end <= end)
        .map(|range| range.start - start + shift..range.end - start + shift)
        .collect();
    (snippet, highlights)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_start_near_the_first_match() {
        let text = format!(
            "{} needle in\n\nthe   haystack {}",
            "hay ".repeat(40),
            "straw ".repeat(60)
        );
        let (long, highlights) = snippet(&text, &Query::parse("needle"));
        assert!(long.starts_with("…hay ") && long.ends_with('…'), "{long}");
        assert!(long.contains("needle in the haystack"), "{long}");
        assert_eq!(highlights.len(), 1);
        assert_eq!(&long[highlights[0].clone()], "needle");
        let (short, highlights) = snippet("Ärger mit dem Server", &Query::parse("serv"));
        assert_eq!(short, "Ärger mit dem Server");
        assert_eq!(&short[highlights[0].clone()], "Server");
    }
}
