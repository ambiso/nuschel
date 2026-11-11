// SPDX-License-Identifier: EUPL-1.2
//! Where your voice goes while you are on several servers: listen anywhere,
//! speak in one place (docs/clients.md, "Voice on several servers").
//!
//! The *talk servers* own the microphone. There is one by default: the
//! server whose channel you last joined, or the one you chose with "Use voice
//! here". Every other connected server that is not deafened only listens,
//! and sees you self-muted, so what each server shows of you is true.
//! Unmuting on a second server asks first ([`Unmute::Ask`]); once you said
//! yes, both are talk servers until you move your voice. Push-to-talk and the
//! mute shortcuts act on the talk servers only.
//!
//! This is the policy alone, over each server's state as a [`Seat`]; the
//! client sends what it decides ([`Change`]) and the driver routes the one
//! microphone to every server that is unmuted (`native.rs`).

/// What the policy needs to know of one server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Seat {
    pub connected: bool,
    pub muted: bool,
    pub deafened: bool,
}

impl Seat {
    /// The microphone reaches this server.
    pub fn speaking(self) -> bool {
        self.connected && !self.muted && !self.deafened
    }
}

/// Self-mute and self-deafen to send to a server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub server: String,
    pub muted: bool,
    pub deafened: bool,
}

/// The answer to unmuting on a server.
#[derive(Debug, PartialEq, Eq)]
pub enum Unmute {
    /// Done: these changes are to be sent.
    Now(Vec<Change>),
    /// Your voice is on another server already: ask "Speak on two
    /// servers?", then [`Voice::speak_here`].
    Ask,
}

/// One server's part in your voice, for a UI to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// A talk server, unmuted: the microphone goes here.
    Talking,
    /// A talk server, muted: push-to-talk and the mute shortcut act here.
    Muted,
    /// You hear this server; it sees you muted.
    Listening,
    Deafened,
    Offline,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Voice {
    /// The talk servers by id, the one chosen last at the end.
    talk: Vec<String>,
}

impl Voice {
    /// The talk servers, the one chosen last at the end.
    pub fn talk(&self) -> &[String] {
        &self.talk
    }

    pub fn is_talk(&self, server: &str) -> bool {
        self.talk.iter().any(|talk| talk == server)
    }

    pub fn role(&self, server: &str, seat: Seat) -> Role {
        if !seat.connected {
            Role::Offline
        } else if seat.deafened {
            Role::Deafened
        } else if !self.is_talk(server) {
            Role::Listening
        } else if seat.muted {
            Role::Muted
        } else {
            Role::Talking
        }
    }

    /// Talk servers other than `server` that the microphone reaches.
    fn speaking_elsewhere(&self, server: &str, seats: &impl Fn(&str) -> Seat) -> Vec<String> {
        self.talk
            .iter()
            .filter(|talk| *talk != server && seats(talk).speaking())
            .cloned()
            .collect()
    }

    /// A new connection (muted) becomes the talk server unless your voice
    /// is somewhere already: it joins the channel you were last in.
    pub fn connecting(&mut self, server: &str, seats: &impl Fn(&str) -> Seat) {
        if self.speaking_elsewhere(server, seats).is_empty() {
            self.talk = vec![server.to_string()];
        }
    }

    /// A server went away. If it was the only talk server, the voice stays
    /// near you: `fallback` (the server in front, else another connected
    /// one) takes over, muted as every listening server is.
    pub fn disconnected(&mut self, server: &str, fallback: Option<&str>) {
        self.talk.retain(|talk| talk != server);
        if self.talk.is_empty()
            && let Some(fallback) = fallback.filter(|fallback| *fallback != server)
        {
            self.talk.push(fallback.to_string());
        }
    }

    /// Unmute on `server`. A talk server unmutes at once; any other asks
    /// first while your voice is on another server, and else becomes the
    /// talk server.
    pub fn unmute(&mut self, server: &str, seats: &impl Fn(&str) -> Seat) -> Unmute {
        if !self.is_talk(server) {
            if !self.speaking_elsewhere(server, seats).is_empty() {
                return Unmute::Ask;
            }
            self.talk = vec![server.to_string()];
        }
        Unmute::Now(vec![Change {
            server: server.to_string(),
            muted: false,
            deafened: false,
        }])
    }

    /// The answer to "Speak on two servers?": speak on `server` as well
    /// (`both`), or only there, which mutes the others.
    pub fn speak_here(
        &mut self,
        server: &str,
        both: bool,
        seats: &impl Fn(&str) -> Seat,
    ) -> Vec<Change> {
        let mut changes = Vec::new();
        if both {
            self.talk.retain(|talk| talk != server);
        } else {
            changes = self.mute_others(server, seats);
            self.talk.clear();
        }
        self.talk.push(server.to_string());
        changes.push(Change {
            server: server.to_string(),
            muted: false,
            deafened: false,
        });
        changes
    }

    /// Move your voice to `server`: you joined a channel there, or chose
    /// "Use voice here". A live microphone moves with you (the other servers
    /// now see you muted); a muted one stays muted. Moving within a talk
    /// server changes nothing.
    pub fn move_here(&mut self, server: &str, seats: &impl Fn(&str) -> Seat) -> Vec<Change> {
        if self.is_talk(server) {
            return Vec::new();
        }
        let mut changes = self.mute_others(server, seats);
        let live = !changes.is_empty();
        self.talk = vec![server.to_string()];
        if live && !seats(server).speaking() {
            changes.push(Change {
                server: server.to_string(),
                muted: false,
                deafened: false,
            });
        }
        changes
    }

    /// Mute every other talk server the microphone reaches.
    fn mute_others(&self, server: &str, seats: &impl Fn(&str) -> Seat) -> Vec<Change> {
        self.speaking_elsewhere(server, seats)
            .into_iter()
            .map(|other| Change {
                server: other,
                muted: true,
                deafened: false,
            })
            .collect()
    }

    /// The mute (or deafen) shortcut: toggles the talk servers together.
    /// Without one, `fallback` (the server in front) is unmuted as with its
    /// button, which may ask first.
    pub fn toggle(
        &mut self,
        deafen: bool,
        fallback: Option<&str>,
        seats: &impl Fn(&str) -> Seat,
    ) -> Unmute {
        let targets: Vec<(String, Seat)> = self
            .talk
            .iter()
            .map(|talk| (talk.clone(), seats(talk)))
            .filter(|(_, seat)| seat.connected)
            .collect();
        if targets.is_empty() {
            let Some(fallback) = fallback.filter(|fallback| seats(fallback).connected) else {
                return Unmute::Now(Vec::new());
            };
            let seat = seats(fallback);
            return if deafen {
                Unmute::Now(vec![Change {
                    server: fallback.to_string(),
                    muted: true,
                    deafened: !seat.deafened,
                }])
            } else if seat.muted || seat.deafened {
                self.unmute(fallback, seats)
            } else {
                Unmute::Now(vec![Change {
                    server: fallback.to_string(),
                    muted: true,
                    deafened: false,
                }])
            };
        }
        let changes = if deafen {
            let deafened = targets.iter().any(|(_, seat)| !seat.deafened);
            targets
                .into_iter()
                .map(|(server, seat)| Change {
                    server,
                    muted: deafened || seat.muted,
                    deafened,
                })
                .collect()
        } else {
            // A talk server you deafened stays so, unless every one is.
            let hearing: Vec<_> = targets
                .iter()
                .filter(|(_, seat)| !seat.deafened)
                .cloned()
                .collect();
            let targets = if hearing.is_empty() { targets } else { hearing };
            let muted = targets.iter().any(|(_, seat)| seat.speaking());
            targets
                .into_iter()
                .map(|(server, _)| Change {
                    server,
                    muted,
                    deafened: false,
                })
                .collect()
        };
        Unmute::Now(changes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Servers' seats, changed as the client would after sending.
    struct Servers(BTreeMap<&'static str, Seat>);

    impl Servers {
        /// Connected servers, all muted.
        fn new(names: &[&'static str]) -> Self {
            Self(
                names
                    .iter()
                    .map(|name| {
                        let seat = Seat {
                            connected: true,
                            muted: true,
                            deafened: false,
                        };
                        (*name, seat)
                    })
                    .collect(),
            )
        }

        fn seats(&self) -> impl Fn(&str) -> Seat + '_ {
            |server| self.0.get(server).copied().unwrap_or_default()
        }

        fn apply(&mut self, changes: Vec<Change>) {
            for change in changes {
                let seat = self
                    .0
                    .iter_mut()
                    .find(|(name, _)| **name == change.server)
                    .unwrap()
                    .1;
                seat.muted = change.muted;
                seat.deafened = change.deafened;
            }
        }

        fn speaking(&self) -> Vec<&'static str> {
            self.0
                .iter()
                .filter(|(_, seat)| seat.speaking())
                .map(|(name, _)| *name)
                .collect()
        }
    }

    fn unmute(voice: &mut Voice, servers: &mut Servers, server: &str) -> bool {
        let decision = voice.unmute(server, &servers.seats());
        match decision {
            Unmute::Now(changes) => {
                servers.apply(changes);
                true
            }
            Unmute::Ask => false,
        }
    }

    #[test]
    fn the_last_connection_is_the_talk_server_until_you_speak() {
        let servers = Servers::new(&["a", "b"]);
        let mut voice = Voice::default();
        voice.connecting("a", &servers.seats());
        voice.connecting("b", &servers.seats());
        assert_eq!(voice.talk(), ["b"]);
        assert_eq!(voice.role("a", servers.seats()("a")), Role::Listening);
        assert_eq!(voice.role("b", servers.seats()("b")), Role::Muted);
    }

    #[test]
    fn unmuting_a_second_server_asks_and_listening_is_free() {
        let mut servers = Servers::new(&["a", "b", "c"]);
        let mut voice = Voice::default();
        voice.connecting("a", &servers.seats());
        assert!(unmute(&mut voice, &mut servers, "a"));
        // Connecting elsewhere while you speak keeps your voice where it is.
        voice.connecting("b", &servers.seats());
        voice.connecting("c", &servers.seats());
        assert_eq!(voice.talk(), ["a"]);
        assert_eq!(servers.speaking(), ["a"]);
        // b and c listen without a question; unmuting b asks.
        assert_eq!(voice.role("b", servers.seats()("b")), Role::Listening);
        assert!(!unmute(&mut voice, &mut servers, "b"));
        assert_eq!(servers.speaking(), ["a"], "asking changed nothing");
        // "Speak on both": b joins a; it asks only once.
        let changes = voice.speak_here("b", true, &servers.seats());
        servers.apply(changes);
        assert_eq!(servers.speaking(), ["a", "b"]);
        assert_eq!(voice.talk(), ["a", "b"]);
        servers.apply(vec![Change {
            server: "b".into(),
            muted: true,
            deafened: false,
        }]);
        assert!(unmute(&mut voice, &mut servers, "b"), "asked a second time");
        assert_eq!(servers.speaking(), ["a", "b"]);
    }

    #[test]
    fn only_here_moves_the_microphone() {
        let mut servers = Servers::new(&["a", "b"]);
        let mut voice = Voice::default();
        voice.connecting("a", &servers.seats());
        assert!(unmute(&mut voice, &mut servers, "a"));
        assert!(!unmute(&mut voice, &mut servers, "b"));
        let changes = voice.speak_here("b", false, &servers.seats());
        servers.apply(changes);
        assert_eq!(servers.speaking(), ["b"]);
        assert_eq!(voice.talk(), ["b"]);
        assert_eq!(voice.role("a", servers.seats()("a")), Role::Listening);
    }

    #[test]
    fn unmuting_where_nobody_hears_you_needs_no_question() {
        let mut servers = Servers::new(&["a", "b"]);
        let mut voice = Voice::default();
        voice.connecting("a", &servers.seats());
        voice.connecting("b", &servers.seats());
        // The talk server b is muted: a takes over without asking.
        assert!(unmute(&mut voice, &mut servers, "a"));
        assert_eq!(voice.talk(), ["a"]);
        assert_eq!(servers.speaking(), ["a"]);
    }

    #[test]
    fn joining_a_channel_moves_a_live_microphone_and_keeps_a_muted_one_muted() {
        let mut servers = Servers::new(&["a", "b", "c"]);
        let mut voice = Voice::default();
        voice.connecting("a", &servers.seats());
        assert!(unmute(&mut voice, &mut servers, "a"));
        let changes = voice.move_here("b", &servers.seats());
        servers.apply(changes);
        assert_eq!(voice.talk(), ["b"]);
        assert_eq!(servers.speaking(), ["b"], "the voice moved with you");
        // Moving between channels of the talk server changes nothing.
        assert!(voice.move_here("b", &servers.seats()).is_empty());
        servers.apply(vec![Change {
            server: "b".into(),
            muted: true,
            deafened: false,
        }]);
        let changes = voice.move_here("c", &servers.seats());
        assert!(changes.is_empty(), "a muted voice stays muted: {changes:?}");
        assert_eq!(voice.talk(), ["c"]);
    }

    #[test]
    fn moving_from_two_talk_servers_mutes_both() {
        let mut servers = Servers::new(&["a", "b", "c"]);
        let mut voice = Voice::default();
        voice.connecting("a", &servers.seats());
        assert!(unmute(&mut voice, &mut servers, "a"));
        let changes = voice.speak_here("b", true, &servers.seats());
        servers.apply(changes);
        let changes = voice.move_here("c", &servers.seats());
        servers.apply(changes);
        assert_eq!(servers.speaking(), ["c"]);
        assert_eq!(voice.talk(), ["c"]);
    }

    #[test]
    fn shortcuts_act_on_the_talk_servers_only() {
        let mut servers = Servers::new(&["a", "b", "c"]);
        let mut voice = Voice::default();
        voice.connecting("a", &servers.seats());
        assert!(unmute(&mut voice, &mut servers, "a"));
        let changes = voice.speak_here("b", true, &servers.seats());
        servers.apply(changes);
        // Mute both talk servers, and unmute both again; c is untouched.
        let Unmute::Now(changes) = voice.toggle(false, Some("c"), &servers.seats()) else {
            panic!("the mute shortcut asked");
        };
        servers.apply(changes);
        assert!(servers.speaking().is_empty());
        let Unmute::Now(changes) = voice.toggle(false, Some("c"), &servers.seats()) else {
            panic!("unmuting talk servers asked");
        };
        servers.apply(changes);
        assert_eq!(servers.speaking(), ["a", "b"]);
        // Deafen: both deafened, and back.
        let Unmute::Now(changes) = voice.toggle(true, Some("c"), &servers.seats()) else {
            panic!();
        };
        assert!(changes.iter().all(|change| change.deafened && change.muted));
        assert_eq!(changes.len(), 2);
        servers.apply(changes);
        let Unmute::Now(changes) = voice.toggle(true, Some("c"), &servers.seats()) else {
            panic!();
        };
        servers.apply(changes);
        assert!(!servers.seats()("a").deafened && !servers.seats()("c").deafened);
        // A talk server you deafened is left alone by the mute shortcut.
        servers.apply(vec![Change {
            server: "a".into(),
            muted: true,
            deafened: true,
        }]);
        let Unmute::Now(changes) = voice.toggle(false, None, &servers.seats()) else {
            panic!();
        };
        assert_eq!(
            changes,
            [Change {
                server: "b".into(),
                muted: false,
                deafened: false
            }]
        );
    }

    #[test]
    fn without_a_talk_server_the_shortcut_acts_where_you_are() {
        let mut servers = Servers::new(&["a", "b"]);
        let mut voice = Voice::default();
        let Unmute::Now(changes) = voice.toggle(false, Some("b"), &servers.seats()) else {
            panic!();
        };
        servers.apply(changes);
        assert_eq!(voice.talk(), ["b"]);
        assert_eq!(servers.speaking(), ["b"]);
        // Not connected anywhere: nothing.
        let mut voice = Voice::default();
        let Unmute::Now(changes) = voice.toggle(false, Some("z"), &servers.seats()) else {
            panic!();
        };
        assert!(changes.is_empty());
    }

    #[test]
    fn a_disconnected_talk_server_hands_over_to_where_you_are() {
        let servers = Servers::new(&["a", "b"]);
        let mut voice = Voice::default();
        voice.connecting("a", &servers.seats());
        voice.disconnected("a", Some("b"));
        assert_eq!(voice.talk(), ["b"]);
        voice.disconnected("b", None);
        assert!(voice.talk().is_empty());
        // A listening server going away leaves the talk server alone.
        voice.connecting("a", &servers.seats());
        voice.disconnected("b", Some("b"));
        assert_eq!(voice.talk(), ["a"]);
    }

    #[test]
    fn deafened_servers_are_neither_talking_nor_listening() {
        let voice = Voice {
            talk: vec!["a".into()],
        };
        let deafened = Seat {
            connected: true,
            muted: true,
            deafened: true,
        };
        assert_eq!(voice.role("a", deafened), Role::Deafened);
        assert_eq!(voice.role("b", deafened), Role::Deafened);
        assert_eq!(voice.role("b", Seat::default()), Role::Offline);
    }
}
