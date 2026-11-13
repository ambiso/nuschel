// SPDX-License-Identifier: EUPL-1.2
//! Known answers, the exchange end to end, and malformed input through the real parsers.

use hpke_rs::hpke_types::{AeadAlgorithm, KdfAlgorithm, KemAlgorithm};
use hpke_rs::{Hpke, HpkePrivateKey, Mode};
use hpke_rs_libcrux::{HpkeLibcrux, HpkeLibcruxPrng};
use libcrux_kem::Algorithm::XWingKemDraft06;

use crate::exchange::{
    ACK, GRANT, MAX_PENDING, NEED_CREDENTIALS, REKEY, REQUEST, REQUEST_LIFETIME_MS, open_message,
    seal_message, transcript,
};
use crate::*;

pub(crate) fn hex(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2), "odd hex length");
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
        .collect()
}

fn rng(seed: u8) -> HpkeLibcruxPrng {
    HpkeLibcruxPrng::from_seed([seed; 32])
}

fn link(seed: u8) -> Link {
    Link::generate("https://stream.nuschl.eu", &mut rng(seed)).unwrap()
}

fn sharer(link: &Link, now: u64) -> Sharer {
    Sharer::new(
        link.keys(),
        CredentialCache::new(8, 64 * 1024),
        now,
        &mut rng(1),
    )
    .unwrap()
}

const T0: u64 = 1_800_000_000_000;

/// Joins one viewer and runs the exchange to a confirmed key.
fn join(link: &Link, sharer: &mut Sharer, now: u64, seed: u8) -> (Viewer, EpochKey) {
    let mut rng = rng(seed);
    let (mut viewer, request) = Viewer::join(link.keys(), None, now, &mut rng).unwrap();
    assert!(matches!(
        sharer.receive(&request, now, &mut rng),
        Ok(SharerEvent::Requested { binding: None, .. })
    ));
    let mut key = None;
    for (to, message) in sharer.rekey(now, &mut rng).unwrap() {
        if to == viewer.id() {
            let ViewerEvent::Granted { key: granted, ack } =
                viewer.receive(&message, now, &mut rng).unwrap()
            else {
                panic!("expected a grant");
            };
            let confirmed = sharer.receive(&ack, now, &mut rng).unwrap();
            assert!(matches!(confirmed, SharerEvent::Confirmed(id) if id == viewer.id()));
            key = Some(granted);
        }
    }
    (viewer, key.expect("a grant for the new viewer"))
}

// -------------------------------------------------------------------------------------
// Known answers

#[test]
fn xwing_draft_06_vectors() {
    let text = include_str!("../test-vectors/xwing-draft-06.txt");
    let fields: Vec<(&str, Vec<u8>)> = text
        .lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| {
            line.split_once(' ')
                .map(|(name, value)| (name, hex(value)))
                .unwrap()
        })
        .collect();
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
    let link = Link::parse(&format!(
        "https://stream.nuschl.eu/watch#v=1&k={}",
        "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8"
    ))
    .unwrap();
    assert_eq!(link.secret().expose(), &core::array::from_fn(|i| i as u8));
    let keys = link.keys();
    // STREAM_ID was computed separately, with HKDF written over Python's hmac module.
    assert_eq!(keys.stream_id().to_vec(), hex(STREAM_ID));

    let mut rng = rng(42);
    let mut sharer = Sharer::new(link.keys(), CredentialCache::new(1, 1), T0, &mut rng).unwrap();
    let (mut viewer, request) = Viewer::join(link.keys(), None, T0, &mut rng).unwrap();
    sharer.receive(&request, T0 + 1000, &mut rng).unwrap();
    let mut recorded = request.clone();
    let (_, grant) = sharer.rekey(T0 + 1000, &mut rng).unwrap().remove(0);
    let ViewerEvent::Granted { key, ack } = viewer.receive(&grant, T0 + 1000, &mut rng).unwrap()
    else {
        panic!("expected a grant");
    };
    sharer.receive(&ack, T0 + 1000, &mut rng).unwrap();
    let (_, rekey) = sharer.rekey(T0 + 2000, &mut rng).unwrap().remove(0);
    let frame = FrameEncryptor::new(&key, 1)
        .unwrap()
        .encrypt(true, b"av1")
        .unwrap();
    for message in [&grant, &ack, &rekey, &frame] {
        recorded.extend_from_slice(message);
    }
    assert_eq!(sha256(&[&recorded]).to_vec(), hex(RECORDED));
}

const STREAM_ID: &str = "6e97e9849dc0269b159081f2dcfb96dc";
const RECORDED: &str = "239be01d20e9a077782842b477646cd97b90d79663f26b9a15d475fc08eb7142";

// -------------------------------------------------------------------------------------
// Links

#[test]
fn links_round_trip_and_keep_secrets_out_of_debug() {
    let share = link(3);
    let parsed = Link::parse(&share.share_url().unwrap()).unwrap();
    assert_eq!(parsed, share);
    let watch = Link::parse(&share.watch_url()).unwrap();
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
    assert!(Link::parse(&good).is_ok());
    for (link, error) in [
        (
            format!("http://stream.nuschl.eu/watch#v=1&k={k}"),
            Error::Malformed("link scheme"),
        ),
        (
            format!("https://stream.nuschl.eu/a/watch#v=1&k={k}"),
            Error::Malformed("link host"),
        ),
        (
            format!("https://user@host/watch#v=1&k={k}"),
            Error::Malformed("link host"),
        ),
        (
            format!("https:///watch#v=1&k={k}"),
            Error::Malformed("link host"),
        ),
        (
            format!("https://host/view#v=1&k={k}"),
            Error::Malformed("link page"),
        ),
        (
            "https://host/watch".to_owned(),
            Error::Malformed("link fragment"),
        ),
        (
            format!("https://host/watch#v=2&k={k}"),
            Error::UnsupportedVersion,
        ),
        (
            format!("https://host/watch#k={k}&v=1"),
            Error::Malformed("link version"),
        ),
        (
            format!("https://host/watch#v=1&k={k}&p={k}"),
            Error::Malformed("link parameters"),
        ),
        (
            format!("https://host/share#v=1&k={k}"),
            Error::Malformed("link parameters"),
        ),
        (
            format!("https://host/watch#v=1&k={k}A"),
            Error::Malformed("link secret"),
        ),
        (
            format!("https://host/watch#v=1&k={}", &k[1..]),
            Error::Malformed("link secret"),
        ),
        // The last character carries 4 bits; nonzero padding bits are not canonical.
        (
            format!("https://host/watch#v=1&k={}9", &k[..42]),
            Error::Malformed("link secret"),
        ),
        (
            format!("https://host/watch#v=1&k={}+", &k[..42]),
            Error::Malformed("link secret"),
        ),
        (
            format!("https://{}/watch#v=1&k={k}", "a".repeat(500)),
            Error::TooLong,
        ),
    ] {
        assert_eq!(Link::parse(&link), Err(error), "{link}");
    }
}

// -------------------------------------------------------------------------------------
// The exchange

#[test]
fn viewers_get_only_fresh_epochs_and_removed_viewers_are_locked_out() {
    let link = link(5);
    let mut sharer = sharer(&link, T0);
    let (mut first, key_2) = join(&link, &mut sharer, T0 + 1000, 10);
    assert_eq!(key_2.epoch(), 2);
    assert_eq!(&key_2, sharer.epoch());

    let mut frames_2 = FrameEncryptor::new(&key_2, 0).unwrap();
    let mut first_frames = FrameDecryptor::new();
    first_frames.install(&key_2, T0 + 1000).unwrap();
    let frame = frames_2.encrypt(true, b"before the second viewer").unwrap();
    assert_eq!(
        first_frames.decrypt(&frame, T0 + 1500),
        Ok((true, b"before the second viewer".to_vec()))
    );

    // A second join moves the first viewer to epoch 3 in the same rekey.
    let mut rng = rng(11);
    let (mut second, request) = Viewer::join(link.keys(), None, T0 + 2000, &mut rng).unwrap();
    sharer.receive(&request, T0 + 2000, &mut rng).unwrap();
    let mut key_3 = None;
    for (to, message) in sharer.rekey(T0 + 2000, &mut rng).unwrap() {
        if to == first.id() {
            let ViewerEvent::Rekeyed(key) = first.receive(&message, T0 + 2000, &mut rng).unwrap()
            else {
                panic!("expected a rekey");
            };
            first_frames.install(&key, T0 + 2000).unwrap();
        } else {
            let ViewerEvent::Granted { key, ack } =
                second.receive(&message, T0 + 2000, &mut rng).unwrap()
            else {
                panic!("expected a grant");
            };
            sharer.receive(&ack, T0 + 2000, &mut rng).unwrap();
            key_3 = Some(key);
        }
    }
    let key_3 = key_3.unwrap();
    assert_eq!(key_3.epoch(), 3);
    let mut second_frames = FrameDecryptor::new();
    second_frames.install(&key_3, T0 + 2000).unwrap();
    assert_eq!(
        second_frames.decrypt(&frame, T0 + 2000),
        Err(Error::UnknownKey)
    );

    // The previous epoch stays readable for two seconds.
    let late = frames_2.encrypt(false, b"in flight").unwrap();
    assert_eq!(
        first_frames.decrypt(&late, T0 + 3999),
        Ok((false, b"in flight".to_vec()))
    );
    let later = frames_2.encrypt(false, b"too late").unwrap();
    assert_eq!(
        first_frames.decrypt(&later, T0 + 4000),
        Err(Error::UnknownKey)
    );
    assert_eq!(
        first_frames.install(&key_2, T0 + 4000),
        Err(Error::StaleEpoch)
    );

    // Removing the first viewer: the next epoch goes to the second only.
    assert!(sharer.remove(&first.id()));
    let messages = sharer.rekey(T0 + 4000, &mut rng).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].0, second.id());
    let ViewerEvent::Rekeyed(key_4) = second.receive(&messages[0].1, T0 + 4000, &mut rng).unwrap()
    else {
        panic!("expected a rekey");
    };
    second_frames.install(&key_4, T0 + 4000).unwrap();
    let frame_4 = FrameEncryptor::new(sharer.epoch(), 0)
        .unwrap()
        .encrypt(true, b"after removal")
        .unwrap();
    assert!(second_frames.decrypt(&frame_4, T0 + 4000).is_ok());
    assert_eq!(
        first_frames.decrypt(&frame_4, T0 + 4000),
        Err(Error::UnknownKey)
    );
    assert_eq!(
        first.receive(&messages[0].1, T0 + 4000, &mut rng).err(),
        Some(Error::UnknownViewer)
    );
    assert_eq!(
        sharer.viewers().collect::<Vec<_>>(),
        [(second.id(), ViewerState::Confirmed)]
    );
}

/// A party with S and every message cannot open a grant: the X-Wing secret is the
/// viewer's alone, and the epoch key appears in no message, even with every signal
/// envelope opened.
#[test]
fn passive_link_holder_cannot_decrypt() {
    let link = link(6);
    let mut sharer = sharer(&link, T0);
    let mut rng = rng(12);
    let (mut viewer, request) = Viewer::join(link.keys(), None, T0 + 1000, &mut rng).unwrap();
    sharer.receive(&request, T0 + 1000, &mut rng).unwrap();
    let (_, grant) = sharer.rekey(T0 + 1000, &mut rng).unwrap().remove(0);
    let ViewerEvent::Granted { key, ack } = viewer.receive(&grant, T0 + 1000, &mut rng).unwrap()
    else {
        panic!("expected a grant");
    };
    let (_, rekey) = sharer.rekey(T0 + 2000, &mut rng).unwrap().remove(0);
    let ViewerEvent::Rekeyed(next) = viewer.receive(&rekey, T0 + 2000, &mut rng).unwrap() else {
        panic!("expected a rekey");
    };

    let keys = link.keys();
    let (_, request_body) = open_message(&keys, &request).unwrap();
    let (_, grant_body) = open_message(&keys, &grant).unwrap();
    let mut recordings = vec![request.clone(), grant.clone(), ack, rekey];
    for message in recordings.clone() {
        recordings.push(open_message(&keys, &message).unwrap().1);
    }
    for epoch_key in [&key, &next] {
        let encoded = epoch_key.encode();
        let secret = &encoded[4..];
        for recording in &recordings {
            assert!(!recording.windows(32).any(|window| window == secret));
        }
    }

    // Everything but sk_v: the transcript, info, psk and enc are all known.
    let transcript = transcript(&request_body, grant_body[16..48].try_into().unwrap());
    let info = [b"nuschl/stream/v2 grant".as_slice(), &transcript].concat();
    let enc = &grant_body[48..48 + 1120];
    let sealed = &grant_body[48 + 1120..48 + 1120 + 52];
    let hpke = Hpke::<HpkeLibcrux>::new_with_rng(
        Mode::Psk,
        KemAlgorithm::XWingDraft06,
        KdfAlgorithm::HkdfSha256,
        AeadAlgorithm::ChaCha20Poly1305,
        self::rng(0),
    );
    let psk_id = [b"nuschl/stream/v2".as_slice(), keys.stream_id()].concat();
    for guess in [
        [0u8; 32],
        [0xff; 32],
        *link.secret().expose(),
        *keys.psk.expose(),
    ] {
        let opened = hpke
            .setup_receiver(
                enc,
                &HpkePrivateKey::new(guess.to_vec()),
                &info,
                Some(keys.psk.expose()),
                Some(&psk_id),
                None,
            )
            .and_then(|mut context| context.open(&[], sealed));
        assert!(opened.is_err());
    }
}

#[test]
fn parties_without_the_link_secret_are_rejected() {
    let link = link(7);
    let other = self::link(8);
    let mut sharer = sharer(&link, T0);
    let mut rng = rng(13);
    let (mut outsider, request) = Viewer::join(other.keys(), None, T0, &mut rng).unwrap();
    assert_eq!(
        sharer.receive(&request, T0, &mut rng).err(),
        Some(Error::Authentication)
    );

    // An interceptor relaying between two links: the sharer's grant does not open
    // under the other link's keys.
    let (_, request) = Viewer::join(link.keys(), None, T0, &mut rng).unwrap();
    sharer.receive(&request, T0 + 1000, &mut rng).unwrap();
    let (_, grant) = sharer.rekey(T0 + 1000, &mut rng).unwrap().remove(0);
    assert_eq!(
        outsider.receive(&grant, T0 + 1000, &mut rng).err(),
        Some(Error::Authentication)
    );
}

#[test]
fn replays_reflections_and_bad_confirmations_fail() {
    let link = link(9);
    let keys = link.keys();
    let mut sharer = sharer(&link, T0);
    let mut rng = rng(14);
    let (mut viewer, request) = Viewer::join(link.keys(), None, T0, &mut rng).unwrap();
    sharer.receive(&request, T0, &mut rng).unwrap();
    assert_eq!(
        sharer.receive(&request, T0, &mut rng).err(),
        Some(Error::Replay)
    );
    // Reflected to the viewer itself.
    assert_eq!(
        viewer.receive(&request, T0, &mut rng).err(),
        Some(Error::Unexpected)
    );

    let (_, grant) = sharer.rekey(T0 + 1000, &mut rng).unwrap().remove(0);
    assert_eq!(
        sharer.receive(&grant, T0 + 1000, &mut rng).err(),
        Some(Error::Unexpected)
    );

    // A KeyAck carrying the sharer's own confirmation.
    let (_, grant_body) = open_message(&keys, &grant).unwrap();
    let reflected = [&viewer.id()[..], &grant_body[grant_body.len() - 32..]].concat();
    let reflected = seal_message(&keys, ACK, &reflected, &mut rng).unwrap();
    assert_eq!(
        sharer.receive(&reflected, T0 + 1000, &mut rng).err(),
        Some(Error::Authentication)
    );

    let ViewerEvent::Granted { ack, .. } = viewer.receive(&grant, T0 + 1000, &mut rng).unwrap()
    else {
        panic!("expected a grant");
    };
    assert_eq!(
        viewer.receive(&grant, T0 + 1000, &mut rng).err(),
        Some(Error::Unexpected)
    );
    sharer.receive(&ack, T0 + 1000, &mut rng).unwrap();
    assert_eq!(
        sharer.receive(&ack, T0 + 1000, &mut rng).err(),
        Some(Error::Replay)
    );

    let first = sharer.rekey(T0 + 2000, &mut rng).unwrap().remove(0).1;
    let second = sharer.rekey(T0 + 3000, &mut rng).unwrap().remove(0).1;
    assert!(
        matches!(viewer.receive(&second, T0 + 3000, &mut rng), Ok(ViewerEvent::Rekeyed(k)) if k.epoch() == 4)
    );
    assert_eq!(
        viewer.receive(&first, T0 + 3000, &mut rng).err(),
        Some(Error::Replay)
    );
    assert_eq!(
        viewer.receive(&second, T0 + 3000, &mut rng).err(),
        Some(Error::Replay)
    );
}

#[test]
fn time_and_size_limits() {
    let link = link(15);
    let mut sharer = sharer(&link, T0);
    let mut rng = rng(16);

    // Expired, or dated further ahead than a minute of clock difference allows.
    let (_, request) = Viewer::join(link.keys(), None, T0, &mut rng).unwrap();
    assert_eq!(
        sharer
            .receive(&request, T0 + REQUEST_LIFETIME_MS + 1, &mut rng)
            .err(),
        Some(Error::Expired)
    );
    let (_, request) =
        Viewer::join(link.keys(), None, T0 + 2 * REQUEST_LIFETIME_MS, &mut rng).unwrap();
    assert_eq!(
        sharer.receive(&request, T0 - 1, &mut rng).err(),
        Some(Error::Expired)
    );

    // A grant after the request's lifetime, and a viewer that never confirms.
    let (mut slow, request) = Viewer::join(link.keys(), None, T0, &mut rng).unwrap();
    sharer.receive(&request, T0, &mut rng).unwrap();
    assert_eq!(sharer.rekey(T0 + 999, &mut rng).err(), Some(Error::TooSoon));
    let (_, grant) = sharer.rekey(T0 + 1000, &mut rng).unwrap().remove(0);
    assert_eq!(
        slow.receive(&grant, T0 + REQUEST_LIFETIME_MS + 1, &mut rng)
            .err(),
        Some(Error::Expired)
    );
    assert_eq!(sharer.viewers().count(), 1);
    assert!(
        sharer
            .rekey(T0 + REQUEST_LIFETIME_MS + 1, &mut rng)
            .unwrap()
            .is_empty()
    );
    assert_eq!(sharer.viewers().count(), 0);

    // At most MAX_PENDING requests wait for a rekey.
    let now = T0 + 2 * REQUEST_LIFETIME_MS;
    for _ in 0..MAX_PENDING {
        let (_, request) = Viewer::join(link.keys(), None, now, &mut rng).unwrap();
        sharer.receive(&request, now, &mut rng).unwrap();
    }
    let (_, request) = Viewer::join(link.keys(), None, now, &mut rng).unwrap();
    assert_eq!(
        sharer.receive(&request, now, &mut rng).err(),
        Some(Error::LimitReached)
    );

    // An X-Wing key that fails the ML-KEM modulus check.
    let (viewer, request) = Viewer::join(link.keys(), None, now, &mut rng).unwrap();
    let (_, mut body) = open_message(&link.keys(), &request).unwrap();
    body[56..56 + 1216].fill(0xff);
    let forged = seal_message(&link.keys(), REQUEST, &body, &mut rng).unwrap();
    let mut fresh = self::sharer(&link, now);
    assert_eq!(
        fresh.receive(&forged, now, &mut rng).err(),
        Some(Error::InvalidPublicKey)
    );
    drop(viewer);
}

#[test]
fn credentials_by_fingerprint_with_inline_fallback() {
    let link = link(17);
    let mut sharer = sharer(&link, T0);
    let mut rng = rng(18);
    // The plan's largest credentials: the identity root key and the device-signing
    // certificate, with a hybrid CallBinding.
    let root = vec![1; 1986];
    let certificate = vec![2; 5439];
    let by_fingerprint = Binding {
        call_binding: vec![3; 3471],
        fingerprints: vec![fingerprint(&root), fingerprint(&certificate)],
        inline: Vec::new(),
    };

    let (mut viewer, request) =
        Viewer::join(link.keys(), Some(&by_fingerprint), T0, &mut rng).unwrap();
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

    let inline = Binding {
        inline: vec![certificate.clone(), root.clone()],
        ..by_fingerprint.clone()
    };
    let (_, request) = Viewer::join(link.keys(), Some(&inline), T0, &mut rng).unwrap();
    assert!(request.len() <= MAX_KEY_MESSAGE);
    let expected = VerifiableBinding {
        call_binding: vec![3; 3471],
        credentials: vec![root.clone(), certificate.clone()],
    };
    assert!(matches!(
        sharer.receive(&request, T0, &mut rng).unwrap(),
        SharerEvent::Requested { binding: Some(b), .. } if b == expected
    ));
    // Cached now: fingerprints alone resolve.
    let (_, request) = Viewer::join(link.keys(), Some(&by_fingerprint), T0, &mut rng).unwrap();
    assert!(matches!(
        sharer.receive(&request, T0, &mut rng).unwrap(),
        SharerEvent::Requested { binding: Some(b), .. } if b == expected
    ));

    // Inline bytes must match a named fingerprint.
    let mismatched = Binding {
        inline: vec![vec![9; 10]],
        ..by_fingerprint.clone()
    };
    let (_, request) = Viewer::join(link.keys(), Some(&mismatched), T0, &mut rng).unwrap();
    assert_eq!(
        sharer.receive(&request, T0, &mut rng).err(),
        Some(Error::Malformed("credential matches no fingerprint"))
    );
    // Over the 16 KiB cap.
    let oversized = Binding {
        inline: vec![vec![4; 12_000]],
        fingerprints: vec![fingerprint(&vec![4; 12_000])],
        call_binding: vec![3; 3471],
    };
    assert_eq!(
        Viewer::join(link.keys(), Some(&oversized), T0, &mut rng).err(),
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
    let link = link(19);
    let mut sharer = sharer(&link, T0);
    let mut rng = rng(20);
    let (mut viewer, request) = Viewer::join(link.keys(), None, T0, &mut rng).unwrap();
    sharer.receive(&request, T0, &mut rng).unwrap();
    let (_, grant) = sharer.rekey(T0 + 1000, &mut rng).unwrap().remove(0);
    let mut noise = self::rng(21);
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
            assert!(sharer.receive(&mutated, T0 + 1000, &mut rng).is_err());
            assert!(viewer.receive(&mutated, T0 + 1000, &mut rng).is_err());
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
    let link = link(22);
    let keys = link.keys();
    let mut rng = rng(23);
    let mut noise = self::rng(24);
    let binding = Binding {
        call_binding: vec![1; 40],
        fingerprints: vec![[2; 32], fingerprint(&[3; 8])],
        inline: vec![vec![3; 8]],
    };
    let mut sharer = sharer(&link, T0);
    let (viewer, request) = Viewer::join(link.keys(), Some(&binding), T0, &mut rng).unwrap();
    let (_, request_body) = open_message(&keys, &request).unwrap();

    for len in 0..request_body.len() + 2 {
        let mut body = request_body.clone();
        body.resize(len, 0);
        if len == request_body.len() {
            continue;
        }
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
    let mut viewer = viewer;
    let (mut granted, _) = join(&link, &mut self::sharer(&link, T0), T0 + 1000, 27);
    for (kind, len) in [
        (GRANT, 16 + 32 + 1120 + 52 + 32),
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
                target.receive(&message, T0 + 1000, &mut rng).is_err(),
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
    let mut noise = rng(25);
    let good = link(26).share_url().unwrap();
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
            && let Ok(parsed) = Link::parse(&text)
        {
            let canonical = parsed.share_url().unwrap_or_else(|| parsed.watch_url());
            assert_eq!(canonical, text);
        }
    }
}
