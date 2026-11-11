// SPDX-License-Identifier: EUPL-1.2
//! Task 1.1: the libcrux provider and the hybrid post-quantum ciphersuite.

use nuschel_mls_spike::*;
use openmls::prelude::*;
use openmls_libcrux_crypto::CryptoProvider;
use openmls_traits::crypto::OpenMlsCrypto;
use openmls_traits::types::{AeadType, HashType, HpkeKemType};

#[test]
fn libcrux_offers_the_xwing_suite() {
    let crypto = CryptoProvider::new().unwrap();
    let suites = crypto.supported_ciphersuites();
    for suite in &suites {
        println!("libcrux lists {:#06x} {suite}", u16::from(*suite));
    }
    assert!(suites.contains(&SUITE));
    assert_eq!(u16::from(SUITE), 0x004D);
    assert_eq!(SUITE.hpke_kem_algorithm(), HpkeKemType::XWingKemDraft6);
    assert_eq!(SUITE.signature_algorithm(), SignatureScheme::ED25519);
    assert_eq!(SUITE.aead_algorithm(), AeadType::ChaCha20Poly1305);
    assert_eq!(SUITE.hash_algorithm(), HashType::Sha2_256);
    // No ML-DSA suite with libcrux: its provider signs with Ed25519 only.
    for pq_auth in [
        Ciphersuite::MLS_128_MLKEM768X25519_CHACHA20POLY1305_SHA384_MLDSA44,
        Ciphersuite::MLS_192_MLKEM768_AES256GCM_SHA384_MLDSA65,
        Ciphersuite::MLS_256_MLKEM1024_AES256GCM_SHA384_MLDSA87,
    ] {
        assert!(crypto.supports(pq_auth).is_err(), "{pq_auth}");
    }
}

#[test]
fn xwing_keys_are_hybrid() {
    let kp = Device::new("alice").key_package();
    assert_eq!(kp.ciphersuite(), SUITE);
    // X-Wing public key: ML-KEM-768 encapsulation key (1184) + X25519 (32).
    assert_eq!(kp.hpke_init_key().as_slice().len(), 1216);
    let alice = Device::new("alice");
    let group = alice.create_group(&ConversationBinding {
        server: [1; 32],
        conversation: [2; 16],
        generation: 0,
    });
    assert!(
        group.members().all(|m| m.encryption_key.len() == 1216),
        "leaf HPKE keys"
    );
    let classic = Device::with_suite("bob", CLASSIC).key_package();
    assert_eq!(classic.hpke_init_key().as_slice().len(), 32);
    println!(
        "key package: X-Wing {} bytes, X25519 {} bytes (both with the hybrid identity credential)",
        to_wire(&MlsMessageOut::from(kp)).len(),
        to_wire(&MlsMessageOut::from(classic)).len()
    );
}

fn two_party(suite: Ciphersuite) -> Result<(), String> {
    let alice = Device::with_suite("alice", suite);
    let bob = Device::with_suite("bob", suite);
    let binding = ConversationBinding {
        server: [1; 32],
        conversation: [2; 16],
        generation: 0,
    };
    let mut group = alice.create_group(&binding);
    let (_, welcome, _) = group
        .add_members(&alice.provider, alice.signer(), &[bob.key_package()])
        .map_err(|e| format!("{e:?}"))?;
    group
        .merge_pending_commit(&alice.provider)
        .map_err(|e| format!("{e:?}"))?;
    let mut bob_group = bob
        .stage_welcome(&to_wire(&welcome))?
        .into_group(&bob.provider)
        .map_err(|e| format!("{e:?}"))?;
    let message = alice.send(&mut group, "hi");
    match bob.receive(&mut bob_group, &message)? {
        Received::Application { text, .. } if text == "hi" => Ok(()),
        other => Err(format!("{other:?}")),
    }
}

/// The draft's own X-Wing suites (OpenMLS's provisional code points) are not in
/// libcrux's list, but `supports()` only checks AEAD, hash and signature.
#[test]
fn other_xwing_code_points_with_libcrux() {
    let crypto = CryptoProvider::new().unwrap();
    for suite in [
        SUITE,
        Ciphersuite::MLS_128_MLKEM768X25519_AES128GCM_SHA256_Ed25519,
        Ciphersuite::MLS_128_MLKEM768X25519_AES256GCM_SHA384_Ed25519,
        Ciphersuite::MLS_128_MLKEM768_AES256GCM_SHA384_Ed25519,
        CLASSIC,
    ] {
        let listed = crypto.supported_ciphersuites().contains(&suite);
        let supports = crypto.supports(suite).is_ok();
        let works = std::panic::catch_unwind(|| two_party(suite))
            .unwrap_or_else(|panic| Err(format!("panicked: {:?}", panic.downcast_ref::<String>())));
        println!(
            "{:#06x} {suite}: listed={listed} supports={supports} two-party={}",
            u16::from(suite),
            match &works {
                Ok(()) => "ok".to_string(),
                Err(e) => e.chars().take(80).collect(),
            }
        );
        if suite == SUITE || suite == CLASSIC {
            works.unwrap();
        }
    }
}
