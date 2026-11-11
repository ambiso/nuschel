// SPDX-License-Identifier: EUPL-1.2
//! Per-conversation security state kept by the client (docs/e2ee.md, "Downgrade
//! protection"). The client's own record decides whether a conversation is end-to-end
//! encrypted, never the server's advertisement:
//!
//! - once a conversation has an MLS group, plaintext for it is refused;
//! - a Welcome (or external commit) for a pinned, active conversation is refused
//!   unless it is a newer generation announced inside the current group;
//! - a binding for another server is refused.

use std::collections::HashMap;

use crate::ConversationBinding;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pin {
    pub server: [u8; 32],
    pub generation: u32,
    pub active: bool,
    /// A generation the current group announced (signed by a member, in an MLS
    /// application message) as its replacement.
    pub announced: Option<u32>,
}

pub struct ConversationPins {
    server: [u8; 32],
    pins: HashMap<[u8; 16], Pin>,
}

impl ConversationPins {
    pub fn new(server: [u8; 32]) -> Self {
        Self {
            server,
            pins: HashMap::new(),
        }
    }

    pub fn get(&self, conversation: &[u8; 16]) -> Option<&Pin> {
        self.pins.get(conversation)
    }

    pub fn check_welcome(&self, binding: &ConversationBinding) -> Result<(), String> {
        if binding.server != self.server {
            return Err("group bound to another server".into());
        }
        match self.pins.get(&binding.conversation) {
            None => Ok(()),
            Some(pin) if !pin.active => {
                // Removed earlier: rejoining is allowed, but shown ("you were re-added").
                if binding.generation >= pin.generation {
                    Ok(())
                } else {
                    Err("older generation than the pinned group".into())
                }
            }
            Some(pin) if pin.announced == Some(binding.generation) => Ok(()),
            Some(_) => Err("would silently replace an active end-to-end group".into()),
        }
    }

    pub fn pin(&mut self, binding: &ConversationBinding) {
        self.pins.insert(
            binding.conversation,
            Pin {
                server: binding.server,
                generation: binding.generation,
                active: true,
                announced: None,
            },
        );
    }

    pub fn announce_replacement(&mut self, conversation: &[u8; 16], generation: u32) {
        if let Some(pin) = self.pins.get_mut(conversation) {
            pin.announced = Some(generation);
        }
    }

    pub fn removed(&mut self, conversation: &[u8; 16]) {
        if let Some(pin) = self.pins.get_mut(conversation) {
            pin.active = false;
        }
    }

    /// The server offers this conversation as plaintext (no MLS): allowed only if
    /// it was never end-to-end encrypted.
    pub fn check_plaintext(&self, conversation: &[u8; 16]) -> Result<(), String> {
        match self.pins.get(conversation) {
            None => Ok(()),
            Some(_) => Err("end-to-end conversation offered as plaintext".into()),
        }
    }
}
