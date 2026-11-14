// SPDX-License-Identifier: EUPL-1.2
use std::collections::BTreeMap;

/// Channel IDs are scoped to a trusted server identity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Conversation {
    pub server: String,
    pub channel: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistorySource {
    /// Messages observed by this client, including while using legacy servers.
    LocalObserved,
    /// Only after an authenticated Nuschl history capability has been negotiated.
    ServerArchive,
}

/// Session evidence, never a server-advertised "encryption enabled" flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionSecurity {
    Unknown,
    /// Transport encryption is not end-to-end encryption: the server can read content.
    TransportOnly,
    MlsPending,
    /// Reserved for an authenticated MLS session; no adapter establishes one yet.
    MlsEstablished,
}

/// The authenticated scope in which a page was requested. Transport retries
/// keep the driver generation, so pages also carry a client page epoch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PageOwner {
    pub generation: u64,
    pub epoch: u64,
    pub archive: Option<[u8; 16]>,
    pub identity: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryTicket {
    pub conversation: Conversation,
    pub source: HistorySource,
    generation: u64,
    pub(crate) owner: Option<PageOwner>,
    pub(crate) catchup: bool,
}

#[derive(Default)]
pub struct Navigation {
    selection: Option<Conversation>,
    voice: Option<Conversation>,
    remembered: BTreeMap<String, u32>,
    generation: u64,
}

impl Navigation {
    /// Browsing text never joins or moves a voice session.
    pub fn browse(&mut self, conversation: Conversation, source: HistorySource) -> HistoryTicket {
        self.generation = self.generation.wrapping_add(1);
        self.remembered
            .insert(conversation.server.clone(), conversation.channel);
        self.selection = Some(conversation.clone());
        HistoryTicket {
            conversation,
            source,
            generation: self.generation,
            owner: None,
            catchup: false,
        }
    }

    pub fn selection(&self) -> Option<&Conversation> {
        self.selection.as_ref()
    }

    pub fn remembered_channel(&self, server: &str) -> Option<u32> {
        self.remembered.get(server).copied()
    }

    /// Ignore late history responses after switching channels, including away and back.
    pub fn accepts(&self, ticket: &HistoryTicket) -> bool {
        self.generation == ticket.generation
            && self.selection.as_ref() == Some(&ticket.conversation)
    }

    /// Only an explicit voice action and its session adapter use this setter.
    pub fn set_voice(&mut self, conversation: Option<Conversation>) {
        self.voice = conversation;
    }

    pub fn voice(&self) -> Option<&Conversation> {
        self.voice.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(server: &str, channel: u32) -> Conversation {
        Conversation {
            server: server.into(),
            channel,
        }
    }

    #[test]
    fn browsing_remembers_each_server_without_moving_voice() {
        let mut navigation = Navigation::default();
        navigation.set_voice(Some(channel("one", 1)));
        navigation.browse(channel("one", 2), HistorySource::ServerArchive);
        navigation.browse(channel("two", 9), HistorySource::LocalObserved);
        assert_eq!(navigation.voice(), Some(&channel("one", 1)));
        assert_eq!(navigation.selection(), Some(&channel("two", 9)));
        assert_eq!(navigation.remembered_channel("one"), Some(2));
        assert_eq!(navigation.remembered_channel("two"), Some(9));
    }

    #[test]
    fn late_history_cannot_replace_a_new_selection_or_reload() {
        let mut navigation = Navigation::default();
        let old = navigation.browse(channel("one", 1), HistorySource::ServerArchive);
        let other = navigation.browse(channel("two", 1), HistorySource::LocalObserved);
        assert!(!navigation.accepts(&old));
        assert!(navigation.accepts(&other));
        let current = navigation.browse(channel("one", 1), HistorySource::ServerArchive);
        assert!(!navigation.accepts(&old));
        assert!(!navigation.accepts(&other));
        assert!(navigation.accepts(&current));
    }
}
