// SPDX-License-Identifier: EUPL-1.2
//! Fixtures shared by the integration tests.
#![allow(dead_code)]

use ed25519_dalek::SigningKey;
use nuschl_identity::HybridPublicKey;
use nuschl_kt::{
    AccountBinding, AccountId, AkdDirectory, Directory, DirectoryValue, Ed25519HeadSigner,
    KnownHeads, KtError, KtStore, Label, LogDescriptor, MemoryStore, Relation, RosterCommitment,
    SignedHead, VerifiedHead, VrfSecret, check_freshness, verify_audit, verify_head,
};
use std::sync::Arc;

/// 2026-09-21, in milliseconds.
pub const T0: u64 = 1_790_000_000_000;
pub const SIGNER_SEED: [u8; 32] = [0x11; 32];
pub const VRF_SEED: [u8; 32] = [0x22; 32];

pub type Dir = AkdDirectory<Arc<MemoryStore>>;

pub async fn open_with<S: KtStore>(store: S, signer: [u8; 32], vrf: [u8; 32]) -> AkdDirectory<S> {
    AkdDirectory::open(
        store,
        VrfSecret::from_bytes(vrf),
        Box::new(Ed25519HeadSigner::from_seed(&signer)),
        T0,
    )
    .await
    .expect("open the directory")
}

pub async fn open() -> Dir {
    open_with(Arc::new(MemoryStore::new()), SIGNER_SEED, VRF_SEED).await
}

pub fn account(n: u32) -> AccountId {
    let mut id = [0u8; 16];
    id[..4].copy_from_slice(&n.to_be_bytes());
    id[15] = 0xac;
    AccountId(id)
}

pub fn acct(n: u32) -> Label {
    Label::Account(account(n))
}

/// A well-formed hybrid public key, deterministic in `n` (the ML-DSA half is filler:
/// the directory never verifies with it).
pub fn root(n: u32) -> HybridPublicKey {
    let mut seed = [0u8; 32];
    seed[..4].copy_from_slice(&n.to_be_bytes());
    seed[31] = 0x5e;
    let ed = SigningKey::from_bytes(&seed).verifying_key();
    let mut bytes = Vec::with_capacity(HybridPublicKey::LEN);
    bytes.extend_from_slice(&0xFE01u16.to_be_bytes());
    bytes.extend((0..1952u32).map(|i| (i as u8) ^ (n as u8)));
    bytes.extend_from_slice(ed.as_bytes());
    HybridPublicKey::decode(&bytes).expect("valid hybrid key")
}

/// The `acct:` value binding identity `root_n` with roster number `roster_n`.
pub fn binding(root_n: u32, roster_n: u8) -> DirectoryValue {
    AccountBinding::new(root(root_n), RosterCommitment([roster_n; 32])).to_value()
}

/// A client's view of one log: what a crypto worker would do on each connection.
pub struct Client {
    pub descriptor: LogDescriptor,
    pub known: KnownHeads,
}

impl Client {
    pub fn new(descriptor: &LogDescriptor) -> Self {
        Self {
            descriptor: descriptor.clone(),
            known: KnownHeads::new(descriptor),
        }
    }

    /// Accepts a head served by the server: signature, freshness, monotonicity, and an
    /// audit proof from the latest known head if it advanced.
    pub async fn accept(
        &mut self,
        server: &impl Directory,
        head: &SignedHead,
        now_ms: u64,
    ) -> Result<VerifiedHead, KtError> {
        let head = verify_head(&self.descriptor, head)?;
        check_freshness(&head, now_ms, Default::default())?;
        match self.known.classify_from_server(&head)? {
            Relation::Newer { since } => {
                let audit = server
                    .audit(since.epoch(), head.epoch())
                    .await
                    .expect("audit");
                verify_audit(&self.descriptor, &since, &head, &audit)?;
            }
            Relation::First | Relation::Known | Relation::Older => {}
        }
        self.known.record(head.clone())?;
        Ok(head)
    }
}
