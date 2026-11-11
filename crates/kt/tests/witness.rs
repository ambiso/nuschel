// SPDX-License-Identifier: EUPL-1.2
//! Witnessing (docs/e2ee.md §5.9): the epoch log, the project auditor's checks and
//! what clients require, against honest and malicious servers.
mod common;

use common::*;
use nuschl_kt::witness::auditor::{AddEpochs, AddError, LogState, add_epochs};
use nuschl_kt::witness::note::{Cosigner, Note};
use nuschl_kt::witness::{Checkpoint, origin};
use nuschl_kt::{
    Directory, KtError, MemoryStore, SignedHead, WitnessOutcome, WitnessPolicy, WitnessedHead,
    check_witnessed, verify_head,
};
use std::sync::Arc;

const ORIGIN: &str = "chat.example.org:64738/nuschl-kt/v1";
const HOUR: u64 = 3600 * 1000;

fn auditor() -> Cosigner {
    Cosigner::from_seed("witness.example.org/nuschl-kt", &[0x77; 32]).unwrap()
}

fn policy(auditor: &Cosigner) -> WitnessPolicy {
    WitnessPolicy::all(vec![auditor.key().clone()], 24 * HOUR)
}

/// What a server does: submit to the auditor (following 409s), and keep the
/// cosigned checkpoint.
async fn submit(
    server: &impl Directory,
    auditor: &Cosigner,
    state: &mut Option<LogState>,
    now_ms: u64,
) -> Result<String, AddError> {
    let mut old_size = server.log_size().await.unwrap();
    for _ in 0..3 {
        let request = server.add_epochs(ORIGIN, old_size, 4096).await.unwrap();
        // The request travels as bytes.
        let request = AddEpochs::decode(&request.encode().unwrap()).unwrap();
        match add_epochs(state.as_ref(), &request) {
            Ok(accepted) => {
                let mut kept = accepted.state;
                kept.timestamp = now_ms / 1000;
                let kept = LogState::decode(&kept.encode().unwrap()).unwrap();
                *state = Some(kept);
                let mut note = Note::parse(&request.checkpoint).unwrap();
                note.signatures
                    .push(auditor.cosign(&accepted.checkpoint, now_ms / 1000).unwrap());
                return Ok(note.format());
            }
            Err(AddError::Conflict { size }) => old_size = size,
            Err(error) => return Err(error),
        }
    }
    panic!("the auditor kept answering conflicts");
}

/// What a server attaches to a head.
async fn witnessed(server: &impl Directory, note: &str, epoch: u64) -> WitnessedHead {
    let size = Checkpoint::parse(&Note::parse(note.as_bytes()).unwrap().text)
        .unwrap()
        .size;
    WitnessedHead {
        checkpoint: note.as_bytes().to_vec(),
        inclusion: if epoch < size {
            server.inclusion(epoch, size).await.unwrap()
        } else {
            Vec::new()
        },
    }
}

async fn check(
    server: &impl Directory,
    head: &SignedHead,
    attached: &WitnessedHead,
    policy: &WitnessPolicy,
    now_ms: u64,
) -> Result<WitnessOutcome, KtError> {
    let head = verify_head(server.descriptor(), head).unwrap();
    let attached = WitnessedHead::decode(&attached.encode().unwrap()).unwrap();
    check_witnessed(
        server.descriptor(),
        ORIGIN,
        &head,
        &attached,
        policy,
        now_ms,
    )
}

#[test]
fn the_origin_is_the_sticky_key_with_the_suffix() {
    assert_eq!(origin("chat.example.org:64738").unwrap(), ORIGIN);
}

#[tokio::test]
async fn an_honest_server_is_witnessed_from_its_first_epoch() {
    let server = open().await;
    let auditor = auditor();
    let policy = policy(&auditor);
    let mut state = None;
    assert_eq!(server.log_size().await.unwrap(), 1);

    // A new log: the auditor starts at epoch 0, akd's empty directory.
    let note = submit(&server, &auditor, &mut state, T0).await.unwrap();
    let head = server.latest_head().await.unwrap();
    let attached = witnessed(&server, &note, head.epoch).await;
    assert_eq!(
        check(&server, &head, &attached, &policy, T0 + 1000).await,
        Ok(WitnessOutcome::Witnessed {
            size: 1,
            cosigned_ms: T0
        })
    );

    // Epochs follow; the next submission carries their akd proofs.
    for n in 1..=5 {
        server
            .publish(vec![(acct(n), binding(n, 1))], T0 + u64::from(n) * 1000)
            .await
            .unwrap();
    }
    let head = server.latest_head().await.unwrap();
    assert_eq!(head.epoch, 5);
    // Published but not yet cosigned: pending.
    let stale = witnessed(&server, &note, head.epoch).await;
    assert!(matches!(
        check(&server, &head, &stale, &policy, T0 + 10_000).await,
        Ok(WitnessOutcome::Pending { size: 1, .. })
    ));
    let note = submit(&server, &auditor, &mut state, T0 + 60_000)
        .await
        .unwrap();
    assert_eq!(state.as_ref().unwrap().size(), 6);
    let attached = witnessed(&server, &note, head.epoch).await;
    assert_eq!(
        check(&server, &head, &attached, &policy, T0 + 61_000).await,
        Ok(WitnessOutcome::Witnessed {
            size: 6,
            cosigned_ms: T0 + 60_000
        })
    );

    // Hourly: the same checkpoint again, with a fresh time.
    let note = submit(&server, &auditor, &mut state, T0 + 30 * HOUR)
        .await
        .unwrap();
    let attached = witnessed(&server, &note, head.epoch).await;
    assert!(
        check(&server, &head, &attached, &policy, T0 + 30 * HOUR)
            .await
            .is_ok()
    );

    // Lookups carry older heads? No: a lookup's head is the newest epoch; older
    // heads are included but are not the newest.
    let older = server.head_at(3).await.unwrap();
    let attached = witnessed(&server, &note, 3).await;
    assert_eq!(
        check(&server, &older, &attached, &policy, T0 + 30 * HOUR).await,
        Err(KtError::NotNewest { epoch: 3, size: 6 })
    );
}

#[tokio::test]
async fn nothing_is_required_while_no_witness_is_configured() {
    let server = open().await;
    let head = server.latest_head().await.unwrap();
    let (_, note) = server.checkpoint(ORIGIN).await.unwrap();
    let attached = witnessed(&server, &note, head.epoch).await;
    // Only the log's signature: fine without witnesses (rule C0)...
    assert!(
        check(&server, &head, &attached, &WitnessPolicy::off(), T0)
            .await
            .is_ok()
    );
    // ...and "unavailable" with the auditor required (an unreachable witness).
    let auditor = auditor();
    assert!(matches!(
        check(&server, &head, &attached, &policy(&auditor), T0).await,
        Err(KtError::WitnessMissing(_))
    ));
}

#[tokio::test]
async fn a_large_history_is_submitted_in_steps() {
    let server = open().await;
    for n in 1..=9 {
        server
            .publish(vec![(acct(n), binding(n, 1))], T0 + u64::from(n))
            .await
            .unwrap();
    }
    let auditor = auditor();
    let mut state = None;
    let mut old_size = 0;
    while old_size < 10 {
        let request = server.add_epochs(ORIGIN, old_size, 4).await.unwrap();
        let accepted = add_epochs(state.as_ref(), &request).unwrap();
        old_size = accepted.state.size();
        state = Some(accepted.state);
    }
    let _ = auditor;
    assert_eq!(state.unwrap().size(), 10);
}

#[tokio::test]
async fn the_auditor_refuses_what_does_not_check_out() {
    let server = open().await;
    server
        .publish(vec![(acct(1), binding(1, 1))], T0 + 1)
        .await
        .unwrap();
    let request = server.add_epochs(ORIGIN, 0, 4096).await.unwrap();
    let accepted = add_epochs(None, &request).unwrap();
    let state = accepted.state;

    // A stale size: a conflict with the auditor's size.
    let mut stale = request.clone();
    stale.old_size = 1;
    assert_eq!(
        add_epochs(Some(&state), &stale).unwrap_err(),
        AddError::Conflict { size: 2 }
    );
    // Another log for the same origin (a server showing two logs).
    let other = open_with(Arc::new(MemoryStore::new()), [0x33; 32], VRF_SEED).await;
    let theirs = other
        .add_epochs(ORIGIN, 2.min(other.log_size().await.unwrap()), 1)
        .await;
    let theirs = theirs.unwrap();
    assert!(matches!(
        add_epochs(
            Some(&state),
            &AddEpochs {
                old_size: 2,
                ..theirs
            }
        ),
        Err(AddError::Forbidden(_))
    ));
    // A checkpoint signed by someone else.
    let mut forged = request.clone();
    let (_, note) = other.checkpoint(ORIGIN).await.unwrap();
    forged.checkpoint = note.into_bytes();
    assert!(matches!(
        add_epochs(None, &forged),
        Err(AddError::Forbidden(_))
    ));
    // Another origin in the checkpoint than in the request.
    let mut moved = request.clone();
    moved.origin = "elsewhere.example:1/nuschl-kt/v1".into();
    assert!(matches!(
        add_epochs(None, &moved),
        Err(AddError::Malformed(_))
    ));
    // A tampered akd proof, or roots that do not follow from it.
    let mut tampered = request.clone();
    let audit = tampered.audit.as_mut().unwrap();
    audit.roots[0][0] ^= 1;
    assert!(matches!(
        add_epochs(None, &tampered),
        Err(AddError::Invalid(_))
    ));
    // A checkpoint the epochs do not make.
    let mut late = request.clone();
    late.audit = None;
    assert!(matches!(add_epochs(None, &late), Err(AddError::Invalid(_))));
}

/// A server that shows one client a fork (another directory under the same keys).
#[tokio::test]
async fn a_fork_is_cosigned_by_nobody() {
    let honest = open().await;
    let fork = open().await;
    let auditor = auditor();
    let policy = policy(&auditor);
    honest
        .publish(vec![(acct(1), binding(1, 1))], T0 + 1)
        .await
        .unwrap();
    // The fork binds account 1 to the server's own identity instead.
    fork.publish(vec![(acct(1), binding(66, 1))], T0 + 1)
        .await
        .unwrap();
    let mut state = None;
    let note = submit(&honest, &auditor, &mut state, T0 + 1000)
        .await
        .unwrap();

    // The auditor holds the honest lineage; the fork cannot be cosigned.
    let request = fork.add_epochs(ORIGIN, 1, 4096).await.unwrap();
    assert!(add_epochs(state.as_ref(), &request).is_err());
    let request = fork.add_epochs(ORIGIN, 2, 4096).await.unwrap();
    assert!(add_epochs(state.as_ref(), &request).is_err());

    // Bob is shown the fork's head: with the fork's own checkpoint nobody cosigned...
    let head = fork.latest_head().await.unwrap();
    let (_, own) = fork.checkpoint(ORIGIN).await.unwrap();
    let attached = witnessed(&fork, &own, head.epoch).await;
    assert!(matches!(
        check(&fork, &head, &attached, &policy, T0 + 2000).await,
        Err(KtError::WitnessMissing(_))
    ));
    // ...or with the honest, cosigned checkpoint, which does not contain it.
    let mut borrowed = witnessed(&honest, &note, head.epoch).await;
    borrowed.inclusion = fork.inclusion(head.epoch, 2).await.unwrap();
    assert!(matches!(
        check(&fork, &head, &borrowed, &policy, T0 + 2000).await,
        Err(KtError::CheckpointInvalid(_))
    ));
}

#[tokio::test]
async fn stale_and_re_signed_heads_are_refused() {
    let server = open().await;
    let auditor = auditor();
    let policy = policy(&auditor);
    let mut state = None;
    server
        .publish(vec![(acct(1), binding(1, 1))], T0 + 1)
        .await
        .unwrap();
    let at_one = submit(&server, &auditor, &mut state, T0 + 1000)
        .await
        .unwrap();
    let frozen = server.latest_head().await.unwrap();

    // A stale head: the newest cosignature is older than the bound.
    let attached = witnessed(&server, &at_one, frozen.epoch).await;
    assert!(
        check(&server, &frozen, &attached, &policy, T0 + 23 * HOUR)
            .await
            .is_ok()
    );
    assert!(matches!(
        check(&server, &frozen, &attached, &policy, T0 + 25 * HOUR).await,
        Err(KtError::WitnessMissing(_))
    ));

    // The server moves on and keeps the auditor current...
    server
        .publish(vec![(acct(2), binding(2, 1))], T0 + 2 * HOUR)
        .await
        .unwrap();
    let at_two = submit(&server, &auditor, &mut state, T0 + 30 * HOUR)
        .await
        .unwrap();
    // ...but freezes Bob at epoch 1 by signing its old head again with a fresh time.
    let resigned = SignedHead::sign(
        &nuschl_kt::Ed25519HeadSigner::from_seed(&SIGNER_SEED),
        frozen.log_id,
        frozen.epoch,
        frozen.root,
        T0 + 30 * HOUR,
    )
    .unwrap();
    // With the old checkpoint, the cosignature is too old; the auditor will not
    // cosign the old size again (it holds a larger one).
    let attached = witnessed(&server, &at_one, resigned.epoch).await;
    assert!(matches!(
        check(&server, &resigned, &attached, &policy, T0 + 30 * HOUR).await,
        Err(KtError::WitnessMissing(_))
    ));
    let old = server.add_epochs(ORIGIN, 2, 0).await.unwrap();
    assert!(matches!(
        add_epochs(state.as_ref(), &old),
        Err(AddError::Conflict { size: 3 })
    ));
    // With the new checkpoint, the head is not its newest leaf.
    let attached = witnessed(&server, &at_two, resigned.epoch).await;
    assert_eq!(
        check(&server, &resigned, &attached, &policy, T0 + 30 * HOUR).await,
        Err(KtError::NotNewest { epoch: 1, size: 3 })
    );
}

#[tokio::test]
async fn checkpoints_for_another_origin_or_tampered_proofs_are_refused() {
    let server = open().await;
    let auditor = auditor();
    let policy = policy(&auditor);
    let mut state = None;
    server
        .publish(vec![(acct(1), binding(1, 1))], T0 + 1)
        .await
        .unwrap();
    let note = submit(&server, &auditor, &mut state, T0).await.unwrap();
    let head = server.latest_head().await.unwrap();
    let verified = verify_head(server.descriptor(), &head).unwrap();
    let attached = witnessed(&server, &note, head.epoch).await;
    // The client reaches the server under another name.
    assert!(matches!(
        check_witnessed(
            server.descriptor(),
            "alias.example.org:64738/nuschl-kt/v1",
            &verified,
            &attached,
            &policy,
            T0
        ),
        Err(KtError::CheckpointInvalid(_))
    ));
    let mut tampered = attached.clone();
    tampered.inclusion[0][3] ^= 1;
    assert!(matches!(
        check(&server, &head, &tampered, &policy, T0).await,
        Err(KtError::CheckpointInvalid(_))
    ));
    // A cosignature from the auditor's key that does not verify rejects the note.
    let mut broken = Note::parse(note.as_bytes()).unwrap();
    broken.signatures[1].signature[100] ^= 1;
    let mut attached_broken = attached.clone();
    attached_broken.checkpoint = broken.format().into_bytes();
    assert!(matches!(
        check(&server, &head, &attached_broken, &policy, T0).await,
        Err(KtError::CheckpointInvalid(_))
    ));
    // A cosignature from the future.
    assert!(matches!(
        check(&server, &head, &attached, &policy, T0 - 2 * HOUR).await,
        Err(KtError::WitnessMissing(_))
    ));
}

#[tokio::test]
async fn the_epoch_log_survives_a_restart_and_is_built_for_old_directories() {
    let store = Arc::new(MemoryStore::new());
    let server = open_with(store.clone(), SIGNER_SEED, VRF_SEED).await;
    for n in 1..=6 {
        server
            .publish(vec![(acct(n), binding(n, 1))], T0 + u64::from(n))
            .await
            .unwrap();
    }
    let (checkpoint, _) = server.checkpoint(ORIGIN).await.unwrap();
    drop(server);
    let again = open_with(store, SIGNER_SEED, VRF_SEED).await;
    assert_eq!(again.checkpoint(ORIGIN).await.unwrap().0, checkpoint);
    let proof = again.inclusion(2, 7).await.unwrap();
    assert!(!proof.is_empty());
}

#[tokio::test]
async fn a_clock_that_ran_ahead_does_not_stop_the_log() {
    let store = Arc::new(MemoryStore::new());
    let server = open_with(store.clone(), SIGNER_SEED, VRF_SEED).await;
    server
        .publish(vec![(acct(1), binding(1, 1))], T0 + 1000)
        .await
        .unwrap();
    // The clock jumps a day ahead: an epoch and a refresh are signed then.
    let ahead = T0 + 24 * HOUR;
    server
        .publish(vec![(acct(2), binding(2, 1))], ahead)
        .await
        .unwrap();
    server.refresh_head(ahead + HOUR).await.unwrap();
    // Back to real time: the next head is not in the future, and the stored heads
    // stay in order (no stale-signing evidence between any two of them).
    let now = T0 + 2 * HOUR;
    let head = server.refresh_head(now).await.unwrap();
    assert!(head.issued_at_ms <= now + HOUR, "{}", head.issued_at_ms);
    let published = server
        .publish(vec![(acct(3), binding(3, 1))], now + 1000)
        .await
        .unwrap();
    assert!(published.issued_at_ms <= now + HOUR);
    let mut previous = 0;
    for epoch in 0..=published.epoch {
        let head = server.head_at(epoch).await.unwrap();
        assert!(head.issued_at_ms >= previous, "epoch {epoch}");
        previous = head.issued_at_ms;
    }
    // A client with a correct clock accepts the heads again.
    let mut client = Client::new(server.descriptor());
    client
        .accept(&server, &published, now + 2000)
        .await
        .unwrap();
}

#[tokio::test]
async fn epoch_zero_is_the_same_empty_directory_under_every_key() {
    let a = open().await;
    let b = open_with(Arc::new(MemoryStore::new()), [0x44; 32], [0x55; 32]).await;
    assert_eq!(
        a.latest_head().await.unwrap().root,
        b.latest_head().await.unwrap().root
    );
}
