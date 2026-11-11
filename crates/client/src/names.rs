// SPDX-License-Identifier: EUPL-1.2
//! How people are named. A local nickname, kept by certificate hash as in
//! Mumble, replaces the name someone has on the server, only for you; their
//! real name stays available beside it. Friends, also kept by certificate
//! hash, are told apart from everyone else.
use crate::model::{Activity, User};
use std::collections::BTreeMap;

/// Nicknames and friends by certificate hash: the client's own
/// [`Preferences`], or a platform's settings.
pub trait Names {
    /// Your nickname for whoever holds this certificate.
    fn nickname(&self, hash: Option<&str>) -> Option<&str>;
    /// The name a friend had when added.
    fn friend(&self, hash: Option<&str>) -> Option<&str>;
}

/// What the client needs of the user's preferences; a platform sets them
/// from its own settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Preferences {
    pub nicknames: BTreeMap<String, String>,
    pub friends: BTreeMap<String, String>,
    /// Notifications for direct messages, your voice channel and mentions.
    pub notifications: bool,
    /// A notification when a friend connects.
    pub friend_notifications: bool,
    /// Tell others in a conversation that you are typing.
    pub send_typing: bool,
    /// Voice is sent while a key is held (a server's suggestion of
    /// push-to-talk is then moot).
    pub push_to_talk: bool,
}

impl Names for Preferences {
    fn nickname(&self, hash: Option<&str>) -> Option<&str> {
        hash.and_then(|hash| self.nicknames.get(hash))
            .map(String::as_str)
    }
    fn friend(&self, hash: Option<&str>) -> Option<&str> {
        hash.and_then(|hash| self.friends.get(hash))
            .map(String::as_str)
    }
}

/// The name to show for a user: your nickname for them, else their own.
pub fn display_name<'a>(user: &'a User, names: &'a (impl Names + ?Sized)) -> &'a str {
    names
        .nickname(user.certificate_hash.as_deref())
        .unwrap_or(&user.name)
}

/// "Nickname (real name)" while a nickname is shown, else the name: for
/// tooltips and screen readers.
pub fn full_name(user: &User, names: &(impl Names + ?Sized)) -> String {
    match names.nickname(user.certificate_hash.as_deref()) {
        Some(nickname) => format!("{nickname} ({})", user.name),
        None => user.name.clone(),
    }
}

/// Who wrote a message under `name`, if that is someone online now (or the
/// other side of a direct conversation). History keeps only the author's
/// name, so messages from anyone else keep the name they were sent under.
pub fn author<'a>(
    name: &str,
    users: impl IntoIterator<Item = &'a User>,
    peer: Option<&'a User>,
) -> Option<&'a User> {
    users.into_iter().chain(peer).find(|user| user.name == name)
}

/// A message author's shown name (see [`author`]).
pub fn author_name<'a>(
    name: &'a str,
    author: Option<&'a User>,
    names: &'a (impl Names + ?Sized),
) -> &'a str {
    author.map_or(name, |user| display_name(user, names))
}

/// An activity line with your nicknames in place of the names it gives.
pub fn activity_text(activity: &Activity, names: &(impl Names + ?Sized)) -> String {
    let text = &activity.text;
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for span in &activity.names {
        if let Some(nickname) = names.nickname(span.certificate_hash.as_deref())
            && let Some(before) = text.get(at..span.start)
            && text.get(span.start..span.end).is_some()
        {
            out.push_str(before);
            out.push_str(nickname);
            at = span.end;
        }
    }
    out.push_str(&text[at..]);
    out
}

/// Whether an activity line is about a friend.
pub fn about_friend(activity: &Activity, names: &(impl Names + ?Sized)) -> bool {
    activity
        .names
        .first()
        .is_some_and(|name| names.friend(name.certificate_hash.as_deref()).is_some())
}

/// Whether `body` mentions `name` as a whole word (case-insensitive).
pub fn mentions(body: &str, name: &str) -> bool {
    let name = name.trim().to_lowercase();
    if name.is_empty() {
        return false;
    }
    let body = body.to_lowercase();
    body.match_indices(&name).any(|(start, _)| {
        let before = body[..start].chars().next_back();
        let after = body[start + name.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NameSpan;

    fn user(name: &str, hash: Option<&str>) -> User {
        User {
            name: name.into(),
            certificate_hash: hash.map(Into::into),
            ..Default::default()
        }
    }

    #[test]
    fn nicknames_replace_names_and_keep_the_real_one_nearby() {
        let mut names = Preferences::default();
        let ada = user("Ada", Some("aa"));
        let guest = user("Guest", None);
        assert_eq!(display_name(&ada, &names), "Ada");
        names.nicknames.insert("aa".into(), "Countess".into());
        assert_eq!(display_name(&ada, &names), "Countess");
        assert_eq!(full_name(&ada, &names), "Countess (Ada)");
        assert_eq!(display_name(&guest, &names), "Guest");
    }

    #[test]
    fn activity_lines_use_nicknames_and_know_friends() {
        let mut names = Preferences::default();
        names.nicknames.insert("aa".into(), "Countess".into());
        names.friends.insert("mm".into(), "Mod".into());
        let span = |start, end, hash: &str| NameSpan {
            start,
            end,
            certificate_hash: Some(hash.into()),
        };
        let mut line = Activity {
            text: "Ada was muted by Mod".into(),
            names: vec![span(0, 3, "aa"), span(17, 20, "mm")],
            ..Default::default()
        };
        assert_eq!(activity_text(&line, &names), "Countess was muted by Mod");
        assert!(!about_friend(&line, &names));
        line.names.reverse();
        assert!(about_friend(&line, &names));
    }

    #[test]
    fn mentions_are_whole_words() {
        assert!(mentions("hi Andi!", "andi"));
        assert!(!mentions("Andino", "andi"));
        assert!(!mentions("anything", " "));
    }
}
