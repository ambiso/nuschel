// SPDX-License-Identifier: EUPL-1.2
//! What stock Mumble clients receive for Nuschl text: escaped, line breaks
//! kept, web links made clickable as Mumble's own chat bar does it. The client
//! uses these on stock servers and the server for its stock users (the copy is
//! always made from the same text, so nobody sees something different).

/// The text stock Mumble clients receive. They print "Name: " before the
/// first line, so a message that opens with a code fence gets a leading line
/// break: the fence and the code then start on lines of their own.
pub fn legacy_body(body: &str, html: bool) -> String {
    let body = if body.starts_with("```") {
        std::borrow::Cow::Owned(format!("\n{body}"))
    } else {
        std::borrow::Cow::Borrowed(body)
    };
    if html {
        text_html(&body)
    } else {
        body.into_owned()
    }
}

/// Plain text as Mumble HTML: escaped, line breaks kept, and web links made
/// clickable the way stock Mumble's chat bar makes them (receivers show the
/// HTML they get; they do not detect links themselves).
pub fn text_html(body: &str) -> String {
    let mut out = String::with_capacity(body.len() + 16);
    let mut done = 0;
    for (start, end) in links(body) {
        out.push_str(&escape_html(&body[done..start]));
        let url = &body[start..end];
        let href = if url[..4].eq_ignore_ascii_case("www.") {
            format!("https://{url}")
        } else {
            url.to_owned()
        };
        out.push_str(&format!(
            "<a href=\"{}\">{}</a>",
            escape_html(&href),
            escape_html(url)
        ));
        done = end;
    }
    out.push_str(&escape_html(&body[done..]));
    out
}

/// The links in `text` as byte ranges, in order: http(s):// or mumble:// URLs
/// and www. addresses at a word start, with URL characters as Mumble matches
/// them, minus trailing punctuation that belongs to the sentence. Linear in
/// the text: the server makes this copy of every message under its lock.
pub fn link_ranges(text: &str) -> Vec<(usize, usize)> {
    links(text)
}

fn links(text: &str) -> Vec<(usize, usize)> {
    const PREFIXES: [&str; 4] = ["https://", "http://", "mumble://", "www."];
    let lower = text.to_ascii_lowercase();
    let find = |prefix: &str, from: usize| lower[from..].find(prefix).map(|at| from + at);
    // The next match of each prefix, searched again only once passed:
    // searching every prefix again from every candidate is quadratic.
    let mut next = PREFIXES.map(|prefix| find(prefix, 0));
    let mut links = Vec::new();
    let mut from = 0;
    loop {
        for (index, prefix) in PREFIXES.iter().enumerate() {
            if next[index].is_some_and(|at| at < from) {
                next[index] = find(prefix, from);
            }
        }
        let Some((start, prefix)) = next
            .iter()
            .zip(PREFIXES)
            .filter_map(|(at, prefix)| at.map(|at| (at, prefix.len())))
            .min()
        else {
            return links;
        };
        from = start + 1;
        let word_start = text[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        if !word_start {
            continue;
        }
        let mut end = start
            + text[start..]
                .find(|c: char| {
                    !(c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(c))
                })
                .unwrap_or(text.len() - start);
        // Brackets are counted once: counting again for every character
        // trimmed is quadratic in the link's length. URL characters are ASCII.
        let link = &text[start..end];
        let mut unclosed = link.matches('(').count() as isize - link.matches(')').count() as isize;
        loop {
            match text.as_bytes()[end - 1] {
                b')' if unclosed < 0 => unclosed += 1,
                b'.' | b',' | b';' | b':' | b'!' | b'?' | b'\'' => {}
                _ => break,
            }
            end -= 1;
        }
        if end > start + prefix {
            links.push((start, end));
            from = end;
        }
    }
}

pub fn escape_html(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    for c in body.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("<br/>"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_are_clickable_for_stock_mumble() {
        assert_eq!(
            text_html("see https://google.com."),
            "see <a href=\"https://google.com\">https://google.com</a>."
        );
        assert_eq!(
            text_html("(www.example.org/a?b=1&c=2)"),
            "(<a href=\"https://www.example.org/a?b=1&amp;c=2\">www.example.org/a?b=1&amp;c=2</a>)"
        );
        assert_eq!(
            text_html("wiki https://en.wikipedia.org/wiki/Rust_(language) ok"),
            "wiki <a href=\"https://en.wikipedia.org/wiki/Rust_(language)\">https://en.wikipedia.org/wiki/Rust_(language)</a> ok"
        );
        // Not at a word start, nothing after the prefix, other schemes: plain text.
        assert_eq!(text_html("xhttps://a.b"), "xhttps://a.b");
        assert_eq!(text_html("https:// and www."), "https:// and www.");
        assert_eq!(text_html("javascript:alert(1)"), "javascript:alert(1)");
        assert_eq!(
            text_html("<b>\"x\"</b>"),
            "&lt;b&gt;&quot;x&quot;&lt;/b&gt;"
        );
    }

    #[test]
    fn hostile_links_take_linear_time() {
        // At the server's 64 KiB, the first took ten seconds and the second
        // one under the server's lock, with every bracket or candidate
        // counted or searched again from the start.
        let size = 64 * 1024;
        for (text, links) in [
            (format!("http://a{}", ")".repeat(size)), 1),
            ("awww.".repeat(size / 5), 0),
            ("ahttps://".repeat(size / 9), 0),
            ("http://a ".repeat(size / 9), size / 9),
        ] {
            let started = std::time::Instant::now();
            let html = text_html(&text);
            assert_eq!(html.matches("<a href=").count(), links);
            assert!(
                started.elapsed() < std::time::Duration::from_secs(2),
                "{:?} for {}",
                started.elapsed(),
                &text[..20]
            );
        }
        assert!(
            text_html(&format!("http://a{}", ")".repeat(9)))
                .starts_with("<a href=\"http://a\">http://a</a>)))")
        );
    }

    #[test]
    fn literal_chat_survives_html_enabled_servers() {
        assert_eq!(
            escape_html("<script>&\n\n```rust\nfn main() {}\n```"),
            "&lt;script&gt;&amp;<br/><br/>```rust<br/>fn main() {}<br/>```"
        );
    }

    #[test]
    fn a_message_opening_with_a_code_fence_starts_on_a_new_line_for_stock_mumble() {
        assert_eq!(
            legacy_body("```rust\nfn main() {}\n```", true),
            "<br/>```rust<br/>fn main() {}<br/>```"
        );
        assert_eq!(legacy_body("```\nx\n```", false), "\n```\nx\n```");
        assert_eq!(
            legacy_body("see:\n```\nx\n```", true),
            "see:<br/>```<br/>x<br/>```"
        );
    }
}
