// SPDX-License-Identifier: EUPL-1.2
//! Structured values stand in for a caller-authenticated signaling envelope.
//! These tests do not pretend that an SDP fingerprint establishes user identity.
use nuschl_rtc::{Description, Error, MAX_SDP_BYTES, Peer, Purpose, SignalingId};
use std::time::Instant;

const ROLES: &[Purpose] = &[
    Purpose::Microphone,
    Purpose::SystemAudio,
    Purpose::Camera,
    Purpose::ScreenVideo,
];

fn peer() -> Peer {
    Peer::new("127.0.0.1:41100".parse().unwrap(), Instant::now()).unwrap()
}

fn offer() -> Description {
    peer()
        .offer_tracks(ROLES, SignalingId::new([9; 16]).unwrap())
        .unwrap()
}

#[test]
fn descriptions_reject_duplicate_missing_or_cross_kind_roles_and_mids_before_mutation() {
    let offer = offer();
    let mut bad = Vec::new();
    let mut value = offer.clone();
    value.bindings[0].purpose = Purpose::SystemAudio;
    bad.push(value);
    let mut value = offer.clone();
    value.bindings[0].mid = value.bindings[1].mid.clone();
    bad.push(value);
    let mut value = offer.clone();
    value.bindings.swap(0, 2);
    let first = value.bindings[0].purpose;
    value.bindings[0].purpose = value.bindings[2].purpose;
    value.bindings[2].purpose = first;
    bad.push(value);
    let mut value = offer.clone();
    value.bindings.pop();
    bad.push(value);
    let mut value = offer.clone();
    value.bindings[0].mid = "unknown".into();
    bad.push(value);
    let mut value = offer.clone();
    value.bindings[0].mid = "a".repeat(17);
    bad.push(value);
    let mut value = offer.clone();
    value.bindings.push(value.bindings[0].clone());
    bad.push(value);
    let mut receiver = peer();
    for value in bad {
        assert!(matches!(
            receiver.accept_offer_tracks(&value, offer.id),
            Err(Error::Description)
        ));
        assert!(receiver.track_for(Purpose::Microphone).is_none());
    }
    receiver.accept_offer_tracks(&offer, offer.id).unwrap();
}

#[test]
fn stale_call_ids_and_answer_role_swaps_fail_without_consuming_the_pending_offer() {
    assert!(SignalingId::new([0; 16]).is_err());
    let id = SignalingId::new([11; 16]).unwrap();
    let other = SignalingId::new([12; 16]).unwrap();
    let mut initiator = peer();
    let offer = initiator.offer_tracks(ROLES, id).unwrap();
    let mut receiver = peer();
    assert!(matches!(
        receiver.accept_offer_tracks(&offer, other),
        Err(Error::Description)
    ));
    let answer = receiver.accept_offer_tracks(&offer, id).unwrap();
    assert!(matches!(
        initiator.accept_answer_tracks(&answer, other),
        Err(Error::Description)
    ));
    let mut changed = answer.clone();
    // Both are audio, so SDP kind validation alone cannot catch this attack.
    changed.bindings[0].purpose = Purpose::SystemAudio;
    changed.bindings[1].purpose = Purpose::Microphone;
    assert!(matches!(
        initiator.accept_answer_tracks(&changed, id),
        Err(Error::Description)
    ));
    changed = answer.clone();
    changed.bindings[2].purpose = Purpose::ScreenVideo;
    changed.bindings[3].purpose = Purpose::Camera;
    assert!(matches!(
        initiator.accept_answer_tracks(&changed, id),
        Err(Error::Description)
    ));
    assert!(matches!(
        initiator.accept_answer(&answer.sdp),
        Err(Error::SignalingState)
    ));
    initiator.accept_answer_tracks(&answer, id).unwrap();
    assert!(matches!(
        initiator.accept_answer_tracks(&answer, id),
        Err(Error::SignalingState)
    ));
}

#[test]
fn sdp_labels_cannot_reassign_an_authenticated_purpose() {
    let mut offer = offer();
    // Arbitrary attacker-controlled labels must have no semantic authority.
    offer.sdp = offer.sdp.replace(
        "a=sendrecv",
        "a=sendrecv\r\na=msid:screen-system-audio screen-camera",
    );
    let mut receiver = peer();
    let answer = receiver.accept_offer_tracks(&offer, offer.id).unwrap();
    assert_eq!(answer.bindings, offer.bindings);
    for binding in &offer.bindings {
        assert_eq!(
            receiver.track_for(binding.purpose).unwrap().mid(),
            binding.mid
        );
    }
    assert!(!format!("{offer:?}").contains("fingerprint"));
    assert!(!format!("{offer:?}").contains("ice-pwd"));
}

#[test]
fn legacy_entry_points_never_infer_a_role_for_an_ambiguous_multi_track_offer() {
    let offer = offer();
    assert!(matches!(
        peer().accept_offer(&offer.sdp),
        Err(Error::Description)
    ));
    let mut sender = peer();
    for purposes in [
        &[][..],
        &[Purpose::Camera, Purpose::Camera][..],
        &[Purpose::Microphone; 5][..],
    ] {
        assert!(matches!(
            sender.offer_tracks(purposes, offer.id),
            Err(Error::Description)
        ));
    }
    sender.offer_tracks(ROLES, offer.id).unwrap();
}

#[test]
fn bounded_malformed_sdp_never_allocates_tracks_or_panics() {
    let offer = offer();
    let first_mid = &offer.bindings[0].mid;
    let mut invalid = vec![
        String::new(),
        "x".repeat(MAX_SDP_BYTES + 1),
        format!("{}{}", offer.sdp, "a=x\r\n".repeat(513)),
        offer.sdp.replace("m=audio", "m=application"),
        offer.sdp.replace("opus/48000/2", "PCMU/8000"),
        offer.sdp.replace("VP8/90000", "H264/90000"),
        offer
            .sdp
            .replace("a=sendrecv", "a=sendrecv\r\na=rid:camera send"),
        offer.sdp.replace(&format!("a=mid:{first_mid}"), "a=mid:"),
        offer.sdp.replace(
            &format!("a=mid:{first_mid}"),
            &format!("a=mid:{first_mid}\r\na=mid:duplicate"),
        ),
        offer.sdp.replace(
            &format!("a=mid:{}", offer.bindings[1].mid),
            &format!("a=mid:{first_mid}"),
        ),
        format!(
            "{}m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:extra\r\na=rtpmap:96 VP8/90000\r\n",
            offer.sdp
        ),
    ];
    // An incomplete transport envelope must fail even when its prefix resembles SDP.
    for end in [1, 4, 16, 32, 64] {
        invalid.push(offer.sdp[..end].to_owned());
    }
    let mut receiver = peer();
    for sdp in invalid {
        let value = Description {
            sdp,
            ..offer.clone()
        };
        assert!(receiver.accept_offer_tracks(&value, offer.id).is_err());
        assert!(receiver.track_for(Purpose::Microphone).is_none());
    }
    receiver.accept_offer_tracks(&offer, offer.id).unwrap();
}
