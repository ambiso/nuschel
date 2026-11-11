// SPDX-License-Identifier: EUPL-1.2
//! Message text from hostile peers as a file offer (`nuschl_core::transfer`,
//! the magic-wormhole compatibility mode): nothing may panic, an offer that
//! parses has a valid code and rendezvous URL and a name as Nuschl shows it,
//! and Nuschl's own message for it reads back as the same offer.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuschl_core::transfer::{
    MAX_MESSAGE, display_name, format_offer, format_size, parse_offer, parse_size, save_name,
    valid_code, valid_url,
};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = parse_size(text);
    let save = save_name(text);
    assert!(!save.is_empty() && !save.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|']));
    assert!(!save.starts_with('.') && !save.contains(char::is_control));
    let Some(offer) = parse_offer(text) else {
        return;
    };
    assert!(text.len() <= MAX_MESSAGE);
    assert!(offer.code.as_deref().is_none_or(valid_code));
    assert!(offer.rendezvous.as_deref().is_none_or(valid_url));
    assert_eq!(display_name(&offer.name), offer.name);
    assert!(offer.code.is_some() || offer.rendezvous.is_none());
    // Labels round to their precision, so the size is compared by label.
    let message = format_offer(&offer);
    if let Some(again) = parse_offer(&message.text) {
        assert_eq!(again.name, offer.name);
        assert_eq!(again.code, offer.code);
        assert_eq!(again.rendezvous, offer.rendezvous);
        assert_eq!(format_size(again.size), format_size(offer.size));
    } else {
        // Only a size whose label no longer parses (five digits of petabytes).
        assert!(
            parse_size(&format_size(offer.size)).is_none(),
            "{:?}",
            message.text
        );
    }
});
