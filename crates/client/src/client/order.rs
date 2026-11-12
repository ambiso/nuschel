// SPDX-License-Identifier: EUPL-1.2
//! Presentation order is independent of live connection indices. Only a
//! validated stable-ID permutation changes it; an adapter commits its ordered
//! bookmark snapshot atomically before reporting a successful drop.
use super::Client;
use crate::server::ServerState;
use nuschl_core::bookmark::ServerBookmark;
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub(super) struct Catalog {
    ids: Vec<String>,
    indices: Vec<usize>,
}

impl Catalog {
    pub(super) fn new(bookmarks: &[ServerBookmark]) -> Self {
        Self {
            ids: bookmarks.iter().map(|server| server.id.clone()).collect(),
            indices: (0..bookmarks.len()).collect(),
        }
    }

    /// Catalog changes are infrequent; rebuild the index cache here rather
    /// than looking up/sorting IDs on every frame. Defensive index tracking
    /// also leaves externally introduced duplicate entries visible.
    pub(super) fn synchronize(&mut self, servers: &[ServerState]) {
        let by_id: HashMap<_, _> = servers
            .iter()
            .enumerate()
            .map(|(index, server)| (server.id.as_str(), index))
            .collect();
        let mut seen = HashSet::new();
        let mut indices: Vec<_> = self
            .ids
            .iter()
            .filter_map(|id| by_id.get(id.as_str()).copied())
            .filter(|index| seen.insert(*index))
            .collect();
        indices.extend((0..servers.len()).filter(|index| seen.insert(*index)));
        self.ids = indices
            .iter()
            .map(|index| servers[*index].id.clone())
            .collect();
        self.indices = indices;
    }

    pub(super) fn replaced(&mut self, old_id: &str, new_id: &str) {
        for id in &mut self.ids {
            if id == old_id {
                *id = new_id.to_owned();
            }
        }
    }
}

impl Client {
    /// Stable IDs in their saved/display order; live server indices never move.
    pub fn server_order(&self) -> &[String] {
        &self.catalog.ids
    }

    pub fn ordered_server_indices(&self) -> &[usize] {
        &self.catalog.indices
    }

    /// No allocation or sorting on the UI draw path. Commands still use the
    /// returned runtime index, never the ordinal position in this iterator.
    pub fn ordered_servers(&self) -> impl Iterator<Item = (usize, &ServerState)> {
        self.catalog
            .indices
            .iter()
            .filter_map(|index| self.servers.get(*index).map(|server| (*index, server)))
    }

    /// Exact current-ID permutation only. Failure changes no state. Persistence
    /// belongs to the adapter's atomic write barrier, not a per-frame SDK side
    /// effect. Existing bookmark/cache writers must use ordered snapshots.
    pub fn set_server_order(&mut self, ids: &[String]) -> Result<bool, String> {
        if ids.len() != self.servers.len() {
            return Err("The saved server list changed. Try reordering it again.".into());
        }
        let indices: HashMap<_, _> = self
            .servers
            .iter()
            .enumerate()
            .map(|(index, server)| (server.id.as_str(), index))
            .collect();
        let mut seen = HashSet::new();
        let order: Option<Vec<_>> = ids
            .iter()
            .map(|id| {
                seen.insert(id.as_str())
                    .then(|| indices.get(id.as_str()).copied())
                    .flatten()
            })
            .collect();
        let Some(order) = order.filter(|_| indices.len() == self.servers.len()) else {
            return Err("Server order must contain each current saved server exactly once.".into());
        };
        if self.catalog.ids == ids {
            return Ok(false);
        }
        self.catalog.ids = ids.to_vec();
        self.catalog.indices = order;
        Ok(true)
    }

    pub fn ordered_bookmarks(&self) -> Vec<ServerBookmark> {
        self.ordered_servers()
            .map(|(_, server)| server.bookmark.clone())
            .collect()
    }

    /// Preserve catalog order when a caller prepares a pending add/remove/edit
    /// before mutating Client. New IDs retain their candidate order at the end.
    pub fn order_bookmarks(&self, candidates: &[ServerBookmark]) -> Vec<ServerBookmark> {
        let rank: HashMap<_, _> = self
            .catalog
            .ids
            .iter()
            .enumerate()
            .map(|(index, id)| (id.as_str(), index))
            .collect();
        let mut ordered = candidates.to_vec();
        ordered.sort_by_key(|server| rank.get(server.id.as_str()).copied().unwrap_or(usize::MAX));
        ordered
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Conversation, HistorySource, HistoryTicket,
        names::Preferences,
        platform::{CacheKey, ChangeRequest, Platform, RenderKey, Request},
        timeline::Page,
    };
    use nuschl_core::{SearchCursor, audio::Sound};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[derive(Default)]
    struct NoIo(AtomicUsize);
    impl Platform for NoIo {
        fn connect(&self, _: ServerBookmark, _: Option<String>, _: bool) -> Result<u64, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(1)
        }
        fn disconnect(&self, _: &str) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn request(&self, _: &str, _: Request) -> Result<(), String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn local_history(
            &self,
            _: String,
            _: String,
            _: String,
            _: HistoryTicket,
            _: Page,
        ) -> Result<(), String> {
            panic!("order must not request history")
        }
        fn server_history(
            &self,
            _: String,
            _: [u8; 16],
            _: HistoryTicket,
            _: Option<CacheKey>,
            _: Page,
        ) -> Result<(), String> {
            panic!("order must not request history")
        }
        fn change_message(&self, _: ChangeRequest) -> Result<(), String> {
            panic!("order must not edit messages")
        }
        fn render_html(&self, _: String, _: u64, _: RenderKey, _: String) {
            panic!("order must not render")
        }
        fn decode_avatar(&self, _: String, _: u64, _: String, _: Vec<u8>) {
            panic!("order must not decode")
        }
        fn play_sound(&self, _: &str, _: Sound) {
            panic!("order must not play sound")
        }
        fn chime(&self, _: Sound) {
            panic!("order must not play sound")
        }
        fn search(
            &self,
            _: u64,
            _: (Vec<String>, Option<String>),
            _: String,
            _: Option<SearchCursor>,
        ) -> Result<(), String> {
            panic!("order must not search")
        }
    }

    fn bookmark(name: &str) -> ServerBookmark {
        ServerBookmark::new(name, "localhost", 64738, "tester").unwrap()
    }
    fn client() -> (Client, Arc<NoIo>) {
        let io = Arc::new(NoIo::default());
        (
            Client::new(
                Box::new(io.clone()),
                vec![bookmark("A"), bookmark("B"), bookmark("C")],
                Preferences::default(),
            ),
            io,
        )
    }

    #[test]
    fn a_permutation_preserves_live_indices_voice_selection_and_history_tickets() {
        let (mut client, io) = client();
        let ids = client.server_order().to_vec();
        let conversation = Conversation {
            server: ids[1].clone(),
            channel: 0,
        };
        let ticket = client
            .navigation
            .browse(conversation.clone(), HistorySource::LocalObserved);
        client.navigation.set_voice(Some(conversation.clone()));
        client.servers[1].generation = 83;
        client.servers[1].bookmark.certificate_sha256 = Some("ab".repeat(32));
        client.voice.connecting(&ids[1], &|_| crate::voice::Seat {
            connected: true,
            muted: true,
            deafened: false,
        });
        let voice = client.voice.clone();
        let pointers: Vec<_> = client.servers.iter().map(std::ptr::from_ref).collect();
        assert!(
            client
                .set_server_order(&[ids[2].clone(), ids[0].clone(), ids[1].clone()])
                .unwrap()
        );
        assert_eq!(
            client
                .ordered_servers()
                .map(|(index, _)| index)
                .collect::<Vec<_>>(),
            [2, 0, 1]
        );
        assert_eq!(
            client
                .servers
                .iter()
                .map(|s| s.id.clone())
                .collect::<Vec<_>>(),
            ids
        );
        assert_eq!(
            client
                .servers
                .iter()
                .map(std::ptr::from_ref)
                .collect::<Vec<_>>(),
            pointers
        );
        assert_eq!(client.navigation.selection(), Some(&conversation));
        assert_eq!(client.navigation.voice(), Some(&conversation));
        assert!(client.navigation.accepts(&ticket));
        assert_eq!(client.voice, voice);
        assert_eq!(client.servers[1].generation, 83);
        assert_eq!(
            client.servers[1].bookmark.certificate_sha256.as_deref(),
            Some("ab".repeat(32).as_str())
        );
        assert_eq!(io.0.load(Ordering::SeqCst), 0);
        assert!(client.take_updates().is_empty());
    }

    #[test]
    fn late_server_inputs_keep_their_stable_id_and_generation_after_reordering() {
        let (mut client, io) = client();
        let ids = client.server_order().to_vec();
        client.servers[1].generation = 7;
        client
            .set_server_order(&[ids[1].clone(), ids[2].clone(), ids[0].clone()])
            .unwrap();
        client.handle(crate::input::Input::Server {
            server: ids[1].clone(),
            generation: 7,
            event: crate::input::ServerEvent::Notice("for B only".into()),
        });
        assert_eq!(client.servers[1].notice.as_deref(), Some("for B only"));
        assert!(client.servers[0].notice.is_none());
        client.handle(crate::input::Input::Server {
            server: ids[1].clone(),
            generation: 6,
            event: crate::input::ServerEvent::Notice("stale".into()),
        });
        assert_eq!(client.servers[1].notice.as_deref(), Some("for B only"));
        assert_eq!(io.0.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn invalid_or_identical_orders_do_not_mutate_or_emit_effects() {
        let (mut client, io) = client();
        let original = client.server_order().to_vec();
        for invalid in [
            vec![],
            vec![original[0].clone(); 3],
            vec![original[0].clone(), original[1].clone(), "unknown".into()],
        ] {
            assert!(client.set_server_order(&invalid).is_err());
            assert_eq!(client.server_order(), original);
        }
        assert!(!client.set_server_order(&original).unwrap());
        assert!(client.take_updates().is_empty());
        assert_eq!(io.0.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn adds_removals_replacements_and_serialization_keep_the_catalog_order() {
        let (mut client, _) = client();
        let ids = client.server_order().to_vec();
        client
            .set_server_order(&[ids[2].clone(), ids[0].clone(), ids[1].clone()])
            .unwrap();
        let replacement = bookmark("D");
        let replacement_id = replacement.id.clone();
        client.replace_server(1, replacement);
        assert_eq!(
            client.server_order(),
            [ids[2].clone(), ids[0].clone(), replacement_id.clone()]
        );
        client.remove_server(&ids[0]);
        let added = bookmark("E");
        let added_id = added.id.clone();
        client.add_server(added);
        let expected = vec![ids[2].clone(), replacement_id, added_id];
        assert_eq!(client.server_order(), expected);
        let wire = serde_json::to_string(&client.ordered_bookmarks()).unwrap();
        let restored: Vec<ServerBookmark> = serde_json::from_str(&wire).unwrap();
        let reopened = Client::new(Box::new(NoIo::default()), restored, Preferences::default());
        assert_eq!(reopened.server_order(), expected);
        assert_eq!(
            reopened
                .ordered_servers()
                .map(|(_, server)| server.name.as_str())
                .collect::<Vec<_>>(),
            ["C", "D", "E"]
        );
    }

    #[test]
    fn pending_bookmark_writers_cannot_restore_the_runtime_vector_order() {
        let (mut client, _) = client();
        let ids = client.server_order().to_vec();
        client
            .set_server_order(&[ids[2].clone(), ids[0].clone(), ids[1].clone()])
            .unwrap();
        let mut candidates: Vec<_> = client
            .servers
            .iter()
            .filter(|server| server.id != ids[0])
            .map(|server| server.bookmark.clone())
            .collect();
        candidates.push(bookmark("New"));
        assert_eq!(
            client
                .order_bookmarks(&candidates)
                .iter()
                .map(|server| server.name.as_str())
                .collect::<Vec<_>>(),
            ["C", "B", "New"]
        );
    }
}
