// SPDX-License-Identifier: EUPL-1.2
//! Known answers, the exchange end to end, and malformed input through the real parsers.

use hpke_rs::hpke_types::{AeadAlgorithm, KdfAlgorithm, KemAlgorithm};
use hpke_rs::{Hpke, Mode};
use hpke_rs_libcrux::{HpkeLibcrux, HpkeLibcruxPrng};
use libcrux_kem::Algorithm::XWingKemDraft06;

use crate::exchange::{
    ACK, GRANT, MAX_PENDING, MAX_SEEN, MAX_VIEWERS, NEED_CREDENTIALS, REKEY, REQUEST,
    REQUEST_LIFETIME_MS, hpke_receiver, hpke_sender, open_message, seal_message, transcript,
};
use crate::*;

pub(crate) fn hex(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2), "odd hex length");
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
        .collect()
}

pub(crate) fn rng(seed: u8) -> HpkeLibcruxPrng {
    HpkeLibcruxPrng::from_seed([seed; 32])
}

pub(crate) const HOST: &str = "stream.nuschl.eu";
pub(crate) const T0: u64 = 1_800_000_000_000;

pub(crate) fn link(seed: u8) -> Link {
    Link::generate(HOST, &mut rng(seed)).unwrap()
}

pub(crate) fn sharer(link: &Link) -> Sharer {
    Sharer::new(link.keys(), CredentialCache::new(8, 64 * 1024))
}

/// A viewer's request without a binding.
pub(crate) fn request<R: rand_core::TryCryptoRng>(
    link: &Link,
    now: u64,
    rng: &mut R,
) -> (Viewer, Vec<u8>) {
    Viewer::prepare(link.keys(), now, rng)
        .unwrap()
        .finish(None, rng)
        .unwrap()
}

/// Receives a request and approves it.
pub(crate) fn admit<R: rand_core::TryCryptoRng>(
    sharer: &mut Sharer,
    request: &[u8],
    now: u64,
    rng: &mut R,
) -> ViewerId {
    let SharerEvent::Requested { viewer, digest, .. } = sharer.receive(request, now, rng).unwrap()
    else {
        panic!("expected a request");
    };
    sharer.approve(&viewer, &digest).unwrap();
    viewer
}

/// Runs one join to a confirmed key; returns the viewer, its key and the rekey's other
/// messages.
pub(crate) fn join(
    link: &Link,
    sharer: &mut Sharer,
    now: u64,
    seed: u8,
) -> (Viewer, ReceiveKey, Vec<(ViewerId, Vec<u8>)>) {
    let mut rng = rng(seed);
    let (mut viewer, request) = request(link, now, &mut rng);
    admit(sharer, &request, now, &mut rng);
    let mut others = Vec::new();
    let mut key = None;
    for (to, message) in sharer.rekey(now, &mut rng).unwrap().messages {
        if to != viewer.id() {
            others.push((to, message));
            continue;
        }
        let ViewerEvent::Granted { key: granted, ack } =
            viewer.receive(&message, now, &mut rng).unwrap()
        else {
            panic!("expected a grant");
        };
        let confirmed = sharer.receive(&ack, now, &mut rng).unwrap();
        assert!(matches!(confirmed, SharerEvent::Confirmed(id) if id == viewer.id()));
        key = Some(granted);
    }
    (viewer, key.expect("a grant for the new viewer"), others)
}

fn rekeyed(viewer: &mut Viewer, message: &[u8], now: u64) -> Result<ReceiveKey, Error> {
    match viewer.receive(message, now, &mut rng(99))? {
        ViewerEvent::Rekeyed(key) => Ok(key),
        other => panic!("expected a rekey, got {other:?}"),
    }
}

fn vector_fields(text: &str) -> Vec<(&str, Vec<u8>)> {
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| {
            line.split_once(' ')
                .map(|(name, value)| (name, hex(value)))
                .unwrap()
        })
        .collect()
}

// -------------------------------------------------------------------------------------
// Known answers

#[test]
fn xwing_draft_06_vectors() {
    let fields = vector_fields(include_str!("../test-vectors/xwing-draft-06.txt"));
    assert_eq!(fields.len(), 18);
    for vector in fields.chunks(6) {
        let field = |name| &vector.iter().find(|(n, _)| *n == name).unwrap().1;
        let (private, public) =
            libcrux_kem::key_gen_derand(XWingKemDraft06, field("seed")).unwrap();
        assert_eq!(&public.encode(), field("pk"));
        let (shared, ciphertext) = public.encapsulate_derand(field("eseed")).unwrap();
        assert_eq!(&ciphertext.encode(), field("ct"));
        assert_eq!(&shared.encode(), field("ss"));
        assert_eq!(
            &ciphertext.decapsulate(&private).unwrap().encode(),
            field("ss")
        );
    }
}

/// draft-ietf-hpke-pq: X-Wing in HPKE with this crate's KDF and AEAD, base mode, through
/// the crate's own sender and receiver.
#[test]
fn hpke_pq_xwing_vector() {
    let fields = vector_fields(include_str!("../test-vectors/hpke-pq-xwing.txt"));
    let field = |name| &fields.iter().find(|(n, _)| *n == name).unwrap().1;
    let all = |name| {
        fields
            .iter()
            .filter(move |(n, _)| *n == name)
            .map(|(_, v)| v)
    };
    let eseed: [u8; 64] = field("ikmE")[..].try_into().unwrap();
    let (enc, mut sender) = hpke_sender(field("pkRm"), &eseed, field("info"), None).unwrap();
    assert_eq!(&enc, field("enc"));
    let skr: [u8; 32] = field("skRm")[..].try_into().unwrap();
    let mut receiver = hpke_receiver(&skr, &enc, field("info"), None).unwrap();
    for (aad, ciphertext) in all("aad").zip(all("ct")) {
        assert_eq!(&sender.seal(aad, field("pt")).unwrap(), ciphertext);
        assert_eq!(&receiver.open(aad, ciphertext).unwrap(), field("pt"));
    }
    for (context, value) in all("exporter_context").zip(all("exported_value")) {
        assert_eq!(&sender.export(context, 32).unwrap(), value);
        assert_eq!(&receiver.export(context, 32).unwrap(), value);
    }
}

/// RFC 9180 A.2.2: the PSK-mode key schedule, AEAD and exporter of the suite's KDF and
/// AEAD, through the same hpke-rs and libcrux code.
#[test]
fn rfc9180_psk_mode_vector() {
    let hpke = Hpke::<HpkeLibcrux>::new_with_rng(
        Mode::Psk,
        KemAlgorithm::DhKem25519,
        KdfAlgorithm::HkdfSha256,
        AeadAlgorithm::ChaCha20Poly1305,
        rng(0),
    );
    let mut context = hpke
        .key_schedule(
            &hex("4be079c5e77779d0215b3f689595d59e3e9b0455d55662d1f3666ec606e50ea7"),
            &hex("4f6465206f6e2061204772656369616e2055726e"),
            &hex("0247fd33b913760fa1fa51e1892d9f307fbe65eb171e8132c2af18555a738b82"),
            &hex("456e6e796e20447572696e206172616e204d6f726961"),
        )
        .unwrap();
    let plaintext = hex("4265617574792069732074727574682c20747275746820626561757479");
    for (aad, ciphertext) in [
        (
            "436f756e742d30",
            "4a177f9c0d6f15cfdf533fb65bf84aecdc6ab16b8b85b4cf65a370e07fc1d78d28fb073214525276f4a89608ff",
        ),
        (
            "436f756e742d31",
            "5c3cabae2f0b3e124d8d864c116fd8f20f3f56fda988c3573b40b09997fd6c769e77c8eda6cda4f947f5b704a8",
        ),
    ] {
        assert_eq!(
            context.seal(&hex(aad), &plaintext).unwrap(),
            hex(ciphertext)
        );
    }
    for (exporter_context, value) in [
        (
            "",
            "813c1bfc516c99076ae0f466671f0ba5ff244a41699f7b2417e4c59d46d39f40",
        ),
        (
            "00",
            "2745cf3d5bb65c333658732954ee7af49eb895ce77f8022873a62a13c94cb4e1",
        ),
        (
            "54657374436f6e74657874",
            "ad40e3ae14f21c99bfdebc20ae14ab86f4ca2dc9a4799d200f43a25f99fa78ae",
        ),
    ] {
        assert_eq!(
            context.export(&hex(exporter_context), 32).unwrap(),
            hex(value)
        );
    }
}

/// Pins the wire format and key schedule: every byte of a seeded run. The browser
/// build (K2) must reproduce it.
#[test]
fn seeded_exchange_known_answer() {
    let link = Link::parse(
        "https://stream.nuschl.eu/watch#v=1&k=AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8",
        HOST,
    )
    .unwrap();
    assert_eq!(link.secret().expose(), &core::array::from_fn(|i| i as u8));
    // STREAM_ID was computed separately, with HKDF written over Python's hmac module.
    assert_eq!(link.keys().stream_id().to_vec(), hex(STREAM_ID));

    let mut rng = rng(42);
    let mut sharer = Sharer::new(link.keys(), CredentialCache::new(1, 1));
    let (mut viewer, request) = request(&link, T0, &mut rng);
    admit(&mut sharer, &request, T0, &mut rng);
    let rekey = sharer.rekey(T0, &mut rng).unwrap();
    let grant = &rekey.messages[0].1;
    let ViewerEvent::Granted { key, ack } = viewer.receive(grant, T0, &mut rng).unwrap() else {
        panic!("expected a grant");
    };
    assert_eq!(key.expose(), rekey.key.into_parts().1.as_ref());
    sharer.receive(&ack, T0, &mut rng).unwrap();
    let next = sharer
        .rekey(T0 + 1000, &mut rng)
        .unwrap()
        .messages
        .remove(0)
        .1;
    let recorded = [&request[..], grant, &ack, &next].concat();
    assert_eq!(sha256(&[&recorded]).to_vec(), hex(RECORDED));
}

const STREAM_ID: &str = "6e97e9849dc0269b159081f2dcfb96dc";
const RECORDED: &str = "62e8a8f3dc4eb042862ad3487518ee04ea27edb1f53f7939806793e775318cf7";

// -------------------------------------------------------------------------------------
// Links

#[test]
fn links_round_trip_and_keep_secrets_out_of_debug() {
    let share = link(3);
    let parsed = Link::parse(&share.share_url().unwrap(), HOST).unwrap();
    assert_eq!(parsed, share);
    let watch = Link::parse(&share.watch_url(), HOST).unwrap();
    assert_eq!(watch.secret(), share.secret());
    assert_eq!(watch.publisher(), None);
    assert_eq!(watch.share_url(), None);
    assert_eq!(watch.keys().stream_id(), share.keys().stream_id());
    assert_ne!(link(4).keys().stream_id(), share.keys().stream_id());

    let url = share.share_url().unwrap();
    let encoded_secret = url.split("k=").nth(1).unwrap().split('&').next().unwrap();
    for debug in [format!("{share:?}"), format!("{:?}", share.keys())] {
        assert!(!debug.contains(encoded_secret), "{debug}");
    }
}

#[test]
fn links_reject_every_other_form() {
    let k = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";
    let good = format!("https://stream.nuschl.eu/watch#v=1&k={k}");
    assert!(Link::parse(&good, HOST).is_ok());
    let with_port = format!("https://stream.nuschl.eu:8443/watch#v=1&k={k}");
    assert!(Link::parse(&with_port, "stream.nuschl.eu:8443").is_ok());
    for (link, error) in [
        (with_port.clone(), Error::Malformed("link origin")),
        (
            format!("http://stream.nuschl.eu/watch#v=1&k={k}"),
            Error::Malformed("link origin"),
        ),
        (
            format!("https://Stream.nuschl.eu/watch#v=1&k={k}"),
            Error::Malformed("link origin"),
        ),
        (
            format!("https://evil.example/watch#v=1&k={k}"),
            Error::Malformed("link origin"),
        ),
        (
            format!("https://stream.nuschl.eu.evil/watch#v=1&k={k}"),
            Error::Malformed("link origin"),
        ),
        (
            format!("https://stream.nuschl.eu/a/watch#v=1&k={k}"),
            Error::Malformed("link page"),
        ),
        (
            format!("https://stream.nuschl.eu/view#v=1&k={k}"),
            Error::Malformed("link page"),
        ),
        (
            "https://stream.nuschl.eu/watch".to_owned(),
            Error::Malformed("link fragment"),
        ),
        (
            format!("https://stream.nuschl.eu/watch#v=2&k={k}"),
            Error::UnsupportedVersion,
        ),
        (
            format!("https://stream.nuschl.eu/watch#k={k}&v=1"),
            Error::Malformed("link version"),
        ),
        (
            format!("https://stream.nuschl.eu/watch#v=1&k={k}&p={k}"),
            Error::Malformed("link parameters"),
        ),
        (
            format!("https://stream.nuschl.eu/share#v=1&k={k}"),
            Error::Malformed("link parameters"),
        ),
        (
            format!("https://stream.nuschl.eu/watch#v=1&k={k}A"),
            Error::Malformed("link secret"),
        ),
        (
            format!("https://stream.nuschl.eu/watch#v=1&k={}", &k[1..]),
            Error::Malformed("link secret"),
        ),
        // The last character carries 4 bits; nonzero padding bits are not canonical.
        (
            format!("https://stream.nuschl.eu/watch#v=1&k={}9", &k[..42]),
            Error::Malformed("link secret"),
        ),
        (
            format!("https://stream.nuschl.eu/watch#v=1&k={}", "a".repeat(500)),
            Error::TooLong,
        ),
    ] {
        assert_eq!(Link::parse(&link, HOST), Err(error), "{link}");
    }
    for host in [
        "Stream.nuschl.eu",
        "A]:",
        "[::1]",
        "host:",
        "host:123456",
        "a..b",
        "",
        "a/b",
    ] {
        assert_eq!(
            Link::parse(&good, host),
            Err(Error::Malformed("link host")),
            "{host}"
        );
        assert!(Link::generate(host, &mut rng(1)).is_err(), "{host}");
    }
}

// -------------------------------------------------------------------------------------
// The exchange

#[test]
fn viewers_get_only_fresh_epochs_and_removed_viewers_are_locked_out() {
    let link = link(5);
    let mut sharer = sharer(&link);
    let (mut first, key_1, _) = join(&link, &mut sharer, T0, 10);
    assert_eq!(key_1.epoch(), 1);

    // A second join moves the first viewer to epoch 2 in the same rekey.
    let (mut second, key_2, others) = join(&link, &mut sharer, T0 + 1000, 11);
    assert_eq!(key_2.epoch(), 2);
    assert_eq!(others.len(), 1);
    assert_eq!(others[0].0, first.id());
    let first_2 = rekeyed(&mut first, &others[0].1, T0 + 1000).unwrap();
    assert_eq!(first_2.expose(), key_2.expose());
    assert_ne!(key_1.expose(), key_2.expose());

    // Removing the first viewer: the next epoch goes to the second only, and the first
    // cannot open the second's Rekey.
    assert!(sharer.remove(&first.id()));
    let rekey = sharer.rekey(T0 + 2000, &mut rng(12)).unwrap();
    assert_eq!(rekey.key.epoch(), 3);
    assert_eq!(rekey.messages.len(), 1);
    assert_eq!(rekey.messages[0].0, second.id());
    let key_3 = rekeyed(&mut second, &rekey.messages[0].1, T0 + 2000).unwrap();
    assert_eq!(key_3.expose(), rekey.key.into_parts().1.as_ref());
    assert_eq!(
        first
            .receive(&rekey.messages[0].1, T0 + 2000, &mut rng(13))
            .err(),
        Some(Error::UnknownViewer)
    );
    assert_eq!(
        sharer.viewers().collect::<Vec<_>>(),
        [(second.id(), ViewerState::Confirmed)]
    );
}

/// M3: a rekey grants only approved requests; unapproved ones wait for approval.
#[test]
fn rekey_grants_only_approved_requests() {
    let link = link(30);
    let mut sharer = sharer(&link);
    let mut rng = rng(31);
    let (_, request) = request(&link, T0, &mut rng);
    let SharerEvent::Requested { viewer, digest, .. } =
        sharer.receive(&request, T0, &mut rng).unwrap()
    else {
        panic!("expected a request");
    };
    assert_eq!(
        sharer.viewers().collect::<Vec<_>>(),
        [(viewer, ViewerState::Requested)]
    );
    assert!(sharer.rekey(T0, &mut rng).unwrap().messages.is_empty());
    assert_eq!(sharer.approve(&viewer, &[0; 32]), Err(Error::UnknownViewer));
    sharer.approve(&viewer, &digest).unwrap();
    assert_eq!(
        sharer.viewers().collect::<Vec<_>>(),
        [(viewer, ViewerState::Approved)]
    );
    let rekey = sharer.rekey(T0 + 1000, &mut rng).unwrap();
    assert_eq!(rekey.messages.len(), 1);
    assert_eq!(rekey.messages[0].0, viewer);
    assert_eq!(sharer.approve(&[0; 16], &digest), Err(Error::UnknownViewer));
    assert_eq!(
        sharer.approve(&viewer, &digest),
        Err(Error::UnknownViewer),
        "already granted"
    );
}

/// N1: an approval names the request by its digest, so a late approval cannot grant a
/// newer request that reuses the viewer id.
#[test]
fn late_approvals_name_the_request() {
    let link = link(45);
    let keys = link.keys();
    let mut sharer = sharer(&link);
    let mut rng = rng(46);
    let (_, original) = request(&link, T0, &mut rng);
    let SharerEvent::Requested { viewer, digest, .. } =
        sharer.receive(&original, T0, &mut rng).unwrap()
    else {
        panic!("expected a request");
    };
    // The request expires while its binding is verified; a rekey drops it.
    let later = T0 + REQUEST_LIFETIME_MS + 1;
    sharer.rekey(later, &mut rng).unwrap();
    assert_eq!(sharer.viewers().count(), 0);

    // A link holder sends a new request under the same viewer id, with its own pk_v.
    let (_, other) = request(&link, later, &mut rng);
    let (_, mut body) = open_message(&keys, &other).unwrap();
    body[..16].copy_from_slice(&viewer);
    let reused = seal_message(&keys, REQUEST, &body, &mut rng).unwrap();
    let SharerEvent::Requested { digest: newer, .. } =
        sharer.receive(&reused, later, &mut rng).unwrap()
    else {
        panic!("expected a request");
    };
    assert_ne!(newer, digest);

    // The original verification finishes late.
    assert_eq!(sharer.approve(&viewer, &digest), Err(Error::UnknownViewer));
    assert_eq!(
        sharer.viewers().collect::<Vec<_>>(),
        [(viewer, ViewerState::Requested)]
    );
    assert!(
        sharer
            .rekey(later + 1000, &mut rng)
            .unwrap()
            .messages
            .is_empty()
    );
}

/// M1: the binding signs a digest over pk_v; the sharer reports the digest of the
/// request it received, so a binding copied into another request fails verification.
#[test]
fn copied_bindings_are_refused() {
    let link = link(32);
    let mut sharer = sharer(&link);
    let mut rng = rng(33);
    // A stand-in for the identity layer's signature over the digest.
    let identity_key = [9u8; 32];
    let sign = |digest: &[u8; 32]| sha256(&[&identity_key, digest]).to_vec();
    let verify = |binding: &VerifiableBinding| binding.call_binding == sign(&binding.digest);

    let draft = Viewer::prepare(link.keys(), T0, &mut rng).unwrap();
    let digest = draft.binding_digest();
    let binding = Binding {
        call_binding: sign(&digest),
        fingerprints: Vec::new(),
        inline: Vec::new(),
    };
    let (_, honest) = draft.finish(Some(&binding), &mut rng).unwrap();
    let SharerEvent::Requested {
        binding: Some(received),
        ..
    } = sharer.receive(&honest, T0, &mut rng).unwrap()
    else {
        panic!("expected a bound request");
    };
    assert_eq!(received.digest, digest);
    assert!(verify(&received));

    // Another link holder copies the binding into a request with its own pk_v.
    let copier = Viewer::prepare(link.keys(), T0, &mut rng).unwrap();
    assert_ne!(copier.binding_digest(), digest);
    let (_, copied) = copier.finish(Some(&binding), &mut rng).unwrap();
    let SharerEvent::Requested {
        binding: Some(received),
        ..
    } = sharer.receive(&copied, T0, &mut rng).unwrap()
    else {
        panic!("expected a bound request");
    };
    assert!(!verify(&received));
}

/// A party with S and every message cannot open a grant: the X-Wing secret is the
/// viewer's alone, and the epoch keys appear in no message, even with every signal
/// envelope opened.
#[test]
fn passive_link_holder_cannot_decrypt() {
    let link = link(6);
    let mut sharer = sharer(&link);
    let mut rng = rng(12);
    let (mut viewer, request) = request(&link, T0, &mut rng);
    admit(&mut sharer, &request, T0, &mut rng);
    let first = sharer.rekey(T0, &mut rng).unwrap();
    let grant = first.messages[0].1.clone();
    let ViewerEvent::Granted { key, ack } = viewer.receive(&grant, T0, &mut rng).unwrap() else {
        panic!("expected a grant");
    };
    let second = sharer.rekey(T0 + 1000, &mut rng).unwrap();
    let rekey = second.messages[0].1.clone();
    let next = rekeyed(&mut viewer, &rekey, T0 + 1000).unwrap();

    let keys = link.keys();
    let (_, request_body) = open_message(&keys, &request).unwrap();
    let (_, grant_body) = open_message(&keys, &grant).unwrap();
    let mut recordings = vec![request.clone(), grant.clone(), ack, rekey];
    for message in recordings.clone() {
        recordings.push(open_message(&keys, &message).unwrap().1);
    }
    for epoch_key in [key.expose(), next.expose()] {
        for recording in &recordings {
            assert!(!recording.windows(32).any(|window| window == epoch_key));
        }
    }

    // Everything but sk_v: the transcript, info, psk and enc are all known.
    let transcript = transcript(&request_body, grant_body[16..48].try_into().unwrap());
    let info = [b"nuschl/stream/v2 grant".as_slice(), &transcript].concat();
    let enc = &grant_body[48..48 + 1120];
    let sealed = &grant_body[48 + 1120..];
    let psk_id = [b"nuschl/stream/v2".as_slice(), keys.stream_id()].concat();
    let psk = Some((keys.psk.expose().as_slice(), psk_id.as_slice()));
    for guess in [
        [0u8; 32],
        [0xff; 32],
        *link.secret().expose(),
        *keys.psk.expose(),
    ] {
        let opened = hpke_receiver(&guess, enc, &info, psk)
            .and_then(|mut context| context.open(&[], sealed).map_err(|_| Error::Authentication));
        assert!(opened.is_err());
    }
}

/// L1, pinned: PSK mode authenticates a grant as "from a holder of S". A second party
/// with the link can grant a viewer its own key; media authenticity rests on the SFU's
/// P-based publish check (E1) and on identity-bound sharers (N1).
#[test]
fn any_link_holder_can_act_as_sharer() {
    let link = link(34);
    let mut genuine = sharer(&link);
    let mut impostor = sharer(&link);
    let mut rng = rng(35);
    let (mut viewer, request) = request(&link, T0, &mut rng);
    genuine.receive(&request, T0, &mut rng).unwrap();
    admit(&mut impostor, &request, T0, &mut rng);
    let forged = impostor.rekey(T0, &mut rng).unwrap();
    let ViewerEvent::Granted { key, .. } =
        viewer.receive(&forged.messages[0].1, T0, &mut rng).unwrap()
    else {
        panic!("the forged grant is accepted");
    };
    assert_eq!(key.expose(), forged.key.into_parts().1.as_ref());
}

#[test]
fn parties_without_the_link_secret_are_rejected() {
    let link = link(7);
    let other = self::link(8);
    let mut sharer = sharer(&link);
    let mut rng = rng(13);
    let (mut outsider, request) = self::request(&other, T0, &mut rng);
    assert_eq!(
        sharer.receive(&request, T0, &mut rng).err(),
        Some(Error::Authentication)
    );

    // An interceptor relaying between two links: the sharer's grant does not open under
    // the other link's keys.
    let (_, request) = self::request(&link, T0, &mut rng);
    admit(&mut sharer, &request, T0, &mut rng);
    let grant = sharer.rekey(T0, &mut rng).unwrap().messages.remove(0).1;
    assert_eq!(
        outsider.receive(&grant, T0, &mut rng).err(),
        Some(Error::Authentication)
    );
}

#[test]
fn replays_reflections_and_bad_confirmations_fail() {
    let link = link(9);
    let keys = link.keys();
    let mut sharer = sharer(&link);
    let mut rng = rng(14);
    let (mut viewer, request) = request(&link, T0, &mut rng);
    let SharerEvent::Requested { digest, .. } = sharer.receive(&request, T0, &mut rng).unwrap()
    else {
        panic!("expected a request");
    };
    assert_eq!(
        sharer.receive(&request, T0, &mut rng).err(),
        Some(Error::Replay)
    );
    // Reflected to the viewer itself.
    assert_eq!(
        viewer.receive(&request, T0, &mut rng).err(),
        Some(Error::Unexpected)
    );

    sharer.approve(&viewer.id(), &digest).unwrap();
    let grant = sharer.rekey(T0, &mut rng).unwrap().messages.remove(0).1;
    assert_eq!(
        sharer.receive(&grant, T0, &mut rng).err(),
        Some(Error::Unexpected)
    );
    // An ack carrying bytes other than the exported confirmation.
    let forged =
        seal_message(&keys, ACK, &[&viewer.id()[..], &[0; 32]].concat(), &mut rng).unwrap();
    assert_eq!(
        sharer.receive(&forged, T0, &mut rng).err(),
        Some(Error::Authentication)
    );

    let ViewerEvent::Granted { ack, .. } = viewer.receive(&grant, T0, &mut rng).unwrap() else {
        panic!("expected a grant");
    };
    assert_eq!(
        viewer.receive(&grant, T0, &mut rng).err(),
        Some(Error::Unexpected)
    );
    sharer.receive(&ack, T0, &mut rng).unwrap();
    assert_eq!(
        sharer.receive(&ack, T0, &mut rng).err(),
        Some(Error::Replay)
    );
}

/// L4: a rekey's messages may be sent twice; a viewer that misses one catches up with
/// the next, and duplicates and older ones are replays.
#[test]
fn resends_and_missed_rekeys() {
    let link = link(36);
    let mut sharer = sharer(&link);
    let (mut viewer, _, _) = join(&link, &mut sharer, T0, 37);
    let mut rng = rng(38);
    let lost = sharer
        .rekey(T0 + 1000, &mut rng)
        .unwrap()
        .messages
        .remove(0)
        .1;
    let next = sharer.rekey(T0 + 2000, &mut rng).unwrap();
    let message = &next.messages[0].1;
    let key = rekeyed(&mut viewer, message, T0 + 2000).unwrap();
    assert_eq!(key.epoch(), 3);
    assert_eq!(key.expose(), next.key.into_parts().1.as_ref());
    assert_eq!(
        rekeyed(&mut viewer, message, T0 + 2000).err(),
        Some(Error::Replay)
    );
    assert_eq!(
        rekeyed(&mut viewer, &lost, T0 + 2000).err(),
        Some(Error::Replay)
    );
}

#[test]
fn time_limits_and_clock_steps() {
    let link = link(15);
    let mut sharer = sharer(&link);
    let mut rng = rng(16);

    // Expired, or dated further ahead than a minute of clock difference allows.
    let (_, request) = self::request(&link, T0, &mut rng);
    assert_eq!(
        sharer
            .receive(&request, T0 + REQUEST_LIFETIME_MS + 1, &mut rng)
            .err(),
        Some(Error::Expired)
    );
    let (_, request) = self::request(&link, T0 + 2 * REQUEST_LIFETIME_MS, &mut rng);
    assert_eq!(
        sharer.receive(&request, T0 - 1, &mut rng).err(),
        Some(Error::Expired)
    );

    // A grant after the request's lifetime, and a viewer that never confirms.
    let (mut slow, request) = self::request(&link, T0, &mut rng);
    admit(&mut sharer, &request, T0, &mut rng);
    let grant = sharer.rekey(T0, &mut rng).unwrap().messages.remove(0).1;
    assert_eq!(sharer.rekey(T0 + 999, &mut rng).err(), Some(Error::TooSoon));
    assert_eq!(
        slow.receive(&grant, T0 + REQUEST_LIFETIME_MS + 1, &mut rng)
            .err(),
        Some(Error::Expired)
    );
    assert_eq!(sharer.viewers().count(), 1);
    let rekey = sharer
        .rekey(T0 + REQUEST_LIFETIME_MS + 1, &mut rng)
        .unwrap();
    assert!(rekey.messages.is_empty());
    assert_eq!(sharer.viewers().count(), 0);

    // L3: after the clock steps back an hour, a removal can still rekey at once.
    let now = T0 + REQUEST_LIFETIME_MS + 1 - 3_600_000;
    assert!(sharer.rekey(now, &mut rng).is_ok());
    assert_eq!(sharer.rekey(now + 1, &mut rng).err(), Some(Error::TooSoon));
}

#[test]
fn caps() {
    let link = link(17);
    let mut rng = rng(18);

    // At most MAX_PENDING requests wait for a rekey.
    let mut sharer = sharer(&link);
    for _ in 0..MAX_PENDING {
        let (_, request) = request(&link, T0, &mut rng);
        sharer.receive(&request, T0, &mut rng).unwrap();
    }
    let (_, extra) = request(&link, T0, &mut rng);
    assert_eq!(
        sharer.receive(&extra, T0, &mut rng).err(),
        Some(Error::LimitReached)
    );

    // The nonce cache holds MAX_SEEN requests until their not_after, removed or not.
    let mut sharer = self::sharer(&link);
    for _ in 0..MAX_SEEN {
        let (viewer, request) = request(&link, T0, &mut rng);
        sharer.receive(&request, T0, &mut rng).unwrap();
        sharer.remove(&viewer.id());
    }
    assert_eq!(
        sharer.receive(&extra, T0, &mut rng).err(),
        Some(Error::LimitReached)
    );
    let (_, later) = request(&link, T0 + REQUEST_LIFETIME_MS + 1, &mut rng);
    assert!(
        sharer
            .receive(&later, T0 + REQUEST_LIFETIME_MS + 1, &mut rng)
            .is_ok()
    );

    // At most MAX_VIEWERS viewers and pending requests together.
    let mut sharer = self::sharer(&link);
    let mut now = T0;
    while sharer.viewers().count() < MAX_VIEWERS {
        for _ in 0..MAX_PENDING.min(MAX_VIEWERS - sharer.viewers().count()) {
            let (_, request) = request(&link, now, &mut rng);
            admit(&mut sharer, &request, now, &mut rng);
        }
        sharer.rekey(now, &mut rng).unwrap();
        now += 1000;
    }
    // Once every nonce has expired, the viewer cap alone refuses.
    let now = now + 3 * REQUEST_LIFETIME_MS;
    let (_, extra) = request(&link, now, &mut rng);
    assert_eq!(
        sharer.receive(&extra, now, &mut rng).err(),
        Some(Error::LimitReached)
    );

    // An X-Wing key that fails the ML-KEM modulus check.
    let (_, request) = self::request(&link, T0, &mut rng);
    let (_, mut body) = open_message(&link.keys(), &request).unwrap();
    body[56..56 + 1216].fill(0xff);
    let forged = seal_message(&link.keys(), REQUEST, &body, &mut rng).unwrap();
    assert_eq!(
        self::sharer(&link).receive(&forged, T0, &mut rng).err(),
        Some(Error::InvalidPublicKey)
    );
}

#[test]
fn credentials_by_fingerprint_with_inline_fallback() {
    let link = link(19);
    let keys = link.keys();
    let mut sharer = sharer(&link);
    let mut rng = rng(20);
    // The plan's largest credentials: the identity root key and the device-signing
    // certificate, with a hybrid CallBinding.
    let root = vec![1; 1986];
    let certificate = vec![2; 5439];
    let by_fingerprint = Binding {
        call_binding: vec![3; 3471],
        fingerprints: vec![fingerprint(&root), fingerprint(&certificate)],
        inline: Vec::new(),
    };
    let join = |binding: &Binding, rng: &mut HpkeLibcruxPrng| {
        Viewer::prepare(link.keys(), T0, rng)
            .unwrap()
            .finish(Some(binding), rng)
            .unwrap()
    };

    let (mut viewer, request) = join(&by_fingerprint, &mut rng);
    let SharerEvent::NeedCredentials { message, .. } =
        sharer.receive(&request, T0, &mut rng).unwrap()
    else {
        panic!("expected a cache miss");
    };
    let ViewerEvent::NeedCredentials(missing) = viewer.receive(&message, T0, &mut rng).unwrap()
    else {
        panic!("expected missing fingerprints");
    };
    assert_eq!(missing, by_fingerprint.fingerprints);
    // L6: replayed, or naming credentials the binding did not name.
    assert_eq!(
        viewer.receive(&message, T0, &mut rng).err(),
        Some(Error::Unexpected)
    );
    let (mut other, _) = join(&by_fingerprint, &mut rng);
    let unnamed = [&other.id()[..], &[1], &[7; 32]].concat();
    let unnamed = seal_message(&keys, NEED_CREDENTIALS, &unnamed, &mut rng).unwrap();
    assert_eq!(
        other.receive(&unnamed, T0, &mut rng).err(),
        Some(Error::Unexpected)
    );

    let inline = Binding {
        inline: vec![certificate.clone(), root.clone()],
        ..by_fingerprint.clone()
    };
    let (_, request) = join(&inline, &mut rng);
    assert!(request.len() <= MAX_KEY_MESSAGE);
    let SharerEvent::Requested {
        binding: Some(binding),
        ..
    } = sharer.receive(&request, T0, &mut rng).unwrap()
    else {
        panic!("expected a bound request");
    };
    assert_eq!(binding.call_binding, vec![3; 3471]);
    assert_eq!(binding.credentials, vec![root.clone(), certificate.clone()]);
    // Cached now: fingerprints alone resolve.
    let (_, request) = join(&by_fingerprint, &mut rng);
    let SharerEvent::Requested {
        binding: Some(cached),
        ..
    } = sharer.receive(&request, T0, &mut rng).unwrap()
    else {
        panic!("expected a bound request");
    };
    assert_eq!(cached.credentials, binding.credentials);

    // Inline bytes must match a named fingerprint.
    let mismatched = Binding {
        inline: vec![vec![9; 10]],
        ..by_fingerprint.clone()
    };
    let (_, request) = join(&mismatched, &mut rng);
    assert_eq!(
        sharer.receive(&request, T0, &mut rng).err(),
        Some(Error::Malformed("credential matches no fingerprint"))
    );
    // Over the 16 KiB cap.
    let oversized = Binding {
        inline: vec![vec![4; 12_000]],
        fingerprints: vec![fingerprint(&[4; 12_000])],
        call_binding: vec![3; 3471],
    };
    let draft = Viewer::prepare(link.keys(), T0, &mut rng).unwrap();
    assert_eq!(
        draft.finish(Some(&oversized), &mut rng).err(),
        Some(Error::TooLong)
    );
}

#[test]
fn credential_cache_is_bounded() {
    let mut cache = CredentialCache::new(2, 100);
    let a = cache.insert(vec![1; 40]);
    let b = cache.insert(vec![2; 40]);
    cache.insert(vec![1; 40]); // touches a, so b is the oldest
    let c = cache.insert(vec![3; 40]);
    assert!(cache.get(&a).is_some() && cache.get(&c).is_some());
    assert!(cache.get(&b).is_none());
    let d = cache.insert(vec![4; 90]); // over 100 bytes with any other entry
    assert!(cache.get(&d).is_some() && cache.get(&a).is_none() && cache.get(&c).is_none());
    let e = cache.insert(vec![5; 101]);
    assert!(cache.get(&e).is_none() && cache.get(&d).is_some());
}

// -------------------------------------------------------------------------------------
// Malformed input through the real parsers

/// Bit flips, truncations and extensions of real messages: always an error, never a
/// panic. The signal envelope rejects them first.
#[test]
fn mutated_messages_are_rejected() {
    let link = link(21);
    let mut sharer = sharer(&link);
    let mut rng = rng(22);
    let (mut viewer, request) = request(&link, T0, &mut rng);
    admit(&mut sharer, &request, T0, &mut rng);
    let grant = sharer.rekey(T0, &mut rng).unwrap().messages.remove(0).1;
    let mut noise = self::rng(23);
    for message in [&request, &grant] {
        for _ in 0..200 {
            let mut mutated = message.clone();
            let [position, bit, kind] = random::<3, _>(&mut noise).unwrap();
            let index = usize::from(position) * mutated.len() / 256;
            match kind % 3 {
                0 => mutated[index] ^= 1 << (bit % 8),
                1 => mutated.truncate(index),
                _ => mutated.extend_from_slice(&[bit; 3]),
            }
            assert!(sharer.receive(&mutated, T0, &mut rng).is_err());
            assert!(viewer.receive(&mutated, T0, &mut rng).is_err());
        }
    }
    assert!(
        sharer
            .receive(&vec![1; MAX_KEY_MESSAGE + 1], T0, &mut rng)
            .is_err()
    );
}

/// Bodies sealed correctly under `signal_key`, so the body parsers themselves see the
/// damage: every truncation and extension fails, and byte changes never panic.
#[test]
fn mutated_bodies_reach_the_parsers() {
    let link = link(24);
    let keys = link.keys();
    let mut rng = rng(25);
    let mut noise = self::rng(26);
    let binding = Binding {
        call_binding: vec![1; 40],
        fingerprints: vec![[2; 32], fingerprint(&[3; 8])],
        inline: vec![vec![3; 8]],
    };
    let mut sharer = sharer(&link);
    let (mut viewer, request) = Viewer::prepare(link.keys(), T0, &mut rng)
        .unwrap()
        .finish(Some(&binding), &mut rng)
        .unwrap();
    let (_, request_body) = open_message(&keys, &request).unwrap();

    for len in 0..request_body.len() + 2 {
        if len == request_body.len() {
            continue;
        }
        let mut body = request_body.clone();
        body.resize(len, 0);
        let message = seal_message(&keys, REQUEST, &body, &mut rng).unwrap();
        assert!(
            sharer.receive(&message, T0, &mut rng).is_err(),
            "length {len}"
        );
    }
    let tail = request_body.len() - 100;
    for _ in 0..500 {
        let mut body = request_body.clone();
        let [position, value] = random::<2, _>(&mut noise).unwrap();
        // Mostly in the binding at the end, where the length fields are.
        body[tail + usize::from(position) % 100] = value;
        let message = seal_message(&keys, REQUEST, &body, &mut rng).unwrap();
        let _ = sharer.receive(&message, T0, &mut rng);
    }

    // Grant and NeedCredentials bodies at a new viewer, Rekey bodies at a granted one.
    let (mut granted, _, _) = join(&link, &mut self::sharer(&link), T0, 27);
    for (kind, len) in [
        (GRANT, 16 + 32 + 1120 + 52),
        (NEED_CREDENTIALS, 16 + 1 + 64),
        (REKEY, 16 + 8 + 52),
    ] {
        let target = if kind == REKEY {
            &mut granted
        } else {
            &mut viewer
        };
        for cut in [16, 17, len - 1, len + 1] {
            let mut body = target.id().to_vec();
            body.push(2);
            body.resize(cut, 0);
            let message = seal_message(&keys, kind, &body, &mut rng).unwrap();
            assert!(
                target.receive(&message, T0, &mut rng).is_err(),
                "kind {kind} length {cut}"
            );
        }
    }
    for count in [0, 5, 255] {
        let body = [&viewer.id()[..], &[count], &[0; 32 * 5]].concat();
        let message = seal_message(&keys, NEED_CREDENTIALS, &body, &mut rng).unwrap();
        assert!(
            viewer.receive(&message, T0, &mut rng).is_err(),
            "count {count}"
        );
    }
}

#[test]
fn link_parser_survives_noise() {
    let mut noise = rng(28);
    let good = link(29).share_url().unwrap();
    for _ in 0..2000 {
        let [position, value, kind] = random::<3, _>(&mut noise).unwrap();
        let mut bytes = good.clone().into_bytes();
        let index = usize::from(position) * bytes.len() / 256;
        match kind % 3 {
            0 => bytes[index] = value,
            1 => bytes.truncate(index),
            _ => bytes.insert(index, value),
        }
        if let Ok(text) = String::from_utf8(bytes)
            && let Ok(parsed) = Link::parse(&text, HOST)
        {
            let canonical = parsed.share_url().unwrap_or_else(|| parsed.watch_url());
            assert_eq!(canonical, text);
        }
    }
}
