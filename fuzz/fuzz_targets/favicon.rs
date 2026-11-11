// SPDX-License-Identifier: EUPL-1.2
//! A hostile website's home page through the server icon lookup
//! (`nuschl_client::favicon`): the first line is the server's host, the rest
//! the page. Nothing may panic; the chosen link is never SVG, and what is
//! fetched is HTTPS, without control characters, on the site unless the page
//! names an absolute or scheme-relative URL.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuschl_client::favicon::{icon_link, is_image, resolve, sites};

fuzz_target!(|data: &[u8]| {
    let _ = is_image(data);
    let text = String::from_utf8_lossy(data);
    let (host, page) = text.split_once('\n').unwrap_or((&text, ""));
    // Hosts come from bookmarks, which have no control characters.
    if host.contains(char::is_control) {
        return;
    }
    let sites = sites(host);
    assert_eq!(sites[0], host);
    assert!(sites.len() <= 2 && sites.iter().all(|site| host.ends_with(site)));
    let Some(href) = icon_link(page) else {
        return;
    };
    assert!(!href.to_ascii_lowercase().contains(".svg"), "{href:?}");
    for site in sites {
        let Some(url) = resolve(site, &href) else {
            continue;
        };
        assert!(url.starts_with("https://"), "{url:?}");
        assert!(!url.contains(char::is_control), "{url:?}");
        let href = href.trim();
        if !href.starts_with("https://") && !href.starts_with("//") {
            assert!(url.starts_with(&format!("https://{site}")), "{url:?}");
        }
    }
});
