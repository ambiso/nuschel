// SPDX-License-Identifier: EUPL-1.2
//! `Debug` for the generated messages that carry secrets or private content:
//! passwords, access tokens (channel passwords), voice keys, comments and
//! message bodies show only whether they are present and how long they are.
use super::{Authenticate, CryptSetup, TextMessage, UserState, acl::ChanAcl};
use std::fmt;

struct Redacted(Option<usize>);

impl fmt::Debug for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(length) => write!(f, "<{length} bytes redacted>"),
            None => f.write_str("None"),
        }
    }
}

fn text(value: &Option<String>) -> Redacted {
    Redacted(value.as_ref().map(String::len))
}

fn bytes(value: &Option<Vec<u8>>) -> Redacted {
    Redacted(value.as_ref().map(Vec::len))
}

fn count(values: &[String]) -> String {
    format!("<{} redacted>", values.len())
}

impl fmt::Debug for Authenticate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Authenticate")
            .field("username", &self.username)
            .field("password", &text(&self.password))
            .field("tokens", &count(&self.tokens))
            .field("celt_versions", &self.celt_versions)
            .field("opus", &self.opus)
            .field("client_type", &self.client_type)
            .finish()
    }
}

impl fmt::Debug for CryptSetup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CryptSetup")
            .field("key", &bytes(&self.key))
            .field("client_nonce", &bytes(&self.client_nonce))
            .field("server_nonce", &bytes(&self.server_nonce))
            .finish()
    }
}

impl fmt::Debug for UserState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UserState")
            .field("session", &self.session)
            .field("actor", &self.actor)
            .field("name", &self.name)
            .field("user_id", &self.user_id)
            .field("channel_id", &self.channel_id)
            .field("mute", &self.mute)
            .field("deaf", &self.deaf)
            .field("suppress", &self.suppress)
            .field("self_mute", &self.self_mute)
            .field("self_deaf", &self.self_deaf)
            .field("texture", &bytes(&self.texture))
            .field("plugin_context", &bytes(&self.plugin_context))
            .field("plugin_identity", &text(&self.plugin_identity))
            .field("comment", &text(&self.comment))
            .field("hash", &self.hash)
            .field("priority_speaker", &self.priority_speaker)
            .field("recording", &self.recording)
            .field(
                "temporary_access_tokens",
                &count(&self.temporary_access_tokens),
            )
            .field("listening_channel_add", &self.listening_channel_add)
            .field("listening_channel_remove", &self.listening_channel_remove)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for TextMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TextMessage")
            .field("actor", &self.actor)
            .field("session", &self.session)
            .field("channel_id", &self.channel_id)
            .field("tree_id", &self.tree_id)
            .field("message", &Redacted(Some(self.message.len())))
            .finish()
    }
}

impl fmt::Debug for ChanAcl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A group of "#name" is an access token, i.e. a channel password.
        f.debug_struct("ChanAcl")
            .field("apply_here", &self.apply_here)
            .field("apply_subs", &self.apply_subs)
            .field("inherited", &self.inherited)
            .field("user_id", &self.user_id)
            .field("group", &text(&self.group))
            .field("grant", &self.grant)
            .field("deny", &self.deny)
            .finish()
    }
}
