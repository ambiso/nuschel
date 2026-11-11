// SPDX-License-Identifier: EUPL-1.2
//! `State::authorize` as a table: every action against each kind of actor
//! (SuperUser, an administrator, a channel's owner, a registered user an ACL
//! names, a registered user without one, a guest with a certificate, a guest
//! without), on Murmur's default root ACLs; then the inconsistencies fixed
//! when the checks moved into authz.rs, and a guard that keeps them there.
use super::{Action, Denial, SUPERUSER};
use crate::{
    acl::{AclEntry, Group, perm},
    auth::new_user,
    config::Config,
    conversations::Id,
    extension::{Extension, server_capabilities},
    kt::{KtHandle, Reply},
    routing::Datagrams,
    state::{Channel, ClientInfo, Counters, Link, Peer, Registered, State},
    storage::StorageHandle,
};
use bytes::Bytes;
use nuschel_ipc::server::HistoryVisibility;
use nuschel_protocol::{
    ControlFrame, control as proto,
    crypt::CryptState,
    kind,
    nuschel::{self, v1},
};
use proto::permission_denied::DenyType;
use std::{collections::HashMap, net::SocketAddr, path::Path, sync::Arc, time::Instant};
use tokio::sync::mpsc;

const LOBBY: u32 = 1;
/// Created by the owner; denies the administrator Write (a creator locking
/// an administrator out).
const OWNED: u32 = 2;
/// Enter, Speak, Whisper, Text and Listen denied to all, granted to the member
/// by name; the token `secret` grants Enter and Speak.
const PRIVATE: u32 = 3;
/// A temporary channel the owner made.
const TEMP: u32 = i32::MAX as u32;

const ADMIN: u32 = 2;
const OWNER: u32 = 3;
const MEMBER: u32 = 4;
const REGISTERED: u32 = 5;

const ARCHIVE: Id = [0xaa; 16];
const DIRECT: Id = [0xdd; 16];
const UNKNOWN: Id = [0x99; 16];

fn account(id: u32) -> Id {
    [0x10 + id as u8; 16]
}

fn entry(group: &str, user_id: Option<u32>, grant: u32, deny: u32) -> AclEntry {
    AclEntry {
        apply_here: true,
        apply_subs: true,
        user_id,
        group: group.into(),
        grant,
        deny,
    }
}

struct Fixture {
    state: State,
    /// In the order of [`KINDS`].
    actors: [u32; 7],
    frames: HashMap<u32, mpsc::UnboundedReceiver<Bytes>>,
}

/// The actor kinds, in table order.
const KINDS: [&str; 7] = [
    "superuser",
    "admin",
    "owner",
    "member",
    "registered",
    "guest",
    "anonymous",
];

impl Fixture {
    fn new(config: Config) -> Self {
        let settings = Arc::new(config.settings().expect("valid settings"));
        let mut state = State::new(
            settings,
            StorageHandle::detached(),
            Arc::new(Counters::default()),
        );
        state.initialise_root();
        state.kt = Some(KtHandle::detached(vec![0; 71], nuschel_kt::LogId([7; 32])));
        state
            .channels
            .get_mut(0)
            .and_then(|root| root.node.groups.get_mut("admin"))
            .expect("Murmur's admin group")
            .add
            .insert(ADMIN);
        for (id, name) in [
            (ADMIN, "Admin"),
            (OWNER, "Owner"),
            (MEMBER, "Member"),
            (REGISTERED, "Registered"),
        ] {
            state.registry.insert(Registered {
                id,
                account: account(id),
                name: name.into(),
                password_hash: None,
                certificate_hash: Some(format!("{id:02x}").repeat(20)),
                last_channel: None,
                last_active: None,
            });
        }
        let owners = || {
            let mut group = Group::new();
            group.add.insert(OWNER);
            group
        };
        let mut add = |id: u32, name: &str, edit: &dyn Fn(&mut Channel)| {
            let mut channel = Channel::new(id, Some(0), name.into());
            edit(&mut channel);
            state.conversations.add_channel(channel.conversation_id, id);
            state.channels.0.insert(id, channel);
            if let Some(root) = state.channels.get_mut(0) {
                root.children.push(id);
            }
        };
        add(LOBBY, "Lobby", &|channel| {
            channel.node.acls = vec![entry("all", None, perm::MAKE_TEMP_CHANNEL, 0)];
        });
        add(OWNED, "Owned", &|channel| {
            channel.node.groups.insert("admin".into(), owners());
            let mut locked = entry("", Some(ADMIN), 0, perm::WRITE);
            locked.apply_subs = false;
            channel.node.acls = vec![locked];
        });
        let private = perm::ENTER | perm::SPEAK | perm::WHISPER | perm::TEXT_MESSAGE | perm::LISTEN;
        add(PRIVATE, "Private", &|channel| {
            channel.node.acls = vec![
                entry("all", None, 0, private),
                entry("", Some(MEMBER), private, 0),
                entry("#secret", None, perm::ENTER | perm::SPEAK, 0),
            ];
        });
        add(TEMP, "Temporary", &|channel| {
            channel.temporary = true;
            channel.node.groups.insert("admin".into(), owners());
        });
        state.conversations.add_archived(ARCHIVE);
        state
            .conversations
            .add_direct(DIRECT, account(MEMBER), account(REGISTERED));
        let mut fixture = Self {
            state,
            actors: [0; 7],
            frames: HashMap::new(),
        };
        let people = [
            (Some(SUPERUSER), 0),
            (Some(ADMIN), 0),
            (Some(OWNER), OWNED),
            (Some(MEMBER), PRIVATE),
            (Some(REGISTERED), LOBBY),
            (None, TEMP),
            (None, LOBBY),
        ];
        for (index, (user_id, channel)) in people.into_iter().enumerate() {
            let hash = match (user_id, index) {
                (Some(id), _) => Some(format!("{id:02x}").repeat(20)),
                (None, 5) => Some("ab".repeat(20)),
                (None, _) => None,
            };
            fixture.actors[index] = fixture.connect(KINDS[index], user_id, hash, channel);
        }
        fixture.state.invalidate();
        fixture
    }

    /// A Nuschel connection in `channel`.
    fn connect(
        &mut self,
        name: &str,
        user_id: Option<u32>,
        hash: Option<String>,
        channel: u32,
    ) -> u32 {
        let session = self.state.allocate_session().expect("a session");
        let (link, frames, _) = Link::new(1 << 20);
        self.frames.insert(session, frames);
        let peer = Peer {
            address: SocketAddr::from(([10, 0, 0, session as u8], 50_000)),
            certificates: Vec::new(),
            certificate_hash: hash,
            strong: false,
            negotiation: Default::default(),
            info: ClientInfo {
                version: (1 << 48) | (5 << 32),
                ..Default::default()
            },
            link,
        };
        let (crypt, _) = CryptState::generate_server([session as u8; 48]);
        let config = self.state.settings.config.clone();
        let mut user = new_user(
            &config,
            session,
            name.into(),
            user_id,
            &peer,
            &proto::Authenticate::default(),
            crypt,
            Instant::now(),
        );
        user.extension = Some(Box::new(Extension::new(
            server_capabilities(true),
            &config,
            Instant::now(),
        )));
        user.account = user_id
            .and_then(|id| self.state.registry.users.get(&id))
            .map(|registered| registered.account);
        user.channel = channel;
        self.state.users.insert(session, user);
        if let Some(channel) = self.state.channels.get_mut(channel) {
            channel.users.push(session);
        }
        self.state.invalidate();
        session
    }

    fn actor(&self, kind: &str) -> u32 {
        self.actors[KINDS
            .iter()
            .position(|known| *known == kind)
            .expect("a kind")]
    }

    fn conversation(&self, channel: u32) -> Id {
        self.state
            .channels
            .get(channel)
            .expect("a channel")
            .conversation_id
    }

    /// Frames sent to `session` since last asked: (type, payload).
    fn received(&mut self, session: u32) -> Vec<(u16, Bytes)> {
        let mut out = Vec::new();
        if let Some(frames) = self.frames.get_mut(&session) {
            while let Ok(frame) = frames.try_recv() {
                let kind = u16::from_be_bytes([frame[0], frame[1]]);
                out.push((kind, frame.slice(6..)));
            }
        }
        out
    }

    fn handle(&mut self, session: u32, kind: u16, message: &impl prost::Message) {
        let frame = ControlFrame {
            kind,
            payload: Bytes::from(message.encode_to_vec()),
        };
        self.state.handle(session, frame, &mut Datagrams::default());
    }
}

/// An expected answer, relative to the actor asking.
#[derive(Clone, Copy, Debug)]
enum W {
    /// Allowed.
    Y,
    /// The actor lacks `permission` in `channel`.
    P(u32, u32),
    /// The kind of actor named lacks `permission` in `channel`.
    Of(&'static str, u32, u32),
    T(DenyType),
    /// The actor has no certificate.
    Cert,
    CertOf(&'static str),
    /// Ignored silently.
    I,
    /// The extension's `not_found`.
    N,
}
use W::{Cert, CertOf, I, N, Of, P, T, Y};

impl Fixture {
    fn expected(&self, actor: u32, want: W) -> Result<(), Denial> {
        match want {
            Y => Ok(()),
            P(permission, channel) => Err(Denial::Permission {
                session: actor,
                channel,
                permission,
            }),
            Of(kind, permission, channel) => Err(Denial::Permission {
                session: self.actor(kind),
                channel,
                permission,
            }),
            T(deny) => Err(Denial::Type(deny)),
            Cert => Err(Denial::MissingCertificate { session: actor }),
            CertOf(kind) => Err(Denial::MissingCertificate {
                session: self.actor(kind),
            }),
            I => Err(Denial::Ignored),
            N => Err(Denial::NotFound),
        }
    }

    /// One row: the action (built per actor) and what each kind gets.
    fn row<'a>(&self, name: &str, action: impl Fn(u32) -> Action<'a>, wants: [W; 7]) {
        for (index, want) in wants.into_iter().enumerate() {
            let actor = self.actors[index];
            let action = action(actor);
            assert_eq!(
                self.state.authorize(actor, action),
                self.expected(actor, want),
                "{name}: {} doing {action:?}",
                KINDS[index]
            );
        }
    }
}

#[test]
fn presence_voice_and_text() {
    let f = Fixture::new(Config::default());
    let [_, _, _, member, registered, guest, _] = f.actors;
    let (enter, speak, whisper, text, listen, moves, write) = (
        perm::ENTER,
        perm::SPEAK,
        perm::WHISPER,
        perm::TEXT_MESSAGE,
        perm::LISTEN,
        perm::MOVE,
        perm::WRITE,
    );
    f.row(
        "enter a restricted channel",
        |_| Action::Enter { channel: PRIVATE },
        [
            Y,
            Y,
            P(enter, PRIVATE),
            Y,
            P(enter, PRIVATE),
            P(enter, PRIVATE),
            P(enter, PRIVATE),
        ],
    );
    f.row(
        "enter an open channel",
        |_| Action::Enter { channel: LOBBY },
        [Y; 7],
    );
    f.row(
        "move the registered user into the restricted channel",
        |_| Action::Move {
            user: registered,
            to: PRIVATE,
        },
        [
            Y,
            Y,
            P(moves, LOBBY),
            P(moves, LOBBY),
            P(enter, PRIVATE),
            P(moves, LOBBY),
            P(moves, LOBBY),
        ],
    );
    f.row(
        "move the guest into the restricted channel (denial names the guest)",
        |_| Action::Move {
            user: guest,
            to: PRIVATE,
        },
        [
            Y,
            Y,
            Of("guest", enter, PRIVATE),
            P(moves, TEMP),
            P(moves, TEMP),
            P(enter, PRIVATE),
            P(moves, TEMP),
        ],
    );
    f.row(
        "join a full channel",
        |_| Action::IgnoreUserLimit { channel: OWNED },
        [
            Y,
            P(write, OWNED),
            Y,
            P(write, OWNED),
            P(write, OWNED),
            P(write, OWNED),
            P(write, OWNED),
        ],
    );
    f.row(
        "listen to the restricted channel",
        |actor| Action::Listen {
            user: actor,
            channel: PRIVATE,
        },
        [
            Y,
            Y,
            P(listen, PRIVATE),
            Y,
            P(listen, PRIVATE),
            P(listen, PRIVATE),
            P(listen, PRIVATE),
        ],
    );
    f.row(
        "listen is evaluated for the listener, not the asker",
        |_| Action::Listen {
            user: registered,
            channel: PRIVATE,
        },
        [Of("registered", listen, PRIVATE); 7],
    );
    // SuperUser has every permission but Speak and Whisper; Write does not
    // imply them either.
    f.row(
        "speak in the restricted channel",
        |_| Action::Speak { channel: PRIVATE },
        [
            P(speak, PRIVATE),
            P(speak, PRIVATE),
            P(speak, PRIVATE),
            Y,
            P(speak, PRIVATE),
            P(speak, PRIVATE),
            P(speak, PRIVATE),
        ],
    );
    f.row(
        "speak in an open channel",
        |_| Action::Speak { channel: LOBBY },
        [P(speak, LOBBY), Y, Y, Y, Y, Y, Y],
    );
    f.row(
        "whisper into the restricted channel",
        |_| Action::Whisper { channel: PRIVATE },
        [
            P(whisper, PRIVATE),
            P(whisper, PRIVATE),
            P(whisper, PRIVATE),
            Y,
            P(whisper, PRIVATE),
            P(whisper, PRIVATE),
            P(whisper, PRIVATE),
        ],
    );
    f.row(
        "whisper to the member (in the restricted channel)",
        |_| Action::WhisperUser { user: member },
        [
            P(whisper, PRIVATE),
            P(whisper, PRIVATE),
            P(whisper, PRIVATE),
            Y,
            P(whisper, PRIVATE),
            P(whisper, PRIVATE),
            P(whisper, PRIVATE),
        ],
    );
    f.row(
        "text the restricted channel",
        |_| Action::TextChannel { channel: PRIVATE },
        [
            Y,
            Y,
            P(text, PRIVATE),
            Y,
            P(text, PRIVATE),
            P(text, PRIVATE),
            P(text, PRIVATE),
        ],
    );
    f.row(
        "text the member (TextMessage in their channel)",
        |_| Action::TextUser { user: member },
        [
            Y,
            Y,
            P(text, PRIVATE),
            Y,
            P(text, PRIVATE),
            P(text, PRIVATE),
            P(text, PRIVATE),
        ],
    );
}

#[test]
fn users() {
    let f = Fixture::new(Config::default());
    let [superuser, _, _, _, registered, guest, anonymous] = f.actors;
    let (mute, reset, register, self_register) = (
        perm::MUTE_DEAFEN,
        perm::RESET_USER_CONTENT,
        perm::REGISTER,
        perm::SELF_REGISTER,
    );
    let (kick, ban) = (perm::BAN | perm::KICK, perm::BAN);
    f.row(
        "change SuperUser",
        |_| Action::ChangeUser { user: superuser },
        [
            Y,
            T(DenyType::SuperUser),
            T(DenyType::SuperUser),
            T(DenyType::SuperUser),
            T(DenyType::SuperUser),
            T(DenyType::SuperUser),
            T(DenyType::SuperUser),
        ],
    );
    f.row(
        "change a registered user",
        |_| Action::ChangeUser { user: registered },
        [Y; 7],
    );
    f.row(
        "rename anyone",
        |actor| Action::RenameUser { user: actor },
        [T(DenyType::UserName); 7],
    );
    let muting = |user| {
        move |_| Action::SpeakState {
            user,
            suppress: false,
            muting: true,
        }
    };
    f.row(
        "mute the registered user",
        muting(registered),
        [
            Y,
            Y,
            P(mute, LOBBY),
            P(mute, LOBBY),
            P(mute, LOBBY),
            P(mute, LOBBY),
            P(mute, LOBBY),
        ],
    );
    f.row(
        "mute SuperUser (not even SuperUser itself)",
        muting(superuser),
        [T(DenyType::SuperUser); 7],
    );
    f.row(
        "suppress is never set by a client",
        |_| Action::SpeakState {
            user: registered,
            suppress: true,
            muting: true,
        },
        [P(mute, LOBBY); 7],
    );
    f.row(
        "mute in a temporary channel needs Mute/Deafen in its permanent parent",
        muting(guest),
        [
            Y,
            Y,
            T(DenyType::TemporaryChannel),
            P(mute, TEMP),
            P(mute, TEMP),
            P(mute, TEMP),
            P(mute, TEMP),
        ],
    );
    f.row(
        "priority speaker in a temporary channel needs only the channel's",
        |_| Action::SpeakState {
            user: guest,
            suppress: false,
            muting: false,
        },
        [
            Y,
            Y,
            Y,
            P(mute, TEMP),
            P(mute, TEMP),
            P(mute, TEMP),
            P(mute, TEMP),
        ],
    );
    f.row(
        "clear the registered user's comment",
        |_| Action::SetComment {
            user: registered,
            clearing: true,
        },
        [Y, Y, P(reset, 0), P(reset, 0), Y, P(reset, 0), P(reset, 0)],
    );
    f.row(
        "set the registered user's comment",
        |_| Action::SetComment {
            user: registered,
            clearing: false,
        },
        [
            T(DenyType::TextTooLong),
            T(DenyType::TextTooLong),
            P(reset, 0),
            P(reset, 0),
            Y,
            P(reset, 0),
            P(reset, 0),
        ],
    );
    f.row(
        "clear the registered user's avatar",
        |_| Action::SetTexture {
            user: registered,
            clearing: true,
        },
        [Y, Y, P(reset, 0), P(reset, 0), Y, P(reset, 0), P(reset, 0)],
    );
    f.row(
        "register oneself",
        |actor| Action::Register { user: actor },
        [
            P(self_register, 0),
            P(self_register, 0),
            P(self_register, 0),
            P(self_register, 0),
            P(self_register, 0),
            Y,
            Cert,
        ],
    );
    f.row(
        "register the guest",
        |_| Action::Register { user: guest },
        [
            Y,
            Y,
            P(register, 0),
            P(register, 0),
            P(register, 0),
            Y,
            P(register, 0),
        ],
    );
    f.row(
        "register the guest without a certificate",
        |_| Action::Register { user: anonymous },
        [
            CertOf("anonymous"),
            CertOf("anonymous"),
            P(register, 0),
            P(register, 0),
            P(register, 0),
            P(register, 0),
            Cert,
        ],
    );
    f.row(
        "self-mute, plugin data, recording and listeners of the registered user",
        |_| Action::SelfState { user: registered },
        [I, I, I, I, Y, I, I],
    );
    f.row(
        "kick the registered user",
        |_| Action::Kick { user: registered },
        [
            Y,
            Y,
            P(kick, 0),
            P(kick, 0),
            P(kick, 0),
            P(kick, 0),
            P(kick, 0),
        ],
    );
    f.row(
        "kick SuperUser",
        |_| Action::Kick { user: superuser },
        [P(kick, 0); 7],
    );
    f.row(
        "ban the registered user",
        |_| Action::Ban { user: registered },
        [Y, Y, P(ban, 0), P(ban, 0), P(ban, 0), P(ban, 0), P(ban, 0)],
    );
    f.row(
        "the registered user list",
        |_| Action::ManageRegistrations,
        [
            Y,
            Y,
            P(register, 0),
            P(register, 0),
            P(register, 0),
            P(register, 0),
            P(register, 0),
        ],
    );
    f.row(
        "rename or unregister SuperUser through it",
        |_| Action::EditRegistration { user_id: SUPERUSER },
        [I; 7],
    );
    f.row(
        "rename or unregister someone else through it",
        |_| Action::EditRegistration {
            user_id: REGISTERED,
        },
        [Y; 7],
    );
    f.row(
        "the ban list",
        |_| Action::ManageBans,
        [Y, Y, P(ban, 0), P(ban, 0), P(ban, 0), P(ban, 0), P(ban, 0)],
    );
    f.row(
        "look up registered names (Write somewhere)",
        |_| Action::QueryUsers,
        [Y, Y, Y, I, I, I, I],
    );
    let member = f.actor("member");
    f.row(
        "statistics of the member (in the restricted channel)",
        |_| Action::UserStats { user: member },
        [
            Y,
            Y,
            P(perm::ENTER, PRIVATE),
            Y,
            P(perm::ENTER, PRIVATE),
            P(perm::ENTER, PRIVATE),
            P(perm::ENTER, PRIVATE),
        ],
    );
    f.row(
        "certificates and address of the registered user",
        |_| Action::UserStatsDetails { user: registered },
        [Y, Y, I, I, Y, I, I],
    );
    f.row(
        "connection statistics of the registered user (same channel or details)",
        |_| Action::UserStatsLocal { user: registered },
        [Y, Y, I, I, Y, I, Y],
    );
}

#[test]
fn channels() {
    let f = Fixture::new(Config::default());
    let (write, make, make_temporary, link) = (
        perm::WRITE,
        perm::MAKE_CHANNEL,
        perm::MAKE_TEMP_CHANNEL,
        perm::LINK_CHANNEL,
    );
    f.row(
        "make a channel",
        |_| Action::MakeChannel {
            parent: LOBBY,
            temporary: false,
        },
        [
            Y,
            Y,
            P(make, LOBBY),
            P(make, LOBBY),
            P(make, LOBBY),
            P(make, LOBBY),
            P(make, LOBBY),
        ],
    );
    f.row(
        "make a temporary channel (granted to all in the lobby)",
        |_| Action::MakeChannel {
            parent: LOBBY,
            temporary: true,
        },
        [Y, Y, Y, Y, Y, Y, Cert],
    );
    let temporary_parent = T(DenyType::TemporaryChannel);
    f.row(
        "make a channel in a temporary one",
        |_| Action::MakeChannel {
            parent: TEMP,
            temporary: true,
        },
        [
            temporary_parent,
            temporary_parent,
            temporary_parent,
            temporary_parent,
            temporary_parent,
            P(make_temporary, TEMP),
            P(make_temporary, TEMP),
        ],
    );
    let locked = [
        Y,
        P(write, OWNED),
        Y,
        P(write, OWNED),
        P(write, OWNED),
        P(write, OWNED),
        P(write, OWNED),
    ];
    f.row(
        "edit the owned channel",
        |_| Action::EditChannel { channel: OWNED },
        locked,
    );
    f.row(
        "rename the owned channel",
        |_| Action::RenameChannel { channel: OWNED },
        locked,
    );
    f.row(
        "its ACL: Write at the root overrides the lock-out",
        |_| Action::EditAcl { channel: OWNED },
        [
            Y,
            Y,
            Y,
            P(write, OWNED),
            P(write, OWNED),
            P(write, OWNED),
            P(write, OWNED),
        ],
    );
    f.row(
        "rename the root",
        |_| Action::RenameChannel { channel: 0 },
        [P(write, 0); 7],
    );
    f.row(
        "remove the root",
        |_| Action::RemoveChannel { channel: 0 },
        [P(write, 0); 7],
    );
    f.row(
        "remove the temporary channel",
        |_| Action::RemoveChannel { channel: TEMP },
        [
            Y,
            Y,
            Y,
            P(write, TEMP),
            P(write, TEMP),
            P(write, TEMP),
            P(write, TEMP),
        ],
    );
    f.row(
        "move the owned channel into the lobby",
        |_| Action::MoveChannel {
            channel: OWNED,
            to: LOBBY,
        },
        [
            Y,
            P(write, OWNED),
            P(make, LOBBY),
            P(write, OWNED),
            P(write, OWNED),
            P(write, OWNED),
            P(write, OWNED),
        ],
    );
    f.row(
        "move a channel into a temporary one",
        |_| Action::MoveChannel {
            channel: OWNED,
            to: TEMP,
        },
        [T(DenyType::TemporaryChannel); 7],
    );
    f.row(
        "move the temporary channel (Make temporary on the new parent)",
        |_| Action::MoveChannel {
            channel: TEMP,
            to: LOBBY,
        },
        [
            Y,
            Y,
            Y,
            P(write, TEMP),
            P(write, TEMP),
            P(write, TEMP),
            P(write, TEMP),
        ],
    );
    f.row(
        "link the owned channel",
        |_| Action::LinkChannel { channel: OWNED },
        [
            Y,
            P(link, OWNED),
            Y,
            P(link, OWNED),
            P(link, OWNED),
            P(link, OWNED),
            P(link, OWNED),
        ],
    );
}

#[test]
fn conversations() {
    let mut f = Fixture::new(Config::default());
    let (lobby, private, owned) = (
        f.conversation(LOBBY),
        f.conversation(PRIVATE),
        f.conversation(OWNED),
    );
    let registered_only = [Y, Y, Y, Y, Y, N, N];
    f.row(
        "read the lobby (guests read no history by default)",
        |_| Action::ReadHistory(&lobby),
        registered_only,
    );
    f.row(
        "read the restricted channel",
        |_| Action::ReadHistory(&private),
        [Y, Y, N, Y, N, N, N],
    );
    f.row(
        "read the direct conversation",
        |_| Action::ReadHistory(&DIRECT),
        [N, N, N, Y, Y, N, N],
    );
    f.row(
        "read a deleted channel's archive",
        |_| Action::ReadHistory(&ARCHIVE),
        [Y, N, N, N, N, N, N],
    );
    f.row(
        "read an unknown conversation",
        |_| Action::ReadHistory(&UNKNOWN),
        [N; 7],
    );
    f.row("send to the lobby", |_| Action::Send(&lobby), [Y; 7]);
    f.row(
        "send to the restricted channel",
        |_| Action::Send(&private),
        [Y, Y, N, Y, N, N, N],
    );
    f.row(
        "type in the restricted channel",
        |_| Action::Typing(&private),
        [Y, Y, N, Y, N, N, N],
    );
    // The member may text the registered user's channel; not the other way.
    f.row(
        "send to the direct conversation",
        |_| Action::Send(&DIRECT),
        [N, N, N, Y, N, N, N],
    );
    f.row("send to an archive", |_| Action::Send(&ARCHIVE), [N; 7]);
    let member_account = account(MEMBER);
    f.row(
        "message the member's account",
        |_| Action::SendDirect {
            peer: &member_account,
        },
        [Y, Y, N, N, N, N, N],
    );
    f.row(
        "edit in the lobby",
        |_| Action::Edit(&lobby),
        registered_only,
    );
    f.row(
        "delete one's own message in the lobby",
        |_| Action::DeleteOwn(&lobby),
        registered_only,
    );
    f.row(
        "delete one's own message where no conversation is",
        |_| Action::DeleteOwn(&UNKNOWN),
        [N; 7],
    );
    f.row(
        "moderate the owned channel",
        |_| Action::Moderate(&owned),
        [Y, N, Y, N, N, N, N],
    );
    f.row(
        "moderate the lobby",
        |_| Action::Moderate(&lobby),
        [Y, Y, N, N, N, N, N],
    );
    f.row(
        "moderate a direct conversation",
        |_| Action::Moderate(&DIRECT),
        [N; 7],
    );
    f.row(
        "moderate an archive",
        |_| Action::Moderate(&ARCHIVE),
        [N; 7],
    );
    f.row(
        "the owned channel's settings",
        |_| Action::ChannelSettings { channel: OWNED },
        [Y, N, Y, N, N, N, N],
    );
    f.row(
        "an unknown channel's settings",
        |_| Action::ChannelSettings { channel: 999 },
        [N; 7],
    );
    f.row(
        "a read marker in the lobby",
        |_| Action::ReadMarker(&lobby),
        registered_only,
    );
    f.row(
        "a read marker in the restricted channel",
        |_| Action::ReadMarker(&private),
        [Y, Y, N, Y, N, N, N],
    );
    let registered_account = account(REGISTERED);
    f.row(
        "open the direct conversation with the registered user",
        |_| Action::OpenDirect {
            peer: &registered_account,
        },
        [Y, Y, Y, Y, N, N, N],
    );
    f.row(
        "open a direct conversation with nobody",
        |_| Action::OpenDirect { peer: &UNKNOWN },
        [N; 7],
    );
    f.row(
        "list direct conversations",
        |_| Action::ListDirects,
        registered_only,
    );

    // Moderation off: authors only.
    let mut config = Config::default();
    config.history.moderators_delete = false;
    let quiet = Fixture::new(config);
    let quiet_owned = quiet.conversation(OWNED);
    quiet.row(
        "moderate with moderators_delete off",
        |_| Action::Moderate(&quiet_owned),
        [N; 7],
    );
    // Guests read with history.guests = "full".
    let mut config = Config::default();
    config.history.guests = crate::config::GuestHistory::Full;
    let open = Fixture::new(config);
    let open_lobby = open.conversation(LOBBY);
    open.row(
        "read the lobby with guests = full",
        |_| Action::ReadHistory(&open_lobby),
        [Y; 7],
    );

    // `since_access`: outside the channel, only what was said while one could read.
    if let Some(channel) = f.state.channels.get_mut(OWNED) {
        channel.history_visibility = HistoryVisibility::SinceAccess;
    }
    f.state
        .conversations
        .access
        .entry(owned)
        .or_default()
        .insert(account(MEMBER), vec![(2, Some(4))]);
    f.row(
        "see message 3 of the owned channel live",
        |_| Action::SeeLive {
            conversation: &owned,
            seq: 3,
        },
        [I, I, Y, Y, I, I, I],
    );
    f.row(
        "see message 5 of the owned channel live",
        |_| Action::SeeLive {
            conversation: &owned,
            seq: 5,
        },
        [I, I, Y, I, I, I, I],
    );
    f.row(
        "see a message of the lobby live",
        |_| Action::SeeLive {
            conversation: &lobby,
            seq: 99,
        },
        [Y; 7],
    );
}

#[test]
fn key_transparency() {
    let f = Fixture::new(Config::default());
    let (registered_account, member_account) = (account(REGISTERED), account(MEMBER));
    for action in [Action::KtStatus, Action::KtAudit, Action::KtHead] {
        f.row("status, audits and heads", |_| action, [Y; 7]);
    }
    f.row(
        "publish one's own binding",
        |_| Action::KtPublish,
        [Y, Y, Y, Y, Y, N, N],
    );
    f.row(
        "look up a registered account",
        |_| Action::KtLookup {
            account: &registered_account,
        },
        [Y; 7],
    );
    f.row(
        "look up an unknown account",
        |_| Action::KtLookup { account: &UNKNOWN },
        [N; 7],
    );
    // Contacts: the owner, a direct conversation, or the same channel now.
    f.row(
        "the member's binding history",
        |_| Action::KtHistory {
            account: &member_account,
        },
        [N, N, N, Y, Y, N, N],
    );
}

#[test]
fn reported_permissions_and_groups() {
    let f = Fixture::new(Config::default());
    let [superuser, admin, owner, _, registered, _, _] = f.actors;
    assert_eq!(f.state.sync_permissions(superuser), perm::ALL);
    assert_eq!(
        f.state.sync_permissions(registered),
        perm::DEFAULT | perm::MAKE_TEMP_CHANNEL | perm::SELF_REGISTER
    );
    assert_ne!(f.state.sync_permissions(admin) & perm::BAN, 0);
    assert!(f.state.in_group(owner, OWNED, "admin"));
    assert!(f.state.in_group(admin, OWNED, "admin"));
    assert!(!f.state.in_group(registered, OWNED, "admin"));
    assert!(f.state.in_group(registered, LOBBY, "auth"));
    assert!(!f.state.in_group(f.actor("guest"), LOBBY, "auth"));
}

// ---- inconsistencies fixed when the checks moved here ----

/// A temporary access token counts for one `UserState` only. Before, the
/// token's grants were dropped from the cache when the message was done, but
/// suppression (and the client's permission cache) were not re-evaluated as
/// after every other credential change: a token that granted Speak left the
/// user unsuppressed. Murmur's `TemporaryAccessTokenHelper` clears the ACL
/// cache with its suppression update on both ends.
#[test]
fn temporary_tokens_do_not_outlive_their_message() {
    let mut f = Fixture::new(Config::default());
    let registered = f.actor("registered");
    f.received(registered);
    f.handle(
        registered,
        kind::USER_STATE,
        &proto::UserState {
            channel_id: Some(PRIVATE),
            temporary_access_tokens: vec!["secret".into()],
            ..Default::default()
        },
    );
    let user = &f.state.users[&registered];
    assert_eq!(user.channel, PRIVATE, "the token let them in");
    assert!(user.tokens.is_empty(), "the token is gone");
    assert!(
        user.suppress,
        "Speak came only from the token, so the user is suppressed again"
    );
    assert!(
        !f.state
            .allowed(registered, Action::Speak { channel: PRIVATE })
    );
    // The client's permission cache was flushed with the token-free value.
    let flushed = f
        .received(registered)
        .into_iter()
        .filter(|(kind, _)| *kind == kind::PERMISSION_QUERY)
        .filter_map(|(_, payload)| <proto::PermissionQuery as prost::Message>::decode(payload).ok())
        .filter(|query| query.flush() && query.channel_id == Some(PRIVATE))
        .last()
        .expect("a flushed PermissionQuery");
    assert_eq!(flushed.permissions() & perm::SPEAK, 0);
}

/// A stock client's direct message is archived in the accounts' direct
/// conversation only if the sender may write to it as a Nuschel client would:
/// TextMessage in the channel of each of the peer's connections. Before, the
/// stock path checked only the connection it named and then delivered the
/// stored message to all of the account's connections.
#[test]
fn stock_direct_messages_are_archived_only_where_a_nuschel_send_would_be() {
    let mut config = Config::default();
    config.history.archive_stock_direct_messages = true;
    let mut f = Fixture::new(config);
    let registered = f.actor("registered");
    // The member is online twice: in the lobby and in the restricted channel.
    let lobby_member = f.connect("member", Some(MEMBER), None, LOBBY);
    f.state.conversations = Default::default();
    for channel in f.state.channels.0.values() {
        f.state
            .conversations
            .add_channel(channel.conversation_id, channel.id);
    }
    let text = proto::TextMessage {
        session: vec![lobby_member],
        message: "hello".into(),
        ..Default::default()
    };
    f.received(lobby_member);
    f.handle(registered, kind::TEXT_MESSAGE, &text);
    assert!(
        f.state
            .conversations
            .direct(account(REGISTERED), account(MEMBER))
            .is_none(),
        "not archived: the sender may not text the member's other connection"
    );
    assert!(
        f.received(lobby_member)
            .iter()
            .any(|(kind, _)| *kind == kind::TEXT_MESSAGE),
        "the named connection still gets it live, as from Murmur"
    );
    // Once the member's other connection is somewhere the sender may text,
    // the same message is archived.
    let member = f.actor("member");
    let mut moved = proto::UserState {
        session: Some(member),
        channel_id: Some(LOBBY),
        ..Default::default()
    };
    f.state.enter_channel(member, LOBBY, &mut moved);
    f.handle(registered, kind::TEXT_MESSAGE, &text);
    assert!(
        f.state
            .conversations
            .direct(account(REGISTERED), account(MEMBER))
            .is_some()
    );
}

/// A binding history is authorised again when its answer comes back from the
/// key-transparency task, as history pages are re-checked: someone who stopped
/// being a contact in between gets `not_found`.
#[test]
fn key_transparency_histories_are_checked_again_when_answered() {
    let mut f = Fixture::new(Config::default());
    let (owner, registered) = (f.actor("owner"), f.actor("registered"));
    let owner_account = account(OWNER);
    let history = Action::KtHistory {
        account: &owner_account,
    };
    let answer = |request_id| {
        Ok(Reply::History(v1::KtHistoryReply {
            request_id,
            account: owner_account.to_vec(),
            response: vec![1, 2, 3],
        }))
    };
    let mut moved = proto::UserState::default();
    f.state.enter_channel(owner, LOBBY, &mut moved);
    assert!(
        f.state.allowed(registered, history),
        "same channel: a contact"
    );
    f.received(registered);
    f.state.kt_answer(registered, 7, answer(7));
    assert!(
        f.received(registered)
            .iter()
            .any(|(kind, _)| *kind == nuschel::kind::KT_HISTORY_REPLY)
    );
    // Authorised, then the owner leaves before the answer arrives.
    f.state.enter_channel(owner, OWNED, &mut moved);
    assert!(!f.state.allowed(registered, history));
    f.state.kt_answer(registered, 8, answer(8));
    let frames = f.received(registered);
    assert!(
        !frames
            .iter()
            .any(|(kind, _)| *kind == nuschel::kind::KT_HISTORY_REPLY)
    );
    let error = frames
        .iter()
        .find(|(kind, _)| *kind == nuschel::kind::ERROR)
        .and_then(|(_, payload)| <v1::Error as prost::Message>::decode(payload.clone()).ok())
        .expect("an error");
    assert_eq!(error.request_id, 8);
    assert_eq!(error.code(), v1::error::Code::NotFound);
}

// ---- nothing else decides ----

/// The ACL engine's evaluation and the cache are private to authz (the
/// compiler enforces that); this also keeps the other inline forms of a
/// permission decision out of the rest of the server.
#[test]
fn nothing_outside_authz_decides_permissions() {
    let forbidden = [
        "effective_permissions",
        "group_applies",
        "self.permissions(",
        "state.permissions(",
        "can_read(",
        "can_write(",
        "kt_history_allowed",
        "user_id == Some(0)",
        "user_id != Some(0)",
        "Some(SUPERUSER)",
        "Some(crate::authz::SUPERUSER)",
    ];
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = vec![root.clone()];
    let mut offences = Vec::new();
    while let Some(path) = files.pop() {
        if path.is_dir() {
            if path == root.join("authz") {
                continue;
            }
            for entry in std::fs::read_dir(&path).expect("readable sources") {
                files.push(entry.expect("an entry").path());
            }
            continue;
        }
        if path.extension().is_none_or(|extension| extension != "rs")
            || path == root.join("authz.rs")
        {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("readable source");
        for (number, line) in text.lines().enumerate() {
            // A permission bit tested inline (`.has(session, channel, perm::…)`).
            let inline = line.contains(".has(") && line.contains("perm::");
            if !line.trim_start().starts_with("//")
                && (inline || forbidden.iter().any(|pattern| line.contains(pattern)))
            {
                offences.push(format!(
                    "{}:{}: {}",
                    path.display(),
                    number + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        offences.is_empty(),
        "permission decisions outside authz.rs:\n{}",
        offences.join("\n")
    );
}
