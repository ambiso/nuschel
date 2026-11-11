// SPDX-License-Identifier: EUPL-1.2
//! The conversation binding (docs/e2ee.md §6.1): a group context extension
//! (private-use 0xF4E0) naming the server, the conversation, the generation and the
//! conversation's policy, so a group can never be presented as another
//! conversation's or another server's, and the server cannot widen the policy.
//!
//! ```text
//! struct {
//!   uint8  version = 2;
//!   opaque server[32];        // the server's key-transparency log id (hashes its
//!                             // identity key and VRF key, docs/e2ee.md §5.1)
//!   opaque conversation[16];  // the server-assigned ConversationId
//!   uint32 generation;        // +1 when a conversation's group is replaced (§1.5)
//!   uint8  kind;              // 1 direct conversation; 2 channel
//!   uint8  history;           // 1 members (every admitted member is granted the
//!                             // retained history keys); 2 since_join (only the
//!                             // identity's own new devices are)
//!   uint8  membership;        // 1 server (adds and removals follow the server's
//!                             // ACLs); 2 members (only removals do)
//! } ConversationBinding;      // 56 bytes
//! ```
//!
//! Version 1 (the prototype's, 54 bytes, without the two policy fields) still
//! decodes, as a direct conversation with `members` history and membership. The
//! group id is `conversation || generation` (20 bytes), so a replacement group never
//! reuses an id. Policy admins (§6.1) are not carried yet: nobody can change the
//! policy of a group once it exists.

/// The binding's group context extension type.
pub const BINDING_EXTENSION: u16 = 0xF4E0;

/// What kind of conversation a group belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConversationKind {
    Direct,
    Channel,
}

/// Who new members may read history from (docs/e2ee.md §6.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HistoryPolicy {
    /// Every admitted member is granted the retained history (the default, the
    /// user's answer 5).
    Members,
    /// Only the identity's own new devices are.
    SinceJoin,
}

/// Who decides membership (docs/e2ee.md §6.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Membership {
    /// Channels: devices join by external commit when the server's ACLs let them
    /// read, and the server proposes removals.
    Server,
    /// Direct conversations: members add; only removals follow the server.
    Members,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ConversationBinding {
    pub server: [u8; 32],
    pub conversation: [u8; 16],
    pub generation: u32,
    pub kind: ConversationKind,
    pub history: HistoryPolicy,
    pub membership: Membership,
}

impl ConversationBinding {
    pub const LEN: usize = 56;
    const LEN_V1: usize = 54;

    pub fn direct(server: [u8; 32], conversation: [u8; 16]) -> Self {
        Self {
            server,
            conversation,
            generation: 0,
            kind: ConversationKind::Direct,
            history: HistoryPolicy::Members,
            membership: Membership::Members,
        }
    }

    /// A channel: server-driven membership, history for every admitted member.
    pub fn channel(server: [u8; 32], conversation: [u8; 16]) -> Self {
        Self {
            server,
            conversation,
            generation: 0,
            kind: ConversationKind::Channel,
            history: HistoryPolicy::Members,
            membership: Membership::Server,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::LEN);
        out.push(2);
        out.extend_from_slice(&self.server);
        out.extend_from_slice(&self.conversation);
        out.extend_from_slice(&self.generation.to_be_bytes());
        out.push(match self.kind {
            ConversationKind::Direct => 1,
            ConversationKind::Channel => 2,
        });
        out.push(match self.history {
            HistoryPolicy::Members => 1,
            HistoryPolicy::SinceJoin => 2,
        });
        out.push(match self.membership {
            Membership::Server => 1,
            Membership::Members => 2,
        });
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let version = *bytes.first()?;
        let expected = match version {
            1 => Self::LEN_V1,
            2 => Self::LEN,
            _ => return None,
        };
        if bytes.len() != expected {
            return None;
        }
        let kind = match bytes[53] {
            1 => ConversationKind::Direct,
            2 => ConversationKind::Channel,
            _ => return None,
        };
        let (history, membership) = if version == 1 {
            // The prototype's direct conversations; it never made channel groups.
            if kind != ConversationKind::Direct {
                return None;
            }
            (HistoryPolicy::Members, Membership::Members)
        } else {
            let history = match bytes[54] {
                1 => HistoryPolicy::Members,
                2 => HistoryPolicy::SinceJoin,
                _ => return None,
            };
            let membership = match bytes[55] {
                1 => Membership::Server,
                2 => Membership::Members,
                _ => return None,
            };
            (history, membership)
        };
        // A direct conversation never takes server-driven adds (§6.2).
        if kind == ConversationKind::Direct && membership == Membership::Server {
            return None;
        }
        Some(Self {
            server: bytes[1..33].try_into().ok()?,
            conversation: bytes[33..49].try_into().ok()?,
            generation: u32::from_be_bytes(bytes[49..53].try_into().ok()?),
            kind,
            history,
            membership,
        })
    }

    /// The MLS group id: conversation id and generation.
    pub fn group_id(&self) -> Vec<u8> {
        group_id(&self.conversation, self.generation)
    }
}

/// `conversation || generation`.
pub fn group_id(conversation: &[u8; 16], generation: u32) -> Vec<u8> {
    let mut id = conversation.to_vec();
    id.extend_from_slice(&generation.to_be_bytes());
    id
}

/// The conversation and generation a group id names.
pub fn parse_group_id(id: &[u8]) -> Option<([u8; 16], u32)> {
    if id.len() != 20 {
        return None;
    }
    Some((
        id[..16].try_into().ok()?,
        u32::from_be_bytes(id[16..].try_into().ok()?),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bindings_round_trip_and_name_their_group() {
        let binding = ConversationBinding {
            generation: 3,
            ..ConversationBinding::channel([7; 32], [1; 16])
        };
        let bytes = binding.encode();
        assert_eq!(bytes.len(), ConversationBinding::LEN);
        assert_eq!(ConversationBinding::decode(&bytes), Some(binding));
        assert_eq!(parse_group_id(&binding.group_id()), Some(([1; 16], 3)));
        let mut bad = bytes.clone();
        bad[53] = 9;
        assert_eq!(ConversationBinding::decode(&bad), None);
        assert_eq!(ConversationBinding::decode(&bytes[1..]), None);
        // A direct conversation with server-driven membership does not exist.
        let mut widened = ConversationBinding::direct([7; 32], [1; 16]).encode();
        widened[55] = 1;
        assert_eq!(ConversationBinding::decode(&widened), None);
        // The prototype's version 1 decodes as a direct conversation.
        let mut v1 = ConversationBinding::direct([7; 32], [1; 16]).encode();
        v1.truncate(54);
        v1[0] = 1;
        assert_eq!(
            ConversationBinding::decode(&v1),
            Some(ConversationBinding::direct([7; 32], [1; 16]))
        );
    }
}
