// SPDX-License-Identifier: EUPL-1.2
//! Where a server's icon is found on its website: `https://<host>/favicon.ico`,
//! or the icon the home page links to; the parent domain is tried after a
//! subdomain (voice.example.org → example.org). Only HTTPS. The desktop
//! fetches (with short timeouts and a size cap); the pages and bytes are the
//! website's, so everything here takes hostile input. The sandboxed content
//! worker decodes the icon.

/// The host, then its parent domain unless that looks like a public suffix
/// (`co.uk`, `com.au`: a first label of three letters or fewer).
pub fn sites(host: &str) -> Vec<&str> {
    let mut sites = vec![host];
    if let Some((_, parent)) = host.split_once('.')
        && parent.contains('.')
        && parent
            .split('.')
            .next()
            .is_some_and(|label| label.len() > 3)
    {
        sites.push(parent);
    }
    sites
}

/// PNG, JPEG, GIF, ICO or BMP, judged by the first bytes (servers often
/// answer a missing favicon with an HTML page).
pub fn is_image(bytes: &[u8]) -> bool {
    [
        &b"\x89PNG"[..],
        b"\xff\xd8\xff",
        b"GIF8",
        b"\0\0\x01\0",
        b"BM",
    ]
    .iter()
    .any(|magic| bytes.starts_with(magic))
}

/// The best icon a page links to: an Apple touch icon (large PNG), else any
/// other `rel="…icon…"` link; SVG is skipped (not decoded).
pub fn icon_link(page: &str) -> Option<String> {
    let lower = page.to_ascii_lowercase();
    let mut best: Option<(bool, String)> = None;
    for (start, _) in lower.match_indices("<link") {
        let end = lower[start..]
            .find('>')
            .map_or(lower.len(), |end| start + end);
        let (tag, original) = (&lower[start..end], &page[start..end]);
        let (Some(rel), Some(href)) =
            (attribute(tag, tag, "rel"), attribute(tag, original, "href"))
        else {
            continue;
        };
        if !rel.split_whitespace().any(|word| word.contains("icon"))
            || href.to_ascii_lowercase().contains(".svg")
        {
            continue;
        }
        let touch = rel.contains("apple-touch-icon");
        if best
            .as_ref()
            .is_none_or(|(best_touch, _)| touch && !best_touch)
        {
            best = Some((touch, href.replace("&amp;", "&")));
        }
    }
    best.map(|(_, href)| href)
}

/// An attribute's value in a tag, found in `lower` (the tag lowercased) and
/// cut from `original` (same byte offsets: lowercasing is ASCII-only).
fn attribute<'a>(lower: &str, original: &'a str, name: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(at) = lower[from..].find(name) {
        let at = from + at;
        from = at + name.len();
        let before = lower[..at].chars().next_back();
        let rest = lower[from..].trim_start();
        if !before.is_some_and(char::is_whitespace) || !rest.starts_with('=') {
            continue;
        }
        let value_start = lower.len() - rest[1..].trim_start().len();
        let value = &original[value_start..];
        return match value.chars().next()? {
            quote @ ('"' | '\'') => value[1..].split(quote).next(),
            _ => value.split(|c: char| c.is_whitespace() || c == '>').next(),
        };
    }
    None
}

/// An icon reference resolved against the site, HTTPS only.
pub fn resolve(site: &str, href: &str) -> Option<String> {
    let href = href.trim();
    if href.chars().any(char::is_control) {
        return None;
    }
    Some(if href.starts_with("https://") {
        href.to_owned()
    } else if let Some(rest) = href.strip_prefix("//") {
        format!("https://{rest}")
    } else if href.contains("://") || href.starts_with("data:") {
        return None;
    } else if href.starts_with('/') {
        format!("https://{site}{href}")
    } else {
        format!("https://{site}/{href}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_name_their_icon() {
        let page = r#"<html><head><LINK rel="stylesheet" href="a.css">
            <link rel="shortcut icon" href="/static/fav.png?v=1&amp;x=2">
            <link href='https://cdn.example.org/touch.png' rel='apple-touch-icon'>
            <link rel=icon href=/logo.svg></head>"#;
        assert_eq!(
            icon_link(page).as_deref(),
            Some("https://cdn.example.org/touch.png")
        );
        assert_eq!(
            icon_link(r#"<link rel="icon" href="/static/fav.png?v=1&amp;x=2">"#).as_deref(),
            Some("/static/fav.png?v=1&x=2")
        );
        assert_eq!(icon_link(r#"<link rel="icon" href="/logo.svg">"#), None);
        assert_eq!(icon_link("<link"), None);
    }

    #[test]
    fn references_resolve_to_https_only() {
        assert_eq!(
            resolve("example.org", "/a.png").as_deref(),
            Some("https://example.org/a.png")
        );
        assert_eq!(
            resolve("example.org", "a.png").as_deref(),
            Some("https://example.org/a.png")
        );
        assert_eq!(
            resolve("example.org", "//cdn.example/a.png").as_deref(),
            Some("https://cdn.example/a.png")
        );
        assert_eq!(resolve("example.org", "http://example.org/a.png"), None);
        assert_eq!(
            resolve("example.org", "javascript:x"),
            Some("https://example.org/javascript:x".into())
        );
        assert_eq!(resolve("example.org", "data:image/png;base64,AA"), None);
    }

    #[test]
    fn subdomains_also_try_their_parent() {
        assert_eq!(
            sites("voice.example.org"),
            ["voice.example.org", "example.org"]
        );
        assert_eq!(sites("example.co.uk"), ["example.co.uk"]);
        assert_eq!(sites("example.org"), ["example.org"]);
        assert!(is_image(b"\x89PNG\r\n") && !is_image(b"<!doctype html>"));
    }
}
