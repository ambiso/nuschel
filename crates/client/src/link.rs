// SPDX-License-Identifier: EUPL-1.2
//! Opening `mumble://` and share links on native platforms, where the link's parser
//! and the servers' security records are `nuschl-session`'s.
use crate::{Client, Update};
use nuschl_session::config::{MumbleUrl, Profile, ServerBookmark};

impl Client {
    /// Connect to a `mumble://` link, reusing a saved server with its host,
    /// port and (if the link names one) username, or saving a new one. An
    /// invite's certificate pin fills in a missing pin, never replaces one,
    /// and its `policy=nuschl` raises the server's security record before
    /// anything connects. The link's channel is joined once the tree is
    /// known, and its password is kept for this run only. It connects
    /// muted, and `deafened` too if so asked.
    ///
    /// Returns the server's index, or `None` when no saved server matches
    /// and the link names nobody: a UI asks for a username (and adds the
    /// server itself). New or changed saved servers come as
    /// [`Update::BookmarksChangedNow`].
    pub fn open_url(
        &mut self,
        url: MumbleUrl,
        profile: &Profile,
        deafened: bool,
    ) -> Result<Option<usize>, String> {
        if url.require_nuschl {
            profile
                .policy()
                .and_then(|mut store| store.raise(&url.host, url.port, true, &[]))
                .map_err(|error| format!("Could not save the invite's security policy: {error}"))?;
        }
        let existing = self.servers.iter().position(|server| {
            server.bookmark.host == url.host
                && server.bookmark.port == url.port
                && url
                    .username
                    .as_ref()
                    .is_none_or(|name| *name == server.bookmark.username)
        });
        let index = match existing {
            Some(index) => index,
            None => {
                let Some(username) = &url.username else {
                    return Ok(None);
                };
                let mut bookmark = ServerBookmark::new("", &url.host, url.port, username)?;
                // An invite's fp= pins the certificate before the first connection.
                bookmark.certificate_sha256 = url.certificate_sha256.clone();
                self.updates.push(Update::BookmarksChangedNow);
                self.add_server(bookmark)
            }
        };
        let server = &mut self.servers[index];
        if server.bookmark.certificate_sha256.is_none()
            && let Some(pin) = url.certificate_sha256
        {
            server.bookmark.certificate_sha256 = Some(pin);
            self.updates.push(Update::BookmarksChangedNow);
        }
        server.pending_channel = url.channel;
        // A share link's token is resolved once connected (`share/1`).
        server.share_token = url.share_token;
        self.connect(index, url.password, deafened);
        Ok(Some(index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{ChangeRequest, Platform, RenderKey, Request};
    use nuschl_core::{SearchCursor, audio::Sound};

    struct Offline;
    impl Platform for Offline {
        fn connect(&self, _: ServerBookmark, _: Option<String>, _: bool) -> Result<u64, String> {
            Ok(1)
        }
        fn disconnect(&self, _: &str) {}
        fn request(&self, _: &str, _: Request) -> Result<(), String> {
            Ok(())
        }
        fn local_history(
            &self,
            _: String,
            _: String,
            _: String,
            _: crate::HistoryTicket,
            _: crate::timeline::Page,
        ) -> Result<(), String> {
            Ok(())
        }
        fn server_history(
            &self,
            _: String,
            _: [u8; 16],
            _: crate::HistoryTicket,
            _: Option<crate::platform::CacheKey>,
            _: crate::timeline::Page,
        ) -> Result<(), String> {
            Ok(())
        }
        fn change_message(&self, _: ChangeRequest) -> Result<(), String> {
            Ok(())
        }
        fn render_html(&self, _: String, _: u64, _: RenderKey, _: String) {}
        fn decode_avatar(&self, _: String, _: u64, _: String, _: Vec<u8>) {}
        fn play_sound(&self, _: &str, _: Sound) {}
        fn chime(&self, _: Sound) {}
        fn search(
            &self,
            _: u64,
            _: (Vec<String>, Option<String>),
            _: String,
            _: Option<SearchCursor>,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn links_reuse_saved_servers_and_only_fill_in_a_pin() -> Result<(), String> {
        let root = std::env::temp_dir().join(format!("nuschl-link-{}", std::process::id()));
        let profile = Profile::open(Some(root.clone())).map_err(|error| error.to_string())?;
        let saved = ServerBookmark::new("Saved", "voice.example.org", 64738, "ada")?;
        let mut client = Client::new(Box::new(Offline), vec![saved], Default::default());
        // Nobody named and nothing saved there: the UI asks.
        let nobody = MumbleUrl::parse("mumble://other.example.org/")?;
        assert_eq!(client.open_url(nobody, &profile, false)?, None);
        let pin = "ab".repeat(32);
        let invite =
            MumbleUrl::parse(&format!("mumble://voice.example.org/Lobby?fp=sha256:{pin}"))?;
        assert_eq!(client.open_url(invite, &profile, false)?, Some(0));
        assert_eq!(client.servers[0].bookmark.certificate_sha256, Some(pin));
        assert_eq!(client.servers[0].pending_channel, ["Lobby"]);
        let other = format!(
            "mumble://ada@voice.example.org/?fp=sha256:{}",
            "cd".repeat(32)
        );
        assert_eq!(
            client.open_url(MumbleUrl::parse(&other)?, &profile, false)?,
            Some(0)
        );
        assert_eq!(
            client.servers[0].bookmark.certificate_sha256,
            Some("ab".repeat(32)),
            "a link never replaces a pin"
        );
        let grace = MumbleUrl::parse("mumble://grace@voice.example.org:1234/")?;
        assert_eq!(client.open_url(grace, &profile, false)?, Some(1));
        assert_eq!(client.servers[1].bookmark.username, "grace");
        assert!(
            client
                .take_updates()
                .iter()
                .any(|update| matches!(update, Update::BookmarksChangedNow))
        );
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }
}
