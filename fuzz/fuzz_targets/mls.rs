// SPDX-License-Identifier: EUPL-1.2
//! MLS input from hostile peers (docs/e2ee.md §6, `mls/1`): nuschl-e2ee's structural
//! checks of key packages, ratchet trees and message framing (which run before
//! OpenMLS: short X-Wing keys made libcrux-kem 0.0.9 panic); the directory's and
//! the client's key package validation; Welcome routing; the §6.4 envelope, whose
//! decoding must re-encode to the same bytes; the `mls/1` messages; and the delivery
//! service's fencing and commit path on a fixture group, which must refuse without
//! changing the group.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuschl_e2ee::{client, ds, envelope, prevalidate};
use nuschl_protocol::nuschl::v1;
use prost::Message;
use std::sync::{Mutex, OnceLock};

/// A group of one device registered with a delivery service, made once.
fn fixture() -> &'static Mutex<ds::DeliveryGroup> {
    static GROUP: OnceLock<Mutex<ds::DeliveryGroup>> = OnceLock::new();
    GROUP.get_or_init(|| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let validity = nuschl_identity::Validity::starting_at(now - 3600, 365 * 86_400)
            .expect("validity");
        let identity = nuschl_identity::NewIdentity::create(
            &nuschl_identity::DisplayName::new("fuzz").expect("name"),
            validity,
            now,
        )
        .expect("identity");
        let device = client::Client::new().expect("client");
        let binding = nuschl_e2ee::ConversationBinding::direct([7; 32], [1; 16]);
        let signer = ds::ServerSigner::generate().expect("server key");
        let created = device
            .create(
                &identity.device,
                &identity.device_credential,
                &binding,
                signer.public_key(),
                &[],
            )
            .expect("group");
        let (group, _) = ds::DeliveryGroup::create(
            &created.init_group_info,
            &created.init_tree,
            &[7; 32],
            &[1; 16],
            signer.public_key(),
        )
        .expect("delivery group");
        Mutex::new(group)
    })
}

fuzz_target!(|data: &[u8]| {
    let _ = prevalidate::key_package(data);
    let _ = prevalidate::ratchet_tree(data);
    let _ = prevalidate::message(data);
    let _ = prevalidate::wire_format(data);
    let _ = nuschl_e2ee::history::archive_key_index(data);
    let _ = nuschl_e2ee::history::decode_grant(data);
    let key = nuschl_e2ee::history::HistoryKey { index: 0, key: [0; 32] };
    let _ = nuschl_e2ee::history::open_archive(&key, &[0; 16], data);
    let _ = nuschl_e2ee::ConversationBinding::decode(data);
    let _ = nuschl_e2ee::binding::parse_group_id(data);
    if let Ok(parsed) = envelope::Envelope::decode(data) {
        assert_eq!(parsed.encode(), data, "envelopes have one encoding");
    }
    if prevalidate::key_package(data).is_ok() {
        let _ = ds::check_key_package(data);
        let _ = client::ValidKeyPackage::check(data);
    }
    let _ = ds::welcome_recipients(data);
    let _ = v1::MlsKeyPackageUpload::decode(data);
    let _ = v1::MlsKeyPackageClaim::decode(data);
    let _ = v1::MlsCommit::decode(data);
    let _ = v1::MlsSend::decode(data);
    let _ = v1::MlsWelcomeFetch::decode(data);
    let _ = v1::MlsWelcomeAck::decode(data);
    let _ = v1::MlsJoinInfoRequest::decode(data);
    let _ = v1::MlsWelcomes::decode(data);
    let _ = v1::MlsJoinInfo::decode(data);
    if let Ok(mut group) = fixture().lock() {
        let epoch = group.epoch();
        let _ = group.fence(data);
        let _ = group.commit(data, data, Some(data));
        let _ = group.proposal(data);
        assert_eq!(group.epoch(), epoch, "garbage never moves the group");
    }
});
