// SPDX-License-Identifier: EUPL-1.2
//! `mumble://` links and share links (`https://…/join/…`, `nuschel://join/…`)
//! from hostile messages and invites, as the client parses them before
//! connecting (`nuschel_session::config::MumbleUrl`): nothing may panic, and
//! what parses passes the bookmark checks, has a bounded channel path without
//! control characters, a well-formed pin and token, and a share link writes
//! itself again as what it read.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuschel_session::config::{MumbleUrl, ServerBookmark};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(url) = MumbleUrl::parse(text) else {
        return;
    };
    let username = url.username.as_deref().unwrap_or("user");
    assert!(ServerBookmark::new("", &url.host, url.port, username).is_ok());
    assert!(!url.host.contains(char::is_control) && url.host == url.host.to_ascii_lowercase());
    assert!(url.channel.len() <= 64);
    for name in &url.channel {
        assert!(!name.is_empty() && name.len() <= 512 && !name.contains(char::is_control));
    }
    assert!(
        url.password
            .as_deref()
            .is_none_or(|p| !p.contains(char::is_control))
    );
    if let Some(token) = &url.share_token {
        assert!(nuschel_core::share::valid_token(token));
    }
    if let Ok(link) = nuschel_core::share::ShareLink::parse(text) {
        let again = nuschel_core::share::ShareLink::parse(&link.app_url()).expect("written links parse");
        assert_eq!(again.token, link.token);
        assert_eq!((again.host.as_str(), again.port), (link.host.as_str(), link.port));
        assert_eq!(again.certificate_sha256, link.certificate_sha256);
    }
    if let Some(pin) = &url.certificate_sha256 {
        assert!(pin.len() == 64 && pin.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
    }
});
