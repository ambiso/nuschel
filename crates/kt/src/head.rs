// SPDX-License-Identifier: EUPL-1.2
//! Signed epoch heads, their checks, and the client's record of heads it has seen.

use crate::codec::{DecodeError, Reader, Writer};
use crate::error::KtError;
use crate::log::{HeadSigner, LogDescriptor, LogId, SignerError};
use crate::{MSG_EVIDENCE, MSG_HEAD};
use std::cmp::Ordering;
use std::collections::BTreeMap;

/// Domain separation for head signatures.
const HEAD_CONTEXT: &[u8] = b"nuschl-kt/v1 signed head\0";
/// Largest epoch a head may name (one epoch every 10 s lasts 89 million years). Keeps
/// every version and epoch comparison far from integer overflow.
pub const MAX_EPOCH: u64 = 1 << 48;
/// Largest head signature (Ed25519). A hybrid scheme raises this.
pub const MAX_SIGNATURE_BYTES: usize = 64;
/// Encoded size of a head with an Ed25519 signature.
pub const HEAD_BYTES: usize = 2 + 32 + 8 + 32 + 8 + 2 + 64;

/// A server-signed commitment to the directory at one epoch.
///
/// ```text
/// struct {
///   uint8  version = 1;
///   uint8  type = 2;
///   opaque log_id[32];
///   uint64 epoch;              // <= 2^48
///   opaque root[32];           // the backend's root hash at `epoch`
///   uint64 issued_at_ms;       // Unix time of signing
///   opaque signature<0..64>;   // u16 length; over "nuschl-kt/v1 signed head\0" || all fields above
/// } SignedHead;                // 148 bytes with Ed25519
/// ```
///
/// The server re-signs its current epoch with a newer `issued_at_ms` while nothing
/// changes, so a fresh head always exists; heads for the same epoch must name the same
/// root. `issued_at_ms` never decreases across a log's heads, also across epochs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SignedHead {
    pub log_id: LogId,
    pub epoch: u64,
    pub root: [u8; 32],
    pub issued_at_ms: u64,
    pub signature: Vec<u8>,
}

impl SignedHead {
    /// Signs a head. The directory does this; it is public for servers with their own
    /// signing flow and for tests that play a malicious operator.
    pub fn sign(
        signer: &dyn HeadSigner,
        log_id: LogId,
        epoch: u64,
        root: [u8; 32],
        issued_at_ms: u64,
    ) -> Result<Self, SignerError> {
        let mut head = Self {
            log_id,
            epoch,
            root,
            issued_at_ms,
            signature: Vec::new(),
        };
        head.signature = signer.sign(&head.to_be_signed())?;
        Ok(head)
    }

    fn write_signed_part(&self, w: &mut Writer) {
        w.header(MSG_HEAD);
        w.raw(&self.log_id.0);
        w.u64(self.epoch);
        w.raw(&self.root);
        w.u64(self.issued_at_ms);
    }

    fn to_be_signed(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.raw(HEAD_CONTEXT);
        self.write_signed_part(&mut w);
        w.finish()
    }

    pub(crate) fn write(&self, w: &mut Writer) {
        self.write_signed_part(w);
        // Signatures come from a signer of a known scheme; an oversized one would have
        // failed verification on the server already.
        let _ = w.bytes_u16("signature", &self.signature, MAX_SIGNATURE_BYTES);
    }

    pub(crate) fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        r.header(MSG_HEAD)?;
        let log_id = LogId(r.array("head.log_id")?);
        let epoch = r.u64("head.epoch")?;
        if epoch > MAX_EPOCH {
            return Err(DecodeError::InvalidValue {
                field: "head.epoch",
            });
        }
        let root = r.array("head.root")?;
        let issued_at_ms = r.u64("head.issued_at_ms")?;
        let signature = r.bytes_u16("head.signature", MAX_SIGNATURE_BYTES)?.to_vec();
        Ok(Self {
            log_id,
            epoch,
            root,
            issued_at_ms,
            signature,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        self.write(&mut w);
        w.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(bytes, HEAD_BYTES)?;
        let head = Self::read(&mut r)?;
        r.finish()?;
        Ok(head)
    }
}

/// A head whose signature and log id were checked against a pinned descriptor. Only
/// [`verify_head`] creates one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct VerifiedHead(SignedHead);

impl VerifiedHead {
    pub fn epoch(&self) -> u64 {
        self.0.epoch
    }
    pub fn root(&self) -> &[u8; 32] {
        &self.0.root
    }
    pub fn issued_at_ms(&self) -> u64 {
        self.0.issued_at_ms
    }
    pub fn log_id(&self) -> LogId {
        self.0.log_id
    }
    pub fn signed(&self) -> &SignedHead {
        &self.0
    }
}

/// Checks that `head` belongs to the pinned log and carries a valid signature.
pub fn verify_head(descriptor: &LogDescriptor, head: &SignedHead) -> Result<VerifiedHead, KtError> {
    let expected = descriptor.log_id();
    if head.log_id != expected {
        return Err(KtError::WrongLog {
            expected,
            got: head.log_id,
        });
    }
    if head.epoch > MAX_EPOCH {
        return Err(KtError::Malformed(DecodeError::InvalidValue {
            field: "head.epoch",
        }));
    }
    if !descriptor
        .signing_key()
        .verify(&head.to_be_signed(), &head.signature)
    {
        return Err(KtError::BadSignature);
    }
    Ok(VerifiedHead(head.clone()))
}

/// How old and how far in the future a head may be (docs/e2ee.md §5.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FreshnessPolicy {
    /// A head older than this is stale: 25 hours, so an hourly re-signing server with a
    /// day of slack passes.
    pub max_age_ms: u64,
    /// Tolerated clock difference for heads from the future: 1 hour.
    pub max_future_ms: u64,
}

impl Default for FreshnessPolicy {
    fn default() -> Self {
        Self {
            max_age_ms: 25 * 3600 * 1000,
            max_future_ms: 3600 * 1000,
        }
    }
}

/// Checks the head's age against the local clock.
pub fn check_freshness(
    head: &VerifiedHead,
    now_ms: u64,
    policy: FreshnessPolicy,
) -> Result<(), KtError> {
    let issued = head.issued_at_ms();
    if issued > now_ms.saturating_add(policy.max_future_ms) {
        return Err(KtError::HeadFromFuture {
            ahead_ms: issued - now_ms,
        });
    }
    let age = now_ms.saturating_sub(issued);
    if age > policy.max_age_ms {
        return Err(KtError::StaleHead { age_ms: age });
    }
    Ok(())
}

/// Orders two heads of the same log and checks that they can both be honest: the same
/// epoch must name the same root, and signing times must follow epochs. Contradictions
/// are returned as transferable [`Evidence`].
pub fn compare_heads(a: &VerifiedHead, b: &VerifiedHead) -> Result<Ordering, KtError> {
    if a.log_id() != b.log_id() {
        return Err(KtError::WrongLog {
            expected: a.log_id(),
            got: b.log_id(),
        });
    }
    match a.epoch().cmp(&b.epoch()) {
        Ordering::Equal => {
            if a.root() == b.root() {
                Ok(Ordering::Equal)
            } else {
                Err(KtError::Equivocation(Box::new(Evidence {
                    kind: EvidenceKind::Equivocation,
                    first: a.0.clone(),
                    second: b.0.clone(),
                })))
            }
        }
        Ordering::Less => stale_signing(a, b).map(|()| Ordering::Less),
        Ordering::Greater => stale_signing(b, a).map(|()| Ordering::Greater),
    }
}

/// `older.epoch < newer.epoch`: the older epoch must not have been signed after the
/// newer one. If it was, the server was handing out an outdated head while a newer
/// epoch existed (hiding updates from someone).
fn stale_signing(older: &VerifiedHead, newer: &VerifiedHead) -> Result<(), KtError> {
    if older.issued_at_ms() > newer.issued_at_ms() {
        Err(KtError::StaleSigning(Box::new(Evidence {
            kind: EvidenceKind::StaleSigning,
            first: older.0.clone(),
            second: newer.0.clone(),
        })))
    } else {
        Ok(())
    }
}

/// How a head from the server relates to the latest head the client accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Succession {
    /// The same epoch and root (possibly re-signed later).
    Same,
    /// A newer epoch: verify an audit proof from `latest` before accepting it.
    Advanced,
}

/// Monotonicity for heads served by the server itself: never an older epoch.
pub fn check_successor(latest: &VerifiedHead, next: &VerifiedHead) -> Result<Succession, KtError> {
    match compare_heads(latest, next)? {
        Ordering::Equal => Ok(Succession::Same),
        Ordering::Less => Ok(Succession::Advanced),
        Ordering::Greater => Err(KtError::Rollback {
            latest: latest.epoch(),
            got: next.epoch(),
        }),
    }
}

/// What a contradiction between two signed heads shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum EvidenceKind {
    /// Two heads for the same epoch with different roots: the log was forked.
    Equivocation = 1,
    /// `first` names an older epoch than `second` but was signed later.
    StaleSigning = 2,
}

/// Two server-signed heads that cannot both be honest. Anyone holding the log's
/// descriptor can check it, so clients keep it and can show it to an auditor or the
/// operator.
///
/// `struct { uint8 version = 1; uint8 type = 7; uint8 kind; SignedHead first; SignedHead second; }`
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Evidence {
    pub kind: EvidenceKind,
    pub first: SignedHead,
    pub second: SignedHead,
}

impl Evidence {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.header(MSG_EVIDENCE);
        w.u8(self.kind as u8);
        self.first.write(&mut w);
        self.second.write(&mut w);
        w.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(bytes, 3 + 2 * HEAD_BYTES)?;
        r.header(MSG_EVIDENCE)?;
        let kind = match r.u8("evidence.kind")? {
            1 => EvidenceKind::Equivocation,
            2 => EvidenceKind::StaleSigning,
            _ => {
                return Err(DecodeError::InvalidValue {
                    field: "evidence.kind",
                });
            }
        };
        let first = SignedHead::read(&mut r)?;
        let second = SignedHead::read(&mut r)?;
        r.finish()?;
        Ok(Self {
            kind,
            first,
            second,
        })
    }

    /// Checks both signatures and that the heads really contradict each other.
    pub fn verify(&self, descriptor: &LogDescriptor) -> Result<(), KtError> {
        let first = verify_head(descriptor, &self.first)?;
        let second = verify_head(descriptor, &self.second)?;
        let holds = match self.kind {
            EvidenceKind::Equivocation => {
                first.epoch() == second.epoch() && first.root() != second.root()
            }
            EvidenceKind::StaleSigning => {
                first.epoch() < second.epoch() && first.issued_at_ms() > second.issued_at_ms()
            }
        };
        if holds {
            Ok(())
        } else {
            Err(KtError::Malformed(DecodeError::InvalidValue {
                field: "evidence",
            }))
        }
    }
}

/// How a verified head relates to the heads a client knows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Relation {
    /// Nothing known yet: the first head (trust on first use of the log state).
    First,
    /// Consistent with a recorded head of the same epoch.
    Known,
    /// Consistent, and older than the latest accepted head.
    Older,
    /// Newer than the latest accepted head: verify an audit proof from `since`, then
    /// [`KnownHeads::record`] it.
    Newer { since: VerifiedHead },
}

/// The heads a client has accepted for one log, kept to detect forks: the latest head
/// (the base for append-only checks) and a bounded set of recent ones, from the server
/// and from gossip. The crypto worker persists it with [`KnownHeads::heads`] and
/// restores it with [`KnownHeads::restore`].
#[derive(Clone, Debug)]
pub struct KnownHeads {
    log_id: LogId,
    heads: BTreeMap<u64, VerifiedHead>,
    capacity: usize,
}

impl KnownHeads {
    /// Default number of heads kept.
    pub const DEFAULT_CAPACITY: usize = 64;

    pub fn new(descriptor: &LogDescriptor) -> Self {
        Self {
            log_id: descriptor.log_id(),
            heads: BTreeMap::new(),
            capacity: Self::DEFAULT_CAPACITY,
        }
    }

    /// Rebuilds the record from persisted heads, re-verifying each.
    pub fn restore(descriptor: &LogDescriptor, heads: &[SignedHead]) -> Result<Self, KtError> {
        let mut known = Self::new(descriptor);
        let mut verified = heads
            .iter()
            .map(|h| verify_head(descriptor, h))
            .collect::<Result<Vec<_>, _>>()?;
        verified.sort_by_key(VerifiedHead::epoch);
        for head in verified {
            known.record(head)?;
        }
        Ok(known)
    }

    pub fn latest(&self) -> Option<&VerifiedHead> {
        self.heads.values().next_back()
    }

    pub fn heads(&self) -> impl Iterator<Item = &VerifiedHead> {
        self.heads.values()
    }

    /// Compares `head` with every recorded head. Pure: records nothing.
    pub fn classify(&self, head: &VerifiedHead) -> Result<Relation, KtError> {
        if head.log_id() != self.log_id {
            return Err(KtError::WrongLog {
                expected: self.log_id,
                got: head.log_id(),
            });
        }
        for known in self.heads.values() {
            compare_heads(known, head)?;
        }
        let Some(latest) = self.latest() else {
            return Ok(Relation::First);
        };
        Ok(if self.heads.contains_key(&head.epoch()) {
            Relation::Known
        } else if head.epoch() > latest.epoch() {
            Relation::Newer {
                since: latest.clone(),
            }
        } else {
            Relation::Older
        })
    }

    /// For heads from the server: like [`Self::classify`], but an older epoch than the
    /// latest accepted one is a rollback.
    pub fn classify_from_server(&self, head: &VerifiedHead) -> Result<Relation, KtError> {
        match self.classify(head)? {
            Relation::Older => Err(KtError::Rollback {
                latest: self.latest().map_or(0, VerifiedHead::epoch),
                got: head.epoch(),
            }),
            Relation::Known
                if self
                    .latest()
                    .is_some_and(|latest| head.epoch() < latest.epoch()) =>
            {
                Err(KtError::Rollback {
                    latest: self.latest().map_or(0, VerifiedHead::epoch),
                    got: head.epoch(),
                })
            }
            other => Ok(other),
        }
    }

    /// Records a head. For a [`Relation::Newer`] head, call this only after its audit
    /// proof verified. Of two heads for the same epoch the more recently signed is kept.
    pub fn record(&mut self, head: VerifiedHead) -> Result<(), KtError> {
        self.classify(&head)?;
        match self.heads.get(&head.epoch()) {
            Some(existing) if existing.issued_at_ms() >= head.issued_at_ms() => {}
            _ => {
                self.heads.insert(head.epoch(), head);
            }
        }
        while self.heads.len() > self.capacity {
            self.heads.pop_first();
        }
        Ok(())
    }
}
