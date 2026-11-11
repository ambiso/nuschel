// SPDX-License-Identifier: EUPL-1.2
//! Signed heads that members exchange inside MLS application messages (docs/e2ee.md
//! §5.3). This module defines the payload and its checks; the crypto worker carries it
//! as the `kt_head` field of an application message.

use crate::MSG_GOSSIP;
use crate::codec::{DecodeError, Reader, Writer};
use crate::error::KtError;
use crate::head::{HEAD_BYTES, KnownHeads, Relation, SignedHead, VerifiedHead, verify_head};
use crate::log::LogDescriptor;

/// Encoded size of a [`HeadGossip`] with an Ed25519 head.
pub const GOSSIP_BYTES: usize = 2 + HEAD_BYTES;
/// Gossip at most this often per conversation (docs/e2ee.md §5.3: once a day).
pub const GOSSIP_INTERVAL_MS: u64 = 24 * 3600 * 1000;

/// `struct { uint8 version = 1; uint8 type = 6; SignedHead head; }` — 150 bytes.
///
/// The conversation is bound to one server, so the receiver checks the head against
/// that server's pinned descriptor. The MLS sender is authenticated by MLS; the head is
/// authenticated by the server's signature, so a member cannot forge one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HeadGossip {
    pub head: SignedHead,
}

impl HeadGossip {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.header(MSG_GOSSIP);
        self.head.write(&mut w);
        w.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(bytes, GOSSIP_BYTES)?;
        r.header(MSG_GOSSIP)?;
        let head = SignedHead::read(&mut r)?;
        r.finish()?;
        Ok(Self { head })
    }
}

/// What a received head means for the receiver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GossipOutcome {
    /// Matches a head the receiver already holds.
    Known,
    /// Older than the receiver's latest head and consistent with what it holds. Nothing
    /// to do; [`KnownHeads::record`] it to widen the fork check.
    Older,
    /// Newer than anything the receiver accepted (or the first head it sees for this
    /// log): ask the server for an audit proof from `since` to this head
    /// ([`crate::verify_audit`]) and record it once that passes. A server that cannot
    /// produce one has forked the log.
    Newer { since: Option<VerifiedHead> },
}

/// Checks a received head against the conversation server's pinned descriptor and the
/// heads the receiver knows.
///
/// Errors and what they mean here:
/// - [`KtError::Equivocation`] / [`KtError::StaleSigning`]: the *server* forked or
///   withheld; alarm and keep the evidence.
/// - [`KtError::Malformed`], [`KtError::WrongLog`], [`KtError::BadSignature`]: the
///   *sender* sent garbage; drop it (heads cannot be forged by members).
pub fn check_gossip(
    descriptor: &LogDescriptor,
    known: &KnownHeads,
    gossip: &HeadGossip,
) -> Result<(VerifiedHead, GossipOutcome), KtError> {
    let head = verify_head(descriptor, &gossip.head)?;
    let outcome = match known.classify(&head)? {
        Relation::First => GossipOutcome::Newer { since: None },
        Relation::Known => GossipOutcome::Known,
        Relation::Older => GossipOutcome::Older,
        Relation::Newer { since } => GossipOutcome::Newer { since: Some(since) },
    };
    Ok((head, outcome))
}

/// Whether to attach `latest` to an outgoing application message in a conversation:
/// at most once per [`GOSSIP_INTERVAL_MS`], and only if it is newer than the newest
/// head seen in that conversation (sent or received).
pub fn should_gossip(
    latest: &VerifiedHead,
    newest_seen_in_conversation: Option<u64>,
    last_gossip_at_ms: Option<u64>,
    now_ms: u64,
) -> bool {
    let newer = newest_seen_in_conversation.is_none_or(|epoch| latest.epoch() > epoch);
    let due = last_gossip_at_ms.is_none_or(|t| now_ms.saturating_sub(t) >= GOSSIP_INTERVAL_MS);
    newer && due
}
