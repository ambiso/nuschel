// SPDX-License-Identifier: EUPL-1.2
//! How people are named on screen. A local nickname, kept by certificate hash
//! as in Mumble, replaces the name someone has on the server, only for you;
//! their real name stays in a tooltip and in their profile. Friends, also
//! kept by certificate hash, get a marker.
use nuschel_session::{config::ClientSettings, connection::User, model::Activity};

/// The friend marker's colour.
pub const FRIEND: u32 = 0xee6ea8;

/// The name to show for a user: your nickname for them, else their own.
pub fn display_name<'a>(user: &'a User, settings: &'a ClientSettings) -> &'a str {
    settings
        .nickname(user.certificate_hash.as_deref())
        .unwrap_or(&user.name)
}

/// "Nickname (real name)" while a nickname is shown, else the name: for
/// tooltips and screen readers.
pub fn full_name(user: &User, settings: &ClientSettings) -> String {
    match settings.nickname(user.certificate_hash.as_deref()) {
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
    settings: &'a ClientSettings,
) -> &'a str {
    author.map_or(name, |user| display_name(user, settings))
}

/// An activity line with your nicknames in place of the names it gives.
pub fn activity_text(activity: &Activity, settings: &ClientSettings) -> String {
    let text = &activity.text;
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for span in &activity.names {
        if let Some(nickname) = settings.nickname(span.certificate_hash.as_deref())
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
pub fn about_friend(activity: &Activity, settings: &ClientSettings) -> bool {
    activity
        .names
        .first()
        .is_some_and(|name| settings.friend(name.certificate_hash.as_deref()).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuschel_session::model::NameSpan;

    fn user(name: &str, hash: Option<&str>) -> User {
        User {
            name: name.into(),
            certificate_hash: hash.map(Into::into),
            ..Default::default()
        }
    }

    #[test]
    fn nicknames_replace_names_and_keep_the_real_one_nearby() {
        let mut settings = ClientSettings::default();
        let ada = user("Ada", Some("aa"));
        let guest = user("Guest", None);
        assert_eq!(display_name(&ada, &settings), "Ada");
        assert_eq!(full_name(&ada, &settings), "Ada");
        settings.set_nickname("aa", "Countess");
        assert_eq!(display_name(&ada, &settings), "Countess");
        assert_eq!(full_name(&ada, &settings), "Countess (Ada)");
        // Someone without a certificate cannot have a nickname.
        assert_eq!(display_name(&guest, &settings), "Guest");
    }

    #[test]
    fn message_authors_map_through_people_online_now() {
        let mut settings = ClientSettings::default();
        settings.set_nickname("aa", "Countess");
        settings.set_nickname("pp", "Pal");
        let online = [user("Ada", Some("aa")), user("Grace", None)];
        let peer = user("Peer", Some("pp"));
        let find = |name| author(name, &online, Some(&peer));
        assert_eq!(author_name("Ada", find("Ada"), &settings), "Countess");
        assert_eq!(author_name("Grace", find("Grace"), &settings), "Grace");
        // The other side of a direct conversation, even when offline.
        assert_eq!(author_name("Peer", find("Peer"), &settings), "Pal");
        // Someone who has left keeps the stored name.
        assert_eq!(author_name("Linus", find("Linus"), &settings), "Linus");
    }

    #[test]
    fn activity_lines_use_nicknames_where_they_name_people() {
        let mut settings = ClientSettings::default();
        settings.set_nickname("aa", "Countess");
        settings.add_friend("mm", "Mod");
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
        assert_eq!(activity_text(&line, &settings), "Countess was muted by Mod");
        assert!(!about_friend(&line, &settings));
        line.names.reverse();
        assert!(about_friend(&line, &settings));
        // Spans out of order, out of range or inside a character are skipped.
        line.names = vec![span(17, 20, "aa"), span(0, 3, "aa")];
        assert_eq!(activity_text(&line, &settings), "Ada was muted by Countess");
        line.names = vec![span(18, 99, "aa")];
        assert_eq!(activity_text(&line, &settings), line.text);
        line.text = "Äda left".into();
        line.names = vec![span(1, 3, "aa")];
        assert_eq!(activity_text(&line, &settings), line.text);
    }
}
