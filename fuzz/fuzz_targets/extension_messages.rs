// SPDX-License-Identifier: EUPL-1.2
//! The Nuschl extension's messages from hostile peers (docs/extension.md §2):
//! every decoder and bound check over arbitrary bytes, and the same bytes as
//! extension frames from Nuschl connections into the server state (the
//! harness checks the tree, memberships, request bounds and that nobody keeps
//! a subscription it may not read).
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuschl_protocol::nuschl::{self, v1};
use prost::Message;

fuzz_target!(|data: &[u8]| {
    let _ = nuschl::hello(data);
    let _ = nuschl::channel_info(data);
    let _ = v1::ChatSend::decode(data);
    let _ = v1::ChatEdit::decode(data);
    let _ = v1::ChatDelete::decode(data);
    let _ = v1::HistoryRequest::decode(data);
    let _ = v1::Subscribe::decode(data);
    let _ = v1::Unsubscribe::decode(data);
    let _ = v1::ReadMarker::decode(data);
    let _ = v1::Typing::decode(data);
    let _ = v1::OpenDirect::decode(data);
    let _ = v1::ChannelSettings::decode(data);
    let _ = v1::Error::decode(data);
    let _ = v1::Subscribed::decode(data);
    let _ = v1::DirectList::decode(data);
    if let Ok(event) = v1::ChatEvent::decode(data) {
        let _ = event.message.as_ref().map(nuschl::valid_message);
    }
    if let Ok(page) = v1::HistoryPage::decode(data) {
        let _ = page.messages.iter().all(nuschl::valid_message);
    }
    // The same bytes as frames, every type mapped into the extension block.
    let mut frames = data.to_vec();
    let mut at = 0;
    while at + 3 <= frames.len() {
        frames[at + 1] = 32 | (frames[at + 1] & 0x1f);
        at += 3 + usize::from(frames[at + 2]);
    }
    nuschld::fuzzing::Harness::new().run(&frames);
});
