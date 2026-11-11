// SPDX-License-Identifier: EUPL-1.2
//! The conversation binding (docs/e2ee.md §6.1): a group context extension
//! (private-use 0xF4E0) naming the server, the conversation and the generation, so a
//! group can never be presented as another conversation's or another server's.
//!
//! ```text
//! struct {
//!   uint8  version = 1;
//!   opaque server[32];        // the server's key-transparency log id (hashes its
//!                             // identity key and VRF key, docs/e2ee.md §5.1)
//!   opaque conversation[16];  // the server-assigned ConversationId
//!   uint32 generation;        // +1 when a conversation's group is replaced (§1.5)
//!   uint8  kind;              // 1 direct conversation; 2 channel (4c)
//! } ConversationBinding;      // 54 bytes
//! ```
//!
//! The group id is `conversation || generation` (20 bytes), so a replacement group
//! never reuses an id. The policy fields of §6.1 (history, membership mode, admins)
//! join as version 2 with channels; a direct conversation's policy is fixed
//! (membership `members`, only the two identities' devices).

/// The binding's group context extension type.
pub const BINDING_EXTENSION: u16 = 0xF4E0;

/// What kind of conversation a group belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConversationKind {
    Direct,
    Channel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ConversationBinding {
    pub server: [u8; 32],
    pub conversation: [u8; 16],
    pub generation: u32,
    pub kind: ConversationKind,
}

impl ConversationBinding {
    pub const LEN: usize = 54;

    pub fn direct(server: [u8; 32], conversation: [u8; 16]) -> Self {
        Self {
            server,
            conversation,
            generation: 0,
            kind: ConversationKind::Direct,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::LEN);
        out.push(1);
        out.extend_from_slice(&self.server);
        out.extend_from_slice(&self.conversation);
        out.extend_from_slice(&self.generation.to_be_bytes());
        out.push(match self.kind {
            ConversationKind::Direct => 1,
            ConversationKind::Channel => 2,
        });
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != Self::LEN || bytes[0] != 1 {
            return None;
        }
        let kind = match bytes[53] {
            1 => ConversationKind::Direct,
            2 => ConversationKind::Channel,
            _ => return None,
        };
        Some(Self {
            server: bytes[1..33].try_into().ok()?,
            conversation: bytes[33..49].try_into().ok()?,
            generation: u32::from_be_bytes(bytes[49..53].try_into().ok()?),
            kind,
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
            server: [7; 32],
            conversation: [1; 16],
            generation: 3,
            kind: ConversationKind::Channel,
        };
        let bytes = binding.encode();
        assert_eq!(bytes.len(), ConversationBinding::LEN);
        assert_eq!(ConversationBinding::decode(&bytes), Some(binding));
        assert_eq!(parse_group_id(&binding.group_id()), Some(([1; 16], 3)));
        let mut bad = bytes.clone();
        bad[53] = 9;
        assert_eq!(ConversationBinding::decode(&bad), None);
        assert_eq!(ConversationBinding::decode(&bytes[1..]), None);
    }
}
