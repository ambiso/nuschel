// SPDX-License-Identifier: EUPL-1.2
//! Authentication and the initial state a new user receives, as stock Murmur
//! does it (`Server::authenticate`, `Server::msgAuthenticate`). The slow parts,
//! argon2id verification and loading a registered user's blobs, run without
//! the state lock; the decision is re-checked under it.
use crate::{
    authz::{self, Action},
    handlers::bounded_tokens,
    limits::{BandwidthRecord, LeakyBucket},
    password,
    server::Server,
    state::{Peer, State, User, Voice, now_unix, random},
};
use bytes::Bytes;
use nuschel_protocol::{
    control as proto,
    crypt::{CryptSetupParts, CryptState},
    kind,
};
use proto::reject::RejectType;
use std::{cell::RefCell, collections::HashMap, time::Instant};

/// How a login resolved before the server-wide checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Identity {
    Registered(u32),
    /// Not a registered user (Murmur's -2).
    Unknown,
    /// Wrong password or certificate for a registered name (Murmur's -1).
    Failed,
}

pub struct Rejection {
    pub kind: RejectType,
    pub reason: String,
}

fn reject(kind: RejectType, reason: impl Into<String>) -> Rejection {
    Rejection {
        kind,
        reason: reason.into(),
    }
}

/// Authenticate a connection; on success it is a user with this session.
pub async fn authenticate(
    server: &Server,
    peer: &Peer,
    message: proto::Authenticate,
) -> Result<u32, Rejection> {
    let name = message
        .username
        .clone()
        .unwrap_or_default()
        .trim()
        .to_owned();
    let password = message.password.clone().unwrap_or_default();
    // What the registry says about this name, looked up without holding the
    // lock during the expensive password check.
    let by_name = server
        .lock()
        .registry
        .by_name(&name)
        .map(|user| (user.id, user.password_hash.clone()));
    let busy = || {
        reject(
            RejectType::AuthenticatorFail,
            "Your account information can not be verified currently. Please try again later",
        )
    };
    let password_ok = match &by_name {
        Some((id, Some(hash))) => {
            // Guessing a registered user's password is limited per account.
            if server.login_paused(*id, false) {
                return Err(busy());
            }
            let ok = password::verify(password.clone(), hash.clone())
                .await
                .ok_or_else(busy)?;
            if !ok {
                server.login_paused(*id, true);
            }
            Some(ok)
        }
        _ => None,
    };
    // A hash imported from Murmur becomes argon2id after a successful login
    // (crate::password). A busy hasher leaves it for the next login.
    let upgrade = match (&by_name, password_ok) {
        (Some((_, Some(hash))), Some(true)) if password::is_imported(hash) => {
            let cost = password::Cost {
                memory_kib: server.settings.config.password_hashing.memory_kib,
                passes: server.settings.config.password_hashing.passes,
            };
            password::hash(password.clone(), cost).await
        }
        _ => None,
    };
    let identity = {
        let mut state = server.lock();
        let identity = resolve(
            &state,
            &by_name,
            password_ok,
            &password,
            peer.certificate_hash.as_deref(),
        );
        if let (Identity::Registered(id), Some(upgraded), Some((expected, old))) =
            (identity, upgrade, &by_name)
            && id == *expected
        {
            let replaced = match state.registry.users.get_mut(&id) {
                Some(user) if user.password_hash == *old => {
                    user.password_hash = Some(upgraded);
                    true
                }
                _ => false,
            };
            if replaced {
                state.persist_registration(id);
                tracing::info!(
                    user = id,
                    "replaced an imported Murmur password hash with argon2id"
                );
            }
        }
        identity
    };
    let blobs = match identity {
        Identity::Registered(id) if id > 0 => Some(server.storage.user_blobs(id).await),
        _ => None,
    };
    // Remembered channel listeners (Murmur's `loadChannelListenersOf`).
    let listeners = match identity {
        Identity::Registered(id) => server.storage.user_listeners(id).await,
        _ => Vec::new(),
    };
    // The account's read markers, unless another of its connections already
    // loaded them (docs/extension.md §7).
    let account = match identity {
        Identity::Registered(id) => {
            let state = server.lock();
            state
                .registry
                .users
                .get(&id)
                .map(|user| user.account)
                .filter(|account| !state.conversations.markers.contains_key(account))
        }
        _ => None,
    };
    let markers = match account {
        Some(account) => Some((
            account,
            server
                .history_reads
                .history_read_markers(crate::conversations::hex(&account))
                .await,
        )),
        None => None,
    };
    let mut state = server.lock();
    if let Some((account, markers)) = markers {
        state
            .conversations
            .markers
            .entry(account)
            .or_insert_with(|| {
                markers
                    .into_iter()
                    .filter_map(|(id, seq)| Some((crate::conversations::parse(&id)?, seq)))
                    .collect()
            });
    }
    complete(
        &mut state, peer, message, name, password, identity, blobs, listeners,
    )
}

/// Murmur's `Server::authenticate` for the built-in user database.
fn resolve(
    state: &State,
    by_name: &Option<(u32, Option<String>)>,
    password_ok: Option<bool>,
    password: &str,
    certificate: Option<&str>,
) -> Identity {
    let reserved = by_name.is_some();
    let mut identity = None;
    if let Some((id, hash)) = by_name {
        // The account may have changed while the password was being checked.
        if state.registry.users.get(id).map(|user| &user.password_hash) != Some(hash) {
            return Identity::Failed;
        }
        match (hash, password_ok) {
            (Some(_), Some(true)) => identity = Some(*id),
            (Some(_), _) => return Identity::Failed,
            // A password for an account without one could be meant as the server
            // password, but must not let anyone in under a registered name.
            (None, _) if !password.is_empty() => return Identity::Failed,
            // SuperUser always logs in with a password.
            (None, _) if *id == 0 => return Identity::Failed,
            (None, _) => {}
        }
    }
    if identity.is_none() {
        let Some(certificate) = certificate else {
            return if reserved {
                Identity::Failed
            } else {
                Identity::Unknown
            };
        };
        identity = state
            .registry
            .by_certificate(certificate)
            .map(|user| user.id);
    }
    match identity {
        Some(id) => Identity::Registered(id),
        None if reserved => Identity::Failed,
        None => Identity::Unknown,
    }
}

/// Compare secrets as SHA-256 digests in constant time, so neither the content
/// nor the length leaks through timing.
fn constant_time_equal(a: &str, b: &str) -> bool {
    let digest = |text: &str| ring::digest::digest(&ring::digest::SHA256, text.as_bytes());
    password::equal_constant_time(digest(a).as_ref(), digest(b).as_ref())
}

#[allow(clippy::too_many_arguments)]
fn complete(
    state: &mut State,
    peer: &Peer,
    message: proto::Authenticate,
    mut name: String,
    password: String,
    mut identity: Identity,
    blobs: Option<(String, Bytes)>,
    listeners: Vec<(u32, f32)>,
) -> Result<u32, Rejection> {
    if let Identity::Registered(id) = identity {
        match state.registry.users.get(&id) {
            Some(registered) => name = registered.name.clone(),
            None => identity = Identity::Failed,
        }
    }
    let name_ok = state.valid_username(&name);
    let user_id = match identity {
        Identity::Registered(id) => Some(id),
        _ => None,
    };
    let server_password = state.settings.password.clone();
    let mut result = match identity {
        Identity::Unknown if !name_ok => {
            Err(reject(RejectType::InvalidUsername, "Invalid username"))
        }
        Identity::Failed => Err(reject(
            RejectType::WrongUserPw,
            "Wrong certificate or password for existing user",
        )),
        Identity::Unknown
            if !server_password.is_empty() && !constant_time_equal(&server_password, &password) =>
        {
            Err(reject(RejectType::WrongServerPw, "Invalid server password"))
        }
        _ => Ok(()),
    };
    // A user already logged in under this account or name.
    let lower = name.to_lowercase();
    let ghost = state
        .users
        .values()
        .find(|user| {
            (user_id.is_some() && user.user_id == user_id) || user.name.to_lowercase() == lower
        })
        .map(|user| {
            (
                user.session,
                user.address.ip(),
                user.certificate_hash.clone(),
            )
        });
    // An unregistered name in use may only be taken over by the same identity:
    // the same certificate, or (both without one) the same address. Murmur also
    // lets any connection from the same address take it, which on shared
    // addresses (carrier-grade NAT, campuses) is impersonation.
    if result.is_ok()
        && user_id.is_none()
        && let Some((_, address, hash)) = &ghost
    {
        let same_certificate = peer.certificate_hash.is_some() && peer.certificate_hash == *hash;
        let same_anonymous =
            peer.certificate_hash.is_none() && hash.is_none() && *address == peer.address.ip();
        if !same_certificate && !same_anonymous {
            result = Err(reject(RejectType::UsernameInUse, "Username already in use"));
        }
    }
    let config = &state.settings.config;
    if !authz::exempt_from_admission(user_id) && state.users.len() >= config.max_users as usize {
        result = Err(reject(
            RejectType::ServerFull,
            format!("Server is full (max {} users)", config.max_users),
        ));
    }
    if !authz::exempt_from_admission(user_id)
        && peer.certificate_hash.is_none()
        && config.certificate_required
    {
        result = Err(reject(
            RejectType::NoCertificate,
            "A certificate is required to connect to this server",
        ));
    }
    // Clients before 1.2.2 need what Murmur special-cases for them (comments,
    // descriptions and avatars inline instead of by hash) and have no Opus;
    // they are turned away with the reason instead of half working. A client
    // that reports no version is taken as current, as before.
    if peer.info.version != 0 && !peer.info.at_least(1, 2, 2) {
        result = Err(reject(
            RejectType::WrongVersion,
            "This server needs Mumble 1.2.2 or later",
        ));
    }
    result?;
    let session = state
        .allocate_session()
        .ok_or_else(|| reject(RejectType::ServerFull, "No session available"))?;

    // The user exists from here on; permission checks need it in the state.
    let now = Instant::now();
    let (crypt, crypt_setup) = CryptState::generate_server(random());
    let mut user = new_user(
        &state.settings.config,
        session,
        name.clone(),
        user_id,
        peer,
        &message,
        crypt,
        now,
    );
    if let Some((comment, texture)) = blobs {
        user.set_comment(comment);
        user.set_texture(texture);
    }
    user.account = user_id
        .and_then(|id| state.registry.users.get(&id))
        .map(|registered| registered.account);
    state.users.insert(session, user);
    state.invalidate();

    // The channel to start in: the remembered one, the default one, or root.
    let remembered = user_id.and_then(|id| {
        let registered = state.registry.users.get(&id)?;
        let duration = state.settings.config.remember_channel_duration;
        let fresh = duration == 0
            || registered
                .last_active
                .is_some_and(|last| now_unix().saturating_sub(last) <= duration as i64);
        (state.settings.config.remember_channel && fresh)
            .then_some(registered.last_channel)
            .flatten()
    });
    let usable = |state: &State, channel: u32| {
        state.channels.get(channel).is_some()
            && state.allowed(session, Action::Enter { channel })
            && !state.channel_full(channel, Some(session))
    };
    let channel = [remembered, Some(state.settings.config.default_channel)]
        .into_iter()
        .flatten()
        .find(|channel| usable(state, *channel));
    let channel = match channel {
        Some(channel) => channel,
        None if !state.allowed(session, Action::Enter { channel: 0 }) => {
            state.users.remove(&session);
            state.invalidate();
            return Err(reject(
                RejectType::None,
                "You do not have permission to enter this server",
            ));
        }
        None if state.channel_full(0, Some(session)) => {
            state.users.remove(&session);
            state.invalidate();
            return Err(reject(RejectType::ServerFull, "Server channels are full"));
        }
        None => 0,
    };

    if let Some((old, _, _)) = ghost {
        tracing::info!(user = %state.log_name(old), "disconnecting ghost");
        state.send(
            old,
            kind::USER_REMOVE,
            &proto::UserRemove {
                session: old,
                reason: Some("You connected to the server from another device".into()),
                ..Default::default()
            },
        );
        state.disconnect(old, true);
    }
    state
        .hosts
        .entry(peer.address.ip())
        .or_default()
        .push(session);
    // Bind a certificate to an account that has none. Unlike Murmur, a password
    // login from a new certificate does not replace the stored one, so a leaked
    // password cannot take the account over for good.
    if let Some(id) = user_id
        && id != 0
        && let Some(hash) = &peer.certificate_hash
        && let Some(registered) = state.registry.users.get_mut(&id)
        && registered.certificate_hash.is_none()
    {
        registered.certificate_hash = Some(hash.clone());
        state.persist_registration(id);
    }
    send_initial_state(state, session, channel, crypt_setup, listeners);
    tracing::info!(
        user = %state.log_name(session),
        address = %state.settings.addresses.socket(peer.address),
        nuschel = peer.negotiation != crate::extension::Negotiation::Stock,
        strong = peer.strong,
        "authenticated"
    );
    Ok(session)
}

/// Re-add a registered user's remembered listeners after `ServerSync`, as
/// Murmur does: everyone learns the channels, only the user the volumes
/// (unless `broadcast_listener_volume_adjustments`). Unlike Murmur, Listen is
/// checked again, and the listener limits apply; a listener that fails stays
/// remembered for when it is allowed again.
fn restore_listeners(state: &mut State, session: u32, listeners: Vec<(u32, f32)>) {
    let config = &state.settings.config;
    let (per_channel, per_user, broadcast_volumes) = (
        config.listeners_per_channel,
        config.listeners_per_user,
        config.broadcast_listener_volume_adjustments,
    );
    let mut added = Vec::new();
    let mut volumes = Vec::new();
    for (channel, volume) in listeners {
        let Some(target) = state.channels.get(channel) else {
            continue;
        };
        if target.temporary
            || !state.allowed(
                session,
                Action::Listen {
                    user: session,
                    channel,
                },
            )
            || per_channel.is_some_and(|limit| target.listeners.len() >= limit as usize)
            || per_user.is_some_and(|limit| added.len() >= limit as usize)
        {
            continue;
        }
        state.add_listener(session, channel);
        if let Some(user) = state.users.get_mut(&session)
            && let Some(entry) = user.listening.get_mut(&channel)
        {
            *entry = volume.clamp(0., 64.);
        }
        added.push(channel);
        if volume != 1. {
            volumes.push(proto::user_state::VolumeAdjustment {
                listening_channel: Some(channel),
                volume_adjustment: Some(volume),
            });
        }
    }
    if added.is_empty() {
        return;
    }
    state.generation += 1;
    let mut message = proto::UserState {
        session: Some(session),
        listening_channel_add: added,
        ..Default::default()
    };
    if broadcast_volumes {
        message.listening_volume_adjustment = volumes;
        state.broadcast(kind::USER_STATE, &message, None);
    } else {
        state.broadcast(kind::USER_STATE, &message, Some(session));
        message.listening_volume_adjustment = volumes;
        state.send(session, kind::USER_STATE, &message);
    }
}

/// What a new client receives, in Murmur's order.
fn send_initial_state(
    state: &mut State,
    session: u32,
    channel: u32,
    crypt: CryptSetupParts,
    listeners: Vec<(u32, f32)>,
) {
    state.send(
        session,
        kind::CRYPT_SETUP,
        &proto::CryptSetup {
            key: Some(crypt.key.to_vec()),
            client_nonce: Some(crypt.client_nonce.to_vec()),
            server_nonce: Some(crypt.server_nonce.to_vec()),
        },
    );
    // Opus only; the CELT fields say "no CELT" the way Murmur does in Opus mode.
    state.send(
        session,
        kind::CODEC_VERSION,
        &proto::CodecVersion {
            alpha: 0x8000_000b_u32 as i32,
            beta: 0,
            prefer_alpha: true,
            opus: Some(true),
        },
    );
    let opus = state.users.get(&session).is_some_and(|user| user.opus);
    if !opus {
        state.text_to(
            session,
            "<strong>WARNING:</strong> Your client doesn't support the Opus codec the server is using, you \
             won't be able to talk or hear anyone. Please upgrade to a client with Opus support.",
        );
    }
    // The channel tree, parents first, then the links. Nuschel connections
    // also get each channel's conversation id and history properties.
    let order = state.channels.subtree(0);
    let nuschel = state
        .users
        .get(&session)
        .is_some_and(|user| user.extension.is_some());
    for &id in &order {
        if let Some(channel) = state.channels.get(id) {
            let message = state.channel_state(channel, session);
            match nuschel
                .then(|| {
                    nuschel_protocol::nuschel::channel_state_frame(
                        &message,
                        state.channel_info(channel),
                    )
                })
                .flatten()
            {
                Some(frame) => {
                    if let Some(user) = state.users.get(&session) {
                        user.link.send(frame);
                    }
                }
                None => state.send(session, kind::CHANNEL_STATE, &message),
            }
        }
    }
    for &id in &order {
        if let Some(channel) = state
            .channels
            .get(id)
            .filter(|channel| !channel.links.is_empty())
        {
            let message = proto::ChannelState {
                channel_id: Some(id),
                links: channel.links.iter().copied().collect(),
                ..Default::default()
            };
            state.send(session, kind::CHANNEL_STATE, &message);
        }
    }
    let mut entering = proto::UserState::default();
    // A new user starts nowhere; entering the channel computes suppression.
    if let Some(user) = state.users.get_mut(&session) {
        user.channel = u32::MAX;
    }
    state.enter_channel_initial(session, channel, &mut entering);
    let Some(user) = state.users.get(&session) else {
        return;
    };
    let mut announcement = user.state_message();
    announcement.listening_channel_add.clear();
    announcement.suppress = entering.suppress.or(announcement.suppress);
    state.broadcast(kind::USER_STATE, &announcement, None);
    let others: Vec<proto::UserState> = state
        .users
        .values()
        .filter(|other| other.session != session)
        .map(|other| {
            let mut message = other.state_message();
            if other.channel == 0 {
                message.channel_id = None;
            }
            message
        })
        .collect();
    for message in others {
        state.send(session, kind::USER_STATE, &message);
    }
    let permissions = state.sync_permissions(session);
    let settings = state.settings.clone();
    let config = &settings.config;
    // Stock clients are told that text is kept (docs/extension.md §9).
    let mut welcome = state.settings.welcome_text.clone();
    if !nuschel && config.history.notify_stock_clients {
        if !welcome.is_empty() {
            welcome.push_str(if config.allow_html {
                "<br/><br/>"
            } else {
                "\n\n"
            });
        }
        welcome.push_str(&crate::handlers::retention_text(
            config.history.default_retention(),
        ));
    }
    state.send(
        session,
        kind::SERVER_SYNC,
        &proto::ServerSync {
            session: Some(session),
            max_bandwidth: Some(config.max_bandwidth),
            welcome_text: (!welcome.is_empty()).then_some(welcome),
            permissions: Some(u64::from(permissions)),
        },
    );
    restore_listeners(state, session, listeners);
    state.send(
        session,
        kind::SERVER_CONFIG,
        &proto::ServerConfig {
            allow_html: Some(config.allow_html),
            message_length: Some(config.text_message_length),
            image_message_length: Some(config.image_message_length),
            max_users: Some(config.max_users),
            recording_allowed: Some(config.allow_recording),
            ..Default::default()
        },
    );
    let suggest = proto::SuggestConfig {
        version_v1: state.settings.suggest_version.map(|version| {
            ((version >> 48) as u32) << 16
                | (((version >> 32) & 0xff) as u32) << 8
                | ((version >> 16) & 0xff) as u32
        }),
        version_v2: state.settings.suggest_version,
        positional: config.suggest_positional,
        push_to_talk: config.suggest_push_to_talk,
    };
    if suggest != proto::SuggestConfig::default() {
        state.send(session, kind::SUGGEST_CONFIG, &suggest);
    }
    let old_client = state
        .users
        .get(&session)
        .is_some_and(|user| !user.info.at_least(1, 4, 0));
    let listeners_enabled =
        config.listeners_per_channel != Some(0) && config.listeners_per_user != Some(0);
    if old_client && listeners_enabled {
        let warning = if config.allow_html {
            "<b>[WARNING]</b>: This server has the <b>ChannelListener</b> feature enabled but your client version \
             does not support it. This means that users <b>might be listening to what you are saying in your \
             channel without you noticing!</b> You can solve this issue by upgrading to Mumble 1.4.0 or newer."
        } else {
            "[WARNING]: This server has the ChannelListener feature enabled but your client version does not \
             support it. This means that users might be listening to what you are saying in your channel without \
             you noticing! You can solve this issue by upgrading to Mumble 1.4.0 or newer."
        };
        state.text_to(session, warning);
    }
}

impl State {
    /// Place a new user in their first channel (the user starts outside any).
    fn enter_channel_initial(
        &mut self,
        session: u32,
        channel: u32,
        message: &mut proto::UserState,
    ) {
        let Some(user) = self.users.get_mut(&session) else {
            return;
        };
        user.channel = channel;
        if let Some(target) = self.channels.get_mut(channel) {
            target.users.push(session);
        }
        self.invalidate();
        let may_speak = self.allowed(session, Action::Speak { channel });
        if let Some(user) = self.users.get_mut(&session) {
            user.suppress = !may_speak;
            if user.suppress {
                message.suppress = Some(true);
            }
        }
        self.remember_channel(session);
        self.send_permissions(session, channel, false);
        if let Some(parent) = self
            .channels
            .get(channel)
            .and_then(crate::state::Channel::parent)
        {
            self.send_permissions(session, parent, false);
        }
        self.authz_changed(crate::conversations::Scope::Session(session));
    }
}

/// A freshly authenticated user (also used by the fuzzing harness).
#[allow(clippy::too_many_arguments)]
pub(crate) fn new_user(
    config: &crate::config::Config,
    session: u32,
    name: String,
    user_id: Option<u32>,
    peer: &Peer,
    message: &proto::Authenticate,
    crypt: CryptState,
    now: Instant,
) -> User {
    User {
        session,
        name,
        user_id,
        certificate_hash: peer.certificate_hash.clone(),
        certificates: peer.certificates.clone(),
        strong: peer.strong,
        address: peer.address,
        account: None,
        notified: Default::default(),
        extension: peer
            .negotiation
            .freeze()
            .ok()
            .flatten()
            .map(|capabilities| {
                let mut extension = crate::extension::Extension::new(capabilities, config, now);
                extension.relay_only = peer.negotiation.relay_only();
                Box::new(extension)
            }),
        info: peer.info.clone(),
        opus: message.opus(),
        celt_versions: message.celt_versions.iter().copied().take(32).collect(),
        bot: message.client_type() == 1,
        tokens: bounded_tokens(&message.tokens),
        channel: 0,
        mute: false,
        deaf: false,
        suppress: false,
        self_mute: false,
        self_deaf: false,
        priority_speaker: false,
        recording: false,
        comment: String::new(),
        comment_hash: None,
        texture: Bytes::new(),
        texture_hash: None,
        plugin_context: Vec::new(),
        plugin_identity: String::new(),
        listening: Default::default(),
        targets: HashMap::new(),
        target_cache: RefCell::new(HashMap::new()),
        voice: Voice {
            crypt,
            udp: None,
            udp_active: true,
            last_good: now,
            last_resync_request: None,
            remote: Default::default(),
            bandwidth: BandwidthRecord::new(now),
            udp_ping: (0., 0., 0),
            tcp_ping: (0., 0., 0),
            failures: LeakyBucket::new(100, 400, now),
            blocked_until: None,
        },
        link: peer.link.clone(),
        rate: LeakyBucket::new(config.message_limit, config.message_burst, now),
        plugin_rate: LeakyBucket::new(
            config.plugin_message_limit,
            config.plugin_message_burst,
            now,
        ),
        flood: LeakyBucket::new(100, 1000, now),
        queries: LeakyBucket::new(20, 200, now),
        permission_sent: HashMap::new(),
        last_permission_check: None,
        last_activity: now,
        fanout_notice: None,
    }
}
