// SPDX-License-Identifier: EUPL-1.2
//! Key transparency on the wire and over the crypto worker's IPC, from hostile
//! peers: the `kt/1` messages (docs/extension.md) with their bounds and the
//! server's check of a publication; the crypto worker's requests and responses
//! (JSON, base64) with their bounds; and the same bytes as `kt/1` frames from
//! Nuschel connections into the server state, which authorises and queues them.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuschel_ipc::crypto::{CryptoRequest, CryptoResponse};
use nuschel_protocol::nuschel::{self, v1};
use prost::Message;

fuzz_target!(|data: &[u8]| {
    let _ = v1::KtStatus::decode(data);
    let _ = v1::KtStatusReply::decode(data);
    let _ = v1::KtPublished::decode(data);
    let _ = v1::KtLookup::decode(data);
    let _ = v1::KtHistory::decode(data);
    let _ = v1::KtHistoryReply::decode(data);
    let _ = v1::KtAudit::decode(data);
    let _ = v1::KtAuditReply::decode(data);
    let _ = v1::KtHead::decode(data);
    let _ = v1::KtHeadReply::decode(data);
    if let Ok(record) = v1::KtRecord::decode(data) {
        let _ = nuschel::valid_kt_record(&record);
    }
    if let Ok(reply) = v1::KtLookupReply::decode(data) {
        let _ = reply.record.as_ref().map(nuschel::valid_kt_record);
    }
    if let Ok(publish) = v1::KtPublish::decode(data) {
        let _ = nuscheld::kt::check_publish(nuschel_kt::LogId([7; 32]), &[1; 16], &publish);
    }
    if let Ok(hello) = nuschel::hello(data) {
        let _ = hello.map(|hello| nuschel::valid_kt_log(&hello));
    }
    if let Ok(request) = serde_json::from_slice::<CryptoRequest>(data) {
        let _ = request.check();
    }
    let _ = serde_json::from_slice::<CryptoResponse>(data);
    // The same bytes as frames (sender, type, length), every type mapped into
    // 0x4E14..=0x4E1F.
    let mut frames = data.to_vec();
    let mut at = 0;
    while at + 3 <= frames.len() {
        frames[at + 1] = 32 + 0x14 + frames[at + 1] % 12;
        at += 3 + usize::from(frames[at + 2]);
    }
    nuscheld::fuzzing::Harness::new().run(&frames);
});
