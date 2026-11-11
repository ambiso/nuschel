// SPDX-License-Identifier: EUPL-1.2
//! File offers in a conversation, Mumble compatibility mode: a message that
//! stock Mumble users read and act on with any magic-wormhole client, and that
//! Nuschel recognises from its visible text alone.
//!
//! ```text
//! 📎 report.pdf (2.1 MB) — receive with magic-wormhole: wormhole receive 7-guitarist-revenge
//! 📎 report.pdf (2.1 MB) — no longer available.
//! ```
//!
//! Only visible text survives everywhere such a message goes: servers with
//! HTML disabled strip all markup, stock Mumble drops links of unknown schemes,
//! and Nuschel itself keeps legacy HTML only as inert text. So the text is the
//! machine-readable form, and nothing is hidden in it. An offer is identified
//! by its sender, file name and size: a newer message with all three the same
//! replaces an older one (a fresh code after a pickup, or the end of the offer).
//!
//! [`parse_offer`] reads untrusted message text: it never panics, allocates
//! only what it returns, and accepts only the exact shape [`format_offer`] writes.

/// magic-wormhole's public servers, the defaults of every wormhole client.
pub const PUBLIC_RENDEZVOUS: &str = "ws://relay.magic-wormhole.io:4000/v1";
pub const PUBLIC_RELAY: &str = "tcp://transit.magic-wormhole.io:4001";

/// Longer messages are never offers.
pub const MAX_MESSAGE: usize = 2048;
/// File names are cut to this many characters.
pub const MAX_NAME: usize = 200;
const MAX_CODE: usize = 64;
const MAX_URL: usize = 256;

const MARK: &str = "📎 ";
const ENDED: &str = " — no longer available.";
const SEPARATOR: &str = " — ";
const HOME: &str = "https://magic-wormhole.readthedocs.io/";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    /// Shown as offered (see [`display_name`]); never a path.
    pub name: String,
    /// Bytes; approximate once read back from a message's size label.
    pub size: u64,
    /// The magic-wormhole receive code, or `None` once the offer has ended.
    pub code: Option<String>,
    /// The rendezvous server, when it is not magic-wormhole's public one.
    pub rendezvous: Option<String>,
}

impl Offer {
    /// Whether two offers are of the same file (by name and size label).
    pub fn same_file(&self, other: &Offer) -> bool {
        self.name == other.name && format_size(self.size) == format_size(other.size)
    }
}

/// An offer as a message: rich text for servers that allow HTML, plain text
/// for the others. Both read the same once Nuschel has made the HTML inert.
pub struct Formatted {
    pub text: String,
    pub html: String,
}

pub fn format_offer(offer: &Offer) -> Formatted {
    let name = display_name(&offer.name);
    let size = format_size(offer.size);
    let Some(code) = &offer.code else {
        return Formatted {
            text: format!("{MARK}{name} ({size}){ENDED}"),
            html: format!("{MARK}<b>{}</b> ({size}){ENDED}", escape(&name)),
        };
    };
    let command = match &offer.rendezvous {
        Some(url) => format!("wormhole --relay-url={url} receive {code}"),
        None => format!("wormhole receive {code}"),
    };
    Formatted {
        text: format!("{MARK}{name} ({size}){SEPARATOR}receive with magic-wormhole: {command}"),
        html: format!(
            "{MARK}<b>{}</b> ({size}){SEPARATOR}receive with <a href=\"{HOME}\">magic-wormhole</a>: <code>{}</code>",
            escape(&name),
            escape(&command)
        ),
    }
}

/// The offer a message makes, if it is one.
pub fn parse_offer(text: &str) -> Option<Offer> {
    // The content worker's formatting markers never belong to an offer's text:
    // such a source is read from its rendered document instead (`offer_in`).
    if text.len() > MAX_MESSAGE || text.contains(crate::EMBED) {
        return None;
    }
    let rest = text.trim().strip_prefix(MARK)?;
    if rest.contains(['\n', '\r']) {
        return None;
    }
    if let Some(head) = rest.strip_suffix(ENDED) {
        let (name, size) = name_and_size(head)?;
        return Some(Offer {
            name,
            size,
            code: None,
            rendezvous: None,
        });
    }
    let (head, instructions) = rest.rsplit_once(SEPARATOR)?;
    let (name, size) = name_and_size(head)?;
    // The command ends the message; the words before it are only for people.
    let mut words = instructions.split_whitespace().rev();
    let code = words.next()?;
    if words.next()? != "receive" {
        return None;
    }
    let rendezvous = match words.next()? {
        "wormhole" => None,
        option => {
            let url = option
                .strip_prefix("--relay-url=")
                .filter(|url| valid_url(url))?;
            if words.next()? != "wormhole" {
                return None;
            }
            Some(url.to_owned())
        }
    };
    if !valid_code(code) {
        return None;
    }
    Some(Offer {
        name,
        size,
        code: Some(code.to_owned()),
        rendezvous,
    })
}

/// The offer a rendered message makes: its visible text, when the message is
/// exactly one paragraph. HTML offers (as Nuschel sends them where servers
/// allow HTML) reach the client as the content worker's document, never as
/// HTML, and their raw source carries the worker's formatting markers.
pub fn offer_in(document: &crate::MessageDocument) -> Option<Offer> {
    match document.blocks.as_slice() {
        [crate::ContentBlock::Text { text, .. }] => parse_offer(text),
        _ => None,
    }
}

/// "report.pdf (2.1 MB)": the last parenthesis holds the size.
fn name_and_size(head: &str) -> Option<(String, u64)> {
    let (name, size) = head.strip_suffix(')')?.rsplit_once(" (")?;
    let size = parse_size(size)?;
    (!name.is_empty() && display_name(name) == name).then(|| (name.to_owned(), size))
}

/// A wormhole code as clients generate them: a numeric nameplate and words.
pub fn valid_code(code: &str) -> bool {
    let Some((nameplate, words)) = code.split_once('-') else {
        return false;
    };
    code.len() <= MAX_CODE
        && (1..=8).contains(&nameplate.len())
        && nameplate.bytes().all(|byte| byte.is_ascii_digit())
        && words.split('-').all(|word| {
            !word.is_empty()
                && word
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

/// A rendezvous server address: a WebSocket URL of plain URL characters.
pub fn valid_url(url: &str) -> bool {
    (url.starts_with("ws://") || url.starts_with("wss://"))
        && url.len() <= MAX_URL
        && url
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._~:/?#[]@!$&()*+,;=%".contains(&byte))
}

/// Sizes as people read them, in decimal units: "512 bytes", "2.1 MB", "48 GB".
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["kB", "MB", "GB", "TB", "PB"];
    match bytes {
        1 => return "1 byte".into(),
        0..1000 => return format!("{bytes} bytes"),
        _ => {}
    }
    let mut value = bytes as f64 / 1000.;
    let mut unit = 0;
    while value >= 999.5 && unit + 1 < UNITS.len() {
        value /= 1000.;
        unit += 1;
    }
    if value < 9.95 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// A [`format_size`] label back as bytes (the label's precision, not the file's).
pub fn parse_size(label: &str) -> Option<u64> {
    let (number, unit) = label.split_once(' ')?;
    let scale: u64 = match unit {
        "byte" | "bytes" => 1,
        "kB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "TB" => 1_000_000_000_000,
        "PB" => 1_000_000_000_000_000,
        _ => return None,
    };
    let (whole, tenths) = match number.split_once('.') {
        Some((whole, tenth)) if scale > 1 && tenth.len() == 1 => (whole, tenth),
        Some(_) => return None,
        None => (number, "0"),
    };
    if whole.is_empty() || whole.len() > 4 || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let tenths: u64 = tenths.parse().ok().filter(|_| tenths.len() == 1)?;
    let whole: u64 = whole.parse().ok()?;
    Some(whole * scale + tenths * (scale / 10))
}

/// The most a file offered as `size` (a parsed label) can be: labels round to
/// two significant digits, so within 5 %.
pub fn size_limit(size: u64) -> u64 {
    size.saturating_add(size / 16).saturating_add(1024)
}

/// A file name as Nuschel shows and offers it: one line without control,
/// invisible or text-direction characters, at most [`MAX_NAME`] characters.
pub fn display_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|&c| !c.is_control() && !invisible(c))
        .take(MAX_NAME)
        .collect();
    match cleaned.trim() {
        "" => "file".into(),
        trimmed => trimmed.into(),
    }
}

/// A name for the save dialog to suggest: also without path separators,
/// characters Windows forbids, or leading dots. The user chooses the path.
pub fn save_name(name: &str) -> String {
    let name = display_name(name).replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_");
    match name.trim_matches(['.', ' ']) {
        "" => "file".into(),
        trimmed => trimmed.into(),
    }
}

fn invisible(c: char) -> bool {
    matches!(
        c,
        '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}'
    )
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_offers_are_offers_and_only_as_one_paragraph() {
        let text = format_offer(&offer("report.pdf", 2_100_000, Some("7-guitarist-revenge"))).text;
        let one = crate::MessageDocument {
            blocks: vec![crate::ContentBlock::text(text.clone())],
        };
        assert_eq!(
            offer_in(&one),
            Some(offer("report.pdf", 2_100_000, Some("7-guitarist-revenge")))
        );
        let two = crate::MessageDocument {
            blocks: vec![
                crate::ContentBlock::text(text),
                crate::ContentBlock::text("more"),
            ],
        };
        assert_eq!(offer_in(&two), None);
        assert_eq!(
            offer_in(&crate::MessageDocument { blocks: Vec::new() }),
            None
        );
        // The worker's prepared source of an HTML offer keeps its markers.
        assert_eq!(
            parse_offer(
                "📎 \u{FFFC}b\u{FFFC}a.txt\u{FFFC}/b\u{FFFC} (3 bytes) — no longer available."
            ),
            None
        );
    }

    fn offer(name: &str, size: u64, code: Option<&str>) -> Offer {
        Offer {
            name: name.into(),
            size,
            code: code.map(Into::into),
            rendezvous: None,
        }
    }

    #[test]
    fn messages_read_as_the_design_says() {
        let message = format_offer(&offer("report.pdf", 2_100_000, Some("7-guitarist-revenge")));
        assert_eq!(
            message.text,
            "📎 report.pdf (2.1 MB) — receive with magic-wormhole: wormhole receive 7-guitarist-revenge"
        );
        assert_eq!(
            message.html,
            "📎 <b>report.pdf</b> (2.1 MB) — receive with <a href=\"https://magic-wormhole.readthedocs.io/\">magic-wormhole</a>: <code>wormhole receive 7-guitarist-revenge</code>"
        );
        let ended = format_offer(&offer("report.pdf", 2_100_000, None));
        assert_eq!(ended.text, "📎 report.pdf (2.1 MB) — no longer available.");
    }

    #[test]
    fn offers_round_trip() {
        for (name, size, code, rendezvous) in [
            ("report.pdf", 2_100_000, Some("7-guitarist-revenge"), None),
            ("a (1) — b.tar.gz", 512, Some("12345-a-b-c"), None),
            ("x", 1, None, None),
            (
                "Ünïcode 🎉.png",
                48_000_000_000,
                Some("3-x"),
                Some("ws://127.0.0.1:4000/v1"),
            ),
        ] {
            let original = Offer {
                rendezvous: rendezvous.map(Into::into),
                ..offer(name, size, code)
            };
            let parsed = parse_offer(&format_offer(&original).text).unwrap();
            assert!(parsed.same_file(&original), "{parsed:?}");
            assert_eq!(parsed.code, original.code);
            assert_eq!(parsed.rendezvous, original.rendezvous);
            assert_eq!(parsed.name, name);
        }
        // Text around the command may change (e.g. how a link is kept).
        assert!(
            parse_offer("📎 a.txt (3 bytes) — receive with magic-wormhole (https://x/): wormhole receive 2-b")
                .is_some()
        );
    }

    #[test]
    fn sizes_round_trip_through_their_labels() {
        let mut bytes = 1_u64;
        while bytes < u64::MAX / 3 {
            let label = format_size(bytes);
            let parsed = parse_size(&label).unwrap();
            assert_eq!(format_size(parsed), label, "{bytes}");
            assert!(bytes <= size_limit(parsed), "{bytes} {label}");
            bytes = bytes * 3 + 7;
        }
        assert_eq!(format_size(0), "0 bytes");
        assert_eq!(format_size(999_999), "1.0 MB");
        assert_eq!(format_size(9_960), "10 kB");
        for bad in [
            "",
            "1",
            "1.5 bytes",
            "1.25 MB",
            "12345 kB",
            "-1 MB",
            "1 mb",
            ".5 MB",
            "1. MB",
        ] {
            assert_eq!(parse_size(bad), None, "{bad}");
        }
    }

    #[test]
    fn hostile_messages_are_not_offers() {
        for text in [
            "📎 report.pdf (2.1 MB)",
            "📎 (2.1 MB) — no longer available.",
            "📎 a (2 MB) — wormhole receive 7-x; rm -rf ~",
            "📎 a (2 MB) — wormhole receive 7-X",
            "📎 a (2 MB) — wormhole receive x-y",
            "📎 a (2 MB) — wormhole receive 123456789-y",
            "📎 a (2 MB) — wormhole receive 7-",
            "📎 a (2 MB) — wormhole --relay-url=http://x receive 7-y",
            "📎 a (2 MB) — curl --relay-url=ws://x receive 7-y",
            "📎 a\u{202E}fdp.exe (2 MB) — wormhole receive 7-y",
            "📎 line\nbreak (2 MB) — wormhole receive 7-y",
            "receive with magic-wormhole: wormhole receive 7-y",
        ] {
            assert_eq!(parse_offer(text), None, "{text}");
        }
        let long = format!(
            "📎 {} (2 MB) — wormhole receive 7-y",
            "a".repeat(MAX_NAME + 1)
        );
        assert_eq!(parse_offer(&long), None);
        assert_eq!(parse_offer(&"📎 ".repeat(MAX_MESSAGE)), None);
        // Random bytes around real fragments never panic.
        let pieces = [
            "📎 ",
            " — ",
            " (",
            ")",
            "2.1 MB",
            "wormhole",
            " receive ",
            "7-a",
            "é",
            "\n",
            "--relay-url=ws://x",
        ];
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for _ in 0..20_000 {
            let mut text = String::new();
            for _ in 0..(state % 12) {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                text.push_str(pieces[(state % pieces.len() as u64) as usize]);
            }
            if let Some(offer) = parse_offer(&text) {
                assert!(offer.code.as_deref().is_none_or(valid_code));
            }
        }
    }

    #[test]
    fn names_are_one_safe_line() {
        assert_eq!(display_name("a\u{202E}gpj.exe\n"), "agpj.exe");
        assert_eq!(display_name("  \u{200B} "), "file");
        assert_eq!(save_name("../../.bashrc"), "_.._.bashrc");
        assert_eq!(save_name("..."), "file");
        assert_eq!(save_name("C:\\x/y?.txt"), "C__x_y_.txt");
        assert_eq!(display_name(&"é".repeat(500)).chars().count(), MAX_NAME);
    }
}
