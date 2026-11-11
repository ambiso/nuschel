// SPDX-License-Identifier: EUPL-1.2
//! Desktop notifications for messages that arrive while Nuschel is in the
//! background (direct messages, your voice channel, mentions of your name).
//! Linux uses the freedesktop notification service over D-Bus, with an
//! image's thumbnail attached; other systems are not implemented yet.

/// One notification. Message text is untrusted: the body is escaped, since
/// notification servers may interpret markup there; the summary is plain text
/// by specification and only loses control characters.
pub struct Note {
    /// Replaces the previous notification with the same key (one per conversation).
    pub key: String,
    pub summary: String,
    pub body: String,
    /// RGBA8 thumbnail.
    pub image: Option<(u32, u32, Vec<u8>)>,
}

/// Largest thumbnail side, as for images in the timeline.
const MAX_THUMBNAIL_SIDE: u32 = 4096;

/// A message image's RGBA pixels for a notification, if its size is plausible
/// and matches the pixel data. The size is computed in `usize`, so a hostile
/// width and height cannot wrap around to match a short buffer.
pub fn thumbnail(width: u32, height: u32, rgba: &str) -> Option<(u32, u32, Vec<u8>)> {
    use base64::Engine as _;
    let sides = 1..=MAX_THUMBNAIL_SIDE;
    if !sides.contains(&width) || !sides.contains(&height) {
        return None;
    }
    let pixels = base64::engine::general_purpose::STANDARD
        .decode(rgba)
        .ok()?;
    (pixels.len() == width as usize * height as usize * 4).then_some((width, height, pixels))
}

/// Escape the characters notification markup gives meaning to.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if c.is_control() && c != '\n' => {}
            c => out.push(c),
        }
    }
    out
}

/// Plain text for the summary: no control characters or line breaks.
pub fn plain(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .chars()
        .take(120)
        .collect()
}

/// A short, single-paragraph preview of a message body.
pub fn preview(body: &str) -> String {
    let flat: String = body
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| *c != '\u{1e}')
        .collect();
    if flat.chars().count() > 160 {
        format!("{}…", flat.chars().take(160).collect::<String>())
    } else {
        flat
    }
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

/// Start the notification sender; notes go to it through the returned channel.
#[cfg(target_os = "linux")]
pub fn start(handle: &tokio::runtime::Handle) -> tokio::sync::mpsc::Sender<Note> {
    let (sender, mut notes) = tokio::sync::mpsc::channel::<Note>(16);
    handle.spawn(async move {
        let connection = match zbus::Connection::session().await {
            Ok(connection) => connection,
            Err(error) => {
                tracing::warn!(%error, "no session bus: desktop notifications are off");
                return;
            }
        };
        let Ok(proxy) = zbus::Proxy::new(
            &connection,
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
        )
        .await
        else {
            return;
        };
        let mut replaces = std::collections::HashMap::<String, u32>::new();
        while let Some(note) = notes.recv().await {
            use zbus::zvariant::Value;
            let mut hints = std::collections::HashMap::<&str, Value>::new();
            hints.insert("desktop-entry", Value::from("org.nuschel.Nuschel"));
            hints.insert("category", Value::from("im.received"));
            if let Some((width, height, rgba)) = note.image {
                let image = (
                    width as i32,
                    height as i32,
                    (width * 4) as i32,
                    true,
                    8_i32,
                    4_i32,
                    rgba,
                );
                hints.insert("image-data", Value::from(image));
            }
            let previous = replaces.get(&note.key).copied().unwrap_or(0);
            let reply = proxy
                .call::<_, _, u32>(
                    "Notify",
                    &(
                        "Nuschel",
                        previous,
                        "org.nuschel.Nuschel",
                        plain(&note.summary),
                        escape(&note.body),
                        Vec::<&str>::new(),
                        hints,
                        -1_i32,
                    ),
                )
                .await;
            match reply {
                Ok(id) => {
                    replaces.insert(note.key, id);
                }
                Err(error) => tracing::warn!(%error, "desktop notification failed"),
            }
        }
    });
    sender
}

#[cfg(not(target_os = "linux"))]
pub fn start(_: &tokio::runtime::Handle) -> tokio::sync::mpsc::Sender<Note> {
    // Windows toasts and macOS user notifications need platform bindings; not yet.
    tokio::sync::mpsc::channel::<Note>(1).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untrusted_text_cannot_inject_markup() {
        assert_eq!(
            escape("<img src=\"https://tracker.invalid/x\"> & <b>"),
            "&lt;img src=&quot;https://tracker.invalid/x&quot;&gt; &amp; &lt;b&gt;"
        );
    }

    #[test]
    fn thumbnails_match_their_pixel_data() {
        use base64::Engine as _;
        let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
        let pixels = encode(&[0; 2 * 3 * 4]);
        assert_eq!(
            thumbnail(2, 3, &pixels).map(|(w, h, _)| (w, h)),
            Some((2, 3))
        );
        assert!(thumbnail(3, 3, &pixels).is_none());
        assert!(thumbnail(0, 3, &pixels).is_none());
        // 65536 * 65536 * 4 wraps to 0 in u32; the old check accepted an empty image.
        assert!(thumbnail(65536, 65536, "").is_none());
        assert!(thumbnail(1 << 30, 4, &encode(&[0; 16])).is_none());
        assert!(thumbnail(1, 1, "not base64!").is_none());
    }

    #[test]
    fn summaries_are_one_plain_line() {
        assert_eq!(plain("Eve\nin #general\u{7}"), "Eve in #general");
        assert_eq!(plain("a & <b>"), "a & <b>");
    }

    #[test]
    fn mentions_are_whole_words() {
        assert!(mentions("hey andi, look", "Andi"));
        assert!(mentions("ANDI!", "andi"));
        assert!(!mentions("candidate", "andi"));
        assert!(!mentions("andiamo", "andi"));
        assert!(!mentions("anything", ""));
    }

    #[test]
    fn previews_are_short_and_flat() {
        assert_eq!(preview("one\n\ntwo   three"), "one two three");
        assert!(preview(&"x".repeat(500)).ends_with('…'));
    }
}
