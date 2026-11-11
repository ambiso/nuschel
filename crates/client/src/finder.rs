// SPDX-License-Identifier: EUPL-1.2
//! The quick switcher's results (Ctrl+K): channels, people and direct
//! conversations of every server, found by a few typed letters and ranked by
//! where the user went lately and what is unread. People are listed per
//! server: a Mumble server only claims who someone is (even their
//! certificate hash), so the same name or hash on two servers is never taken
//! for one person. A UI keeps the field, the highlighted result and the
//! drawing; what is listed, and in which order, is decided here.
use crate::{
    Client, Conversation,
    model::{TextTarget, User},
    names::{self, Names},
    server::{ServerState, Status},
};
use std::{collections::HashMap, ops::Range};

/// Conversations remembered for ranking, most recent first.
pub const RECENT: usize = 20;

/// Where the user went recently, most recent first.
#[derive(Clone, Debug, Default)]
pub struct Recent(Vec<Conversation>);

impl Recent {
    /// Note a conversation the user opened.
    pub fn visited(&mut self, conversation: &Conversation) {
        self.0.retain(|recent| recent != conversation);
        self.0.insert(0, conversation.clone());
        self.0.truncate(RECENT);
    }

    fn position(&self, conversation: &Conversation) -> Option<usize> {
        self.0.iter().position(|recent| recent == conversation)
    }
}

/// What a result goes to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Channel(u32),
    /// Someone online, by session.
    Person(u32),
    /// A direct conversation, by its conversation's id in `channels`.
    Direct(u32),
}

impl Target {
    /// What a result is, in words (for its accessible label).
    pub fn kind(&self) -> &'static str {
        match self {
            Target::Channel(_) => "channel",
            Target::Person(_) => "person",
            Target::Direct(_) => "direct conversation",
        }
    }
}

/// One result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub server: String,
    pub target: Target,
    pub name: String,
    /// The channels above a channel, or where a person is.
    pub path: String,
    /// What else finds it, less well: a channel's path and name, the real
    /// name of someone with a nickname.
    pub alias: String,
    /// The matched characters of `name`, as byte ranges.
    pub highlights: Vec<Range<usize>>,
    pub score: i32,
    pub unread: usize,
}

/// Everything the switcher can go to, best match for `query` first. With an
/// empty query: suggestions, where the user was lately and what is unread.
pub fn items(client: &Client, recent: &Recent, query: &str) -> Vec<Item> {
    let query: Vec<char> = query.trim().chars().map(fold).collect();
    let selected = client.navigation.selection();
    let names = &client.preferences;
    let mut items = Vec::new();
    for server in &client.servers {
        let connected = server.status == Status::Connected;
        let voice = server.voice_channel();
        let places = Places::new(server);
        let rank = |channel: Option<u32>, unread: usize, mention: bool| {
            let conversation = channel.map(|channel| Conversation {
                server: server.id.clone(),
                channel,
            });
            let recent = conversation
                .as_ref()
                .and_then(|conversation| recent.position(conversation));
            let mut score = recent.map_or(0, |at| 120 - 6 * at as i32);
            score += if mention {
                80
            } else if unread > 0 {
                40
            } else {
                0
            };
            score += if connected { 20 } else { 0 };
            if conversation.is_some() && conversation.as_ref() == selected {
                score -= 40;
            }
            (score, recent.is_some() || unread > 0)
        };
        for channel in &server.channels {
            let (target, path, name, alias) = match &channel.target {
                TextTarget::Channel(id) => {
                    let path = places.path(channel.parent).join(" › ");
                    let alias = format!("{path} {}", channel.name);
                    (Target::Channel(*id), path, channel.name.clone(), alias)
                }
                // Someone online is listed as a person instead.
                TextTarget::Direct(session)
                    if connected
                        && server.users.get(session).is_some_and(|user| {
                            channel
                                .direct_peer
                                .as_ref()
                                .is_none_or(|peer| peer.instance == user.instance)
                        }) =>
                {
                    continue;
                }
                // Likewise a Nuschel server's direct conversation with
                // someone online.
                TextTarget::DirectConversation(_)
                    if connected
                        && channel
                            .direct_peer
                            .as_ref()
                            .and_then(|peer| peer.user_id)
                            .is_some_and(|id| {
                                server.users.values().any(|user| user.user_id == Some(id))
                            }) =>
                {
                    continue;
                }
                TextTarget::Direct(_) | TextTarget::DirectConversation(_) => {
                    let (name, alias) = match &channel.direct_peer {
                        Some(peer) => person(peer, names),
                        None => (channel.name.trim_start_matches("@ ").into(), String::new()),
                    };
                    let path = "Direct conversation".to_string();
                    (Target::Direct(channel.id), path, name, alias)
                }
            };
            let (mut score, suggested) =
                rank(Some(channel.id), channel.unread, channel.unread_mention);
            if matches!(target, Target::Channel(id) if Some(id) == voice) {
                score += 30;
            }
            items.push(Item {
                server: server.id.clone(),
                target,
                name,
                path,
                alias,
                highlights: Vec::new(),
                score: if suggested || !query.is_empty() {
                    score
                } else {
                    i32::MIN
                },
                unread: channel.unread,
            });
        }
        if !connected {
            continue;
        }
        for user in server.users.values() {
            if Some(user.session) == server.own_session {
                continue;
            }
            // Their direct conversation, if one is open (on a Nuschel
            // server, the account's).
            let direct = server.channels.iter().find(|channel| {
                channel.target == TextTarget::Direct(user.session)
                    && channel
                        .direct_peer
                        .as_ref()
                        .is_none_or(|peer| peer.instance == user.instance)
                    || matches!(channel.target, TextTarget::DirectConversation(_))
                        && user.user_id.is_some()
                        && channel.direct_peer.as_ref().and_then(|peer| peer.user_id)
                            == user.user_id
            });
            let (mut score, suggested) = rank(
                direct.map(|channel| channel.id),
                direct.map_or(0, |channel| channel.unread),
                direct.is_some_and(|channel| channel.unread_mention),
            );
            if server.talking.contains(&user.session) {
                score += 20;
            }
            if Some(user.channel) == voice {
                score += 30;
            }
            let (name, alias) = person(user, names);
            let mut path = places.path(Some(user.channel));
            if path.is_empty() {
                path.push(places.name(user.channel));
            }
            items.push(Item {
                server: server.id.clone(),
                target: Target::Person(user.session),
                path: format!("in {}", path.join(" › ")),
                name,
                alias,
                highlights: Vec::new(),
                score: if suggested || !query.is_empty() {
                    score
                } else {
                    i32::MIN
                },
                unread: direct.map_or(0, |channel| channel.unread),
            });
        }
    }
    if query.is_empty() {
        items.retain(|item| item.score != i32::MIN);
    } else {
        items.retain_mut(|item| {
            let found = fuzzy(&query, &item.name)
                .or_else(|| fuzzy(&query, &item.alias).map(|(score, _)| (score - 150, Vec::new())));
            let Some((score, highlights)) = found else {
                return false;
            };
            item.score += score;
            item.highlights = highlights;
            true
        });
    }
    items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.name.cmp(&b.name)));
    items
}

/// Where the highlight is in `items`: the selected result, or the first.
pub fn selected_index(selected: Option<&(String, Target)>, items: &[Item]) -> usize {
    selected
        .and_then(|(server, target)| {
            items
                .iter()
                .position(|item| &item.server == server && &item.target == target)
        })
        .unwrap_or(0)
}

/// A person's shown name (your nickname for them, else theirs), and their
/// own name behind a nickname.
fn person(user: &User, names: &impl Names) -> (String, String) {
    let name = names::display_name(user, names);
    let real = if name == user.name { "" } else { &user.name };
    (name.to_string(), real.to_string())
}

/// One server's channel names and parents, for paths.
pub struct Places<'a>(HashMap<u32, (&'a str, Option<u32>)>);
impl<'a> Places<'a> {
    pub fn new(server: &'a ServerState) -> Self {
        Self(
            server
                .channels
                .iter()
                .filter(|channel| matches!(channel.target, TextTarget::Channel(_)))
                .map(|channel| (channel.id, (channel.name.as_str(), channel.parent)))
                .collect(),
        )
    }

    pub fn name(&self, id: u32) -> &'a str {
        self.0.get(&id).map_or("", |(name, _)| name)
    }

    /// The names from the top down to `channel`, without the root (the
    /// server itself).
    pub fn path(&self, channel: Option<u32>) -> Vec<&'a str> {
        let mut names = Vec::new();
        let mut current = channel;
        // Bounded, should a server send a cycle.
        while let Some(id) = current
            && names.len() < 32
        {
            let Some(&(name, parent)) = self.0.get(&id) else {
                break;
            };
            if parent.is_none() {
                break;
            }
            names.push(name);
            current = parent;
        }
        names.reverse();
        names
    }
}

/// Case folding for matching, one character for one.
pub fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// How well `query` (folded, not empty) matches `text`, and the byte ranges
/// of `text` that matched. Best is all of it, then its start, the start of a
/// word, any run of it, then letters in order (word initials first).
pub fn fuzzy(query: &[char], text: &str) -> Option<(i32, Vec<Range<usize>>)> {
    let original: Vec<(usize, char)> = text.char_indices().collect();
    let chars: Vec<char> = original.iter().map(|&(_, c)| fold(c)).collect();
    let (n, m) = (chars.len(), query.len());
    if m == 0 || m > n {
        return None;
    }
    let offset = |k: usize| original.get(k).map_or(text.len(), |&(at, _)| at);
    let word_start = |k: usize| {
        k == 0 || {
            let (before, here) = (original[k - 1].1, original[k].1);
            (!before.is_alphanumeric() && here.is_alphanumeric())
                || (before.is_lowercase() && here.is_uppercase())
        }
    };
    let runs: Vec<usize> = (0..=n - m)
        .filter(|&start| chars[start..start + m] == *query)
        .collect();
    if let Some(&start) = runs
        .iter()
        .find(|&&start| word_start(start))
        .or(runs.first())
    {
        let score = match start {
            0 if m == n => 1000,
            0 => 900 - (n - m).min(99) as i32,
            _ if word_start(start) => 700 - start.min(99) as i32,
            _ => 500 - start.min(99) as i32,
        };
        let run = offset(start)..offset(start + m);
        return Some((score, Vec::from([run])));
    }
    // Letters in order: each at the next word start if there is one, else
    // the next occurrence; failing that, plainly the next occurrence.
    let pick = |prefer_words: bool| {
        let mut positions = Vec::with_capacity(m);
        let mut from = 0;
        for &wanted in query {
            let next = (from..n)
                .find(|&k| prefer_words && chars[k] == wanted && word_start(k))
                .or_else(|| (from..n).find(|&k| chars[k] == wanted))?;
            positions.push(next);
            from = next + 1;
        }
        Some(positions)
    };
    let positions = pick(true).or_else(|| pick(false))?;
    let score = if positions.iter().all(|&k| word_start(k)) {
        400
    } else {
        let spread = positions[m - 1] - positions[0] + 1 - m;
        200 - spread.min(150) as i32
    };
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for k in positions {
        match ranges.last_mut() {
            Some(range) if range.end == offset(k) => range.end = offset(k + 1),
            _ => ranges.push(offset(k)..offset(k + 1)),
        }
    }
    Some((score, ranges))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(query: &str, text: &str) -> Option<i32> {
        let query: Vec<char> = query.chars().map(fold).collect();
        fuzzy(&query, text).map(|(score, _)| score)
    }

    #[test]
    fn prefixes_and_word_starts_beat_other_matches() {
        assert!(score("dev", "Dev") > score("dev", "Development"));
        assert!(score("dev", "Development") > score("dev", "Web Development"));
        assert!(score("dev", "Web Development") > score("dev", "Abdevel"));
        assert!(score("dev", "Abdevel") > score("wd", "Web Development"));
        assert!(score("wd", "Web Development") > score("wbd", "Web Development"));
        assert_eq!(score("xyz", "Web Development"), None);
        assert_eq!(score("ÄR", "Ärger"), Some(900 - 3));
        // CamelCase counts as words.
        assert_eq!(score("gt", "GameTime"), Some(400));
    }

    #[test]
    fn highlights_are_byte_ranges_of_the_matched_letters() {
        let query: Vec<char> = "wde".chars().collect();
        let (_, ranges) = fuzzy(&query, "Wö Development").unwrap();
        assert_eq!(ranges, [0..1, 4..6]);
    }

    #[test]
    fn recent_conversations_come_first_and_are_bounded() {
        let mut recent = Recent::default();
        for channel in 0..(RECENT as u32 + 5) {
            recent.visited(&Conversation {
                server: "s".into(),
                channel,
            });
        }
        let first = Conversation {
            server: "s".into(),
            channel: RECENT as u32 + 4,
        };
        assert_eq!(recent.position(&first), Some(0));
        recent.visited(&Conversation {
            server: "s".into(),
            channel: 7,
        });
        assert_eq!(recent.position(&first), Some(1));
        assert_eq!(recent.0.len(), RECENT);
    }
}
