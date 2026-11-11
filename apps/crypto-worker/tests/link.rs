// SPDX-License-Identifier: EUPL-1.2
//! Device linking in the worker (docs/e2ee.md §3.4): the four steps between two
//! workers, the linked device holding the same identity and the device-signing key
//! (every linked device, phones included, can link others: docs/e2ee.md §10, answer
//! 2), and what is refused (a wrong code, a second join, linking without the key).
mod common;

use common::*;
use nuschl_ipc::crypto::{CryptoRequest, CryptoResponse, IdentityView, RecoveryPolicy};

fn view(response: CryptoResponse) -> IdentityView {
    match response {
        CryptoResponse::Identity(view) => view,
        other => panic!("{other:?}"),
    }
}

/// Links `new` (no identity yet) to `existing`; the new device's view.
fn link(existing: &mut Client, new: &mut Client) -> IdentityView {
    let (code, pake) = offer(existing);
    let CryptoResponse::LinkJoin { pake, device_keys } = join(new, &code, &pake) else {
        panic!()
    };
    let CryptoResponse::LinkBundle(bundle) =
        existing.ask(CryptoRequest::LinkApprove { pake, device_keys })
    else {
        panic!()
    };
    view(new.ask(CryptoRequest::LinkComplete { bundle }))
}

#[test]
fn a_linked_phone_links_the_next_device() {
    let mut laptop = Client::new();
    let own = view(laptop.ask(CryptoRequest::CreateIdentity {
        device_name: "laptop".into(),
        recovery: RecoveryPolicy::Never,
    }));
    let mut phone = Client::new();
    let phone_view = link(&mut laptop, &mut phone);
    assert!(
        phone_view.can_link,
        "the phone holds the device-signing key"
    );
    // The laptop is gone; the phone adds a tablet on its own.
    drop(laptop);
    let mut tablet = Client::new();
    let tablet_view = link(&mut phone, &mut tablet);
    assert_eq!(tablet_view.fingerprint, own.fingerprint);
    assert!(tablet_view.can_link);
}

fn offer(existing: &mut Client) -> (String, Vec<u8>) {
    match existing.ask(CryptoRequest::LinkOffer) {
        CryptoResponse::LinkOffer { code, pake } => (code, pake),
        other => panic!("{other:?}"),
    }
}

fn join(new: &mut Client, code: &str, pake: &[u8]) -> CryptoResponse {
    new.ask(CryptoRequest::LinkJoin {
        code: code.into(),
        pake: pake.to_vec(),
        device_name: "phone".into(),
    })
}

#[test]
fn a_linked_device_holds_the_same_identity() {
    let mut laptop = Client::new();
    let own = view(laptop.ask(CryptoRequest::CreateIdentity {
        device_name: "laptop".into(),
        recovery: RecoveryPolicy::Never,
    }));
    let mut phone = Client::new();
    let (code, pake) = offer(&mut laptop);
    assert_eq!(code.len(), code.split('-').next().unwrap().len() + 10);
    let CryptoResponse::LinkJoin { pake, device_keys } = join(&mut phone, &code, &pake) else {
        panic!()
    };
    let CryptoResponse::LinkBundle(bundle) =
        laptop.ask(CryptoRequest::LinkApprove { pake, device_keys })
    else {
        panic!()
    };
    let linked = view(phone.ask(CryptoRequest::LinkComplete { bundle }));
    assert_eq!(linked.fingerprint, own.fingerprint);
    assert_eq!(linked.device_name, "phone");
    assert!(linked.created && linked.can_link);
    // Persisted: the phone reopens with the identity.
    phone = phone.restart();
    let reopened = view(phone.ask(CryptoRequest::Identity));
    assert_eq!(reopened.fingerprint, own.fingerprint);
    // A device with an identity cannot join another.
    let (code, pake) = offer(&mut laptop);
    assert!(matches!(
        join(&mut phone, &code, &pake),
        CryptoResponse::Rejected(_)
    ));
}

#[test]
fn a_wrong_code_and_out_of_order_steps_are_refused() {
    let mut laptop = Client::new();
    view(laptop.ask(CryptoRequest::CreateIdentity {
        device_name: "laptop".into(),
        recovery: RecoveryPolicy::Never,
    }));
    // No identity yet: nothing to link.
    let mut phone = Client::new();
    assert!(matches!(
        phone.ask(CryptoRequest::LinkOffer),
        CryptoResponse::Rejected(_)
    ));
    let (code, pake) = offer(&mut laptop);
    // The phone typed another code: the laptop's confirmation check fails.
    let mut digits: Vec<char> = code.chars().collect();
    let last = digits.len() - 1;
    digits[last] = if digits[last] == '0' { '1' } else { '0' };
    let wrong: String = digits.into_iter().collect();
    let CryptoResponse::LinkJoin { pake, device_keys } = join(&mut phone, &wrong, &pake) else {
        panic!()
    };
    assert!(matches!(
        laptop.ask(CryptoRequest::LinkApprove {
            pake: pake.clone(),
            device_keys: device_keys.clone()
        }),
        CryptoResponse::Rejected(_)
    ));
    // The offer is used up: approving again needs a new code.
    assert!(matches!(
        laptop.ask(CryptoRequest::LinkApprove { pake, device_keys }),
        CryptoResponse::Rejected(_)
    ));
    // A bundle without a link in progress, and garbage, are refused.
    let mut other = Client::new();
    assert!(matches!(
        other.ask(CryptoRequest::LinkComplete {
            bundle: vec![0; 64]
        }),
        CryptoResponse::Rejected(_)
    ));
}
