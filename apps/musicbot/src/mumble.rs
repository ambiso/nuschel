// SPDX-License-Identifier: EUPL-1.2
//! One Mumble connection per configured channel: joins the channel, plays
//! the packets of its player while somebody listens, and answers `!web`.
use crate::{
    Bot,
    player::{Changes, Subscription, bitrate_for_bandwidth},
    tokens::Owner,
};
use nuschl_core::audio::SILENT_OPUS;
use nuschl_legacy_html::escape_html;
use nuschl_protocol::voice::VoicePacket;
use nuschl_session::{
    config::{Profile, ServerBookmark},
    connection::{
        self, Channel, Command, ConnectOptions, IncomingText, MessageRate, SessionEvent,
        TextTarget, User,
    },
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::{
    sync::{broadcast::error::RecvError, mpsc, watch},
    time::{Instant, sleep},
};

/// What the web page shows about a connection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub connected: bool,
    /// The channel the bot is in (somebody may have moved it).
    pub channel: Option<String>,
    /// People in the channel who can hear the bot.
    pub listeners: usize,
    pub error: Option<String>,
}

pub struct Connection {
    pub name: String,
    /// `host:port`.
    pub server: String,
    /// The index of the player this channel plays.
    pub player: watch::Sender<usize>,
    status: Mutex<Status>,
}

impl Connection {
    pub fn new(name: String, server: String, player: usize) -> Self {
        Self {
            name,
            server,
            player: watch::Sender::new(player),
            status: Mutex::default(),
        }
    }

    pub fn status(&self) -> MutexGuard<'_, Status> {
        self.status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Changes the status, telling the web page's viewers when it differs.
    fn update(&self, changes: &Changes, update: impl FnOnce(&mut Status)) {
        let mut status = self.status();
        let before = status.clone();
        update(&mut status);
        if *status != before {
            changes.notify();
        }
    }
}

/// Keeps the connection up, reconnecting with growing pauses.
pub async fn run(bot: Arc<Bot>, index: usize) {
    let mut pause = Duration::from_secs(1);
    let mut detector = nuschl_ipc::musicbot::Client::new(
        nuschl_ipc::musicbot::Role::Duck,
        bot.codec_worker.clone(),
    );
    loop {
        let started = Instant::now();
        let error = match session(&bot, index, &mut detector).await {
            Ok(()) => "disconnected".to_owned(),
            Err(error) => error,
        };
        if let Err(error) = detector.close().await {
            bot.connections[index].update(&bot.changes, |status| {
                *status = Status {
                    error: Some(error),
                    ..Status::default()
                };
            });
            // A child whose exit is unconfirmed still owns this connection's
            // process credit: never reconnect with a second detector owner.
            loop {
                sleep(Duration::from_secs(60)).await;
            }
        }
        let connection = &bot.connections[index];
        tracing::warn!(
            "{}: {error}; connecting again in {}s",
            connection.name,
            pause.as_secs()
        );
        connection.update(&bot.changes, |status| {
            *status = Status {
                error: Some(error),
                ..Status::default()
            }
        });
        if started.elapsed() > Duration::from_secs(60) {
            pause = Duration::from_secs(1);
        }
        sleep(pause).await;
        pause = (pause * 2).min(Duration::from_secs(60));
    }
}

/// Server state the bot follows.
#[derive(Default)]
struct View {
    channels: HashMap<u32, Channel>,
    users: HashMap<u32, User>,
    own: Option<u32>,
    max_bandwidth: u32,
    /// What went wrong on this connection, for the web page.
    problem: Option<String>,
}

impl View {
    fn own_channel(&self) -> Option<u32> {
        self.own
            .and_then(|own| self.users.get(&own))
            .map(|user| user.channel)
    }

    fn listeners(&self) -> usize {
        let Some(channel) = self.own_channel() else {
            return 0;
        };
        self.users
            .values()
            .filter(|user| {
                Some(user.session) != self.own
                    && (user.channel == channel || user.listening.contains(&channel))
                    && !user.deafened
                    && !user.self_deafened
            })
            .count()
    }

    /// The channel at `path` below the root (the channel without a parent).
    fn find(&self, path: &[&str]) -> Option<u32> {
        let mut current = self
            .channels
            .values()
            .find(|channel| channel.parent.is_none())?
            .id;
        for segment in path {
            current = self
                .channels
                .values()
                .find(|channel| channel.parent == Some(current) && channel.name == *segment)?
                .id;
        }
        Some(current)
    }

    /// The path of a channel below the root, as `channel` is configured.
    fn path_of(&self, mut id: u32) -> String {
        let mut names = Vec::new();
        while let Some(channel) = self.channels.get(&id) {
            let Some(parent) = channel.parent else { break };
            names.push(channel.name.as_str());
            // A hostile server could send a parent cycle.
            if names.len() > self.channels.len() {
                break;
            }
            id = parent;
        }
        names.reverse();
        names.join("/")
    }

    /// Paths of channels named like the last segment of `path` (ignoring
    /// case and surrounding spaces), for a configured path that was not found.
    fn similar(&self, path: &[&str]) -> Vec<String> {
        let Some(last) = path.last() else {
            return Vec::new();
        };
        let mut found: Vec<String> = self
            .channels
            .values()
            .filter(|channel| {
                channel.parent.is_some() && channel.name.trim().eq_ignore_ascii_case(last.trim())
            })
            .map(|channel| self.path_of(channel.id))
            .collect();
        found.sort();
        found.truncate(5);
        found
    }
}

/// If the parent future itself is dropped, request cancellation as a last resort.
/// Normal returns additionally join the actor before the reconnect loop continues.
struct AbortSession(tokio::task::AbortHandle);
impl Drop for AbortSession {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn session(
    bot: &Arc<Bot>,
    index: usize,
    detector: &mut nuschl_ipc::musicbot::Client,
) -> Result<(), String> {
    let config = &bot.config.channels[index];
    let connection = &bot.connections[index];
    let profile = Profile::open(Some(
        bot.state_directory.join("identities").join(&config.name),
    ))
    .map_err(|error| format!("profile: {error}"))?;
    let identity = profile
        .identity()
        .map_err(|error| format!("client certificate: {error}"))?;
    let mut server =
        ServerBookmark::new(&config.name, &config.host, config.port, &config.username)?;
    server.certificate_sha256 = config.certificate_sha256.clone();
    server.tokens = config.access_tokens()?;
    let password = config
        .password_file
        .as_ref()
        .map(std::fs::read_to_string)
        .transpose()
        .map_err(|error| format!("password file: {error}"))?
        .map(|password| password.trim_end_matches(['\r', '\n']).to_owned());
    let options = ConnectOptions {
        voice_route: Default::default(),
        server,
        identity,
        password,
        force_tcp: config.force_tcp,
        relay_only: false,
        muted: false,
        deafened: false,
        channel: None,
        resend: Vec::new(),
        message_rate: MessageRate::default(),
        required: Default::default(),
        crypto: None,
    };
    let (commands, receiver) = mpsc::channel(64);
    let (events_sender, mut events) = mpsc::channel(256);
    let mut running = tokio::spawn(connection::run(options, receiver, events_sender));
    let _abort_on_exit = AbortSession(running.abort_handle());
    let mut attached = connection.player.subscribe();
    let mut view = View::default();
    let mut subscription: Option<Subscription> = None;
    let mut sequence = 0u64;
    let mut completed = false;
    let result = async {
    loop {
        let wanted = (view.own.is_some()
            && view.listeners() > 0
            && bitrate_for_bandwidth(bot.config.audio.bitrate, view.max_bandwidth) > 0)
            .then(|| *attached.borrow());
        if subscription.as_ref().map(|s| s.player) != wanted {
            if subscription.take().is_some() {
                // Close a transmission cut off by a switch or the last listener leaving.
                voice(&commands, &mut sequence, SILENT_OPUS.to_vec(), true);
            }
            subscription = wanted
                .map(|player| bot.players[player].subscribe(player, index, view.max_bandwidth));
        }
        tokio::select! {
            result = &mut running => {
                completed = true;
                return match result {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(error)) => {
                        tracing::warn!("{}: session failed: {error}", connection.name);
                        Err("The Mumble connection failed.".to_owned())
                    },
                    Err(error) => {
                        tracing::warn!("{}: session task failed: {error}", connection.name);
                        Err("The Mumble session task failed.".to_owned())
                    },
                };
            }
            event = events.recv() => {
                let Some(event) = event else { continue };
                if let SessionEvent::Audio(packet) = &event {
                    if Some(packet.sender) == view.own {
                        continue;
                    }
                    // Somebody speaks where a queue with ducking plays: duck it.
                    let ducking = subscription
                        .as_ref()
                        .map(|subscription| (subscription.player, bot.players[subscription.player].state().ducking))
                        .filter(|(_, ducking)| ducking.enabled);
                    match ducking {
                        Some((player, ducking)) => {
                            if detector.hear(packet.sender, packet.opus.clone(), nuschl_ipc::musicbot::speech_threshold(ducking.sensitivity)).await? {
                                bot.players[player].heard_speech();
                            }
                        }
                        // Nothing to duck: nothing to decode.
                        None => detector.clear().await?,
                    }
                    continue;
                }
                if let SessionEvent::UserRemoved(session) = &event {
                    detector.forget(*session).await?;
                }
                let previous_bandwidth = view.max_bandwidth;
                handle(bot, index, &commands, &mut view, event);
                if view.max_bandwidth != previous_bandwidth
                    && let Some(subscription) = &subscription
                {
                    bot.players[subscription.player].update_bandwidth(index, view.max_bandwidth);
                }
                connection.update(&bot.changes, |status| {
                    status.connected = view.own.is_some();
                    status.listeners = view.listeners();
                    status.channel = view
                        .own_channel()
                        .and_then(|id| view.channels.get(&id))
                        .map(|channel| channel.name.clone());
                    if status.connected {
                        status.error = view.problem.clone().or_else(|| {
                            (status.listeners > 0
                                && bitrate_for_bandwidth(bot.config.audio.bitrate, view.max_bandwidth) == 0)
                                .then(|| "The server bandwidth is too low to transmit music.".to_owned())
                        });
                    }
                });
            }
            changed = attached.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
            }
            packet = next_packet(&mut subscription) => match packet {
                Ok(packet) => voice(&commands, &mut sequence, packet.opus.clone(), packet.end),
                // A slow connection drops what it could not send in time.
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => subscription = None,
            },
        }
    }
    }.await;
    if !completed {
        // An abort request is not completion. Retain the connection slot until
        // the task has actually stopped, including on a detector IPC error.
        running.abort();
        let _ = running.await;
    }
    result
}

async fn next_packet(
    subscription: &mut Option<Subscription>,
) -> Result<Arc<crate::player::Packet>, RecvError> {
    match subscription {
        Some(subscription) => subscription.packets.recv().await,
        None => std::future::pending().await,
    }
}

/// Sends a voice packet; when the session is busy, the packet is dropped
/// rather than delayed.
fn voice(commands: &mpsc::Sender<Command>, sequence: &mut u64, opus: Vec<u8>, end: bool) {
    let packet = VoicePacket {
        sender: 0,
        sequence: *sequence,
        opus,
        end,
        volume: 1.0,
    };
    // The sequence counts 10 ms units; a packet holds 20 ms.
    *sequence += 2;
    let _ = commands.try_send(Command::Voice { target: 0, packet });
}

fn handle(
    bot: &Arc<Bot>,
    index: usize,
    commands: &mpsc::Sender<Command>,
    view: &mut View,
    event: SessionEvent,
) {
    let config = &bot.config.channels[index];
    match event {
        SessionEvent::Channel(channel) => {
            view.channels.insert(channel.id, channel);
        }
        SessionEvent::ChannelRemoved(id) => {
            view.channels.remove(&id);
        }
        SessionEvent::User(user) => {
            view.users.insert(user.session, user);
        }
        SessionEvent::UserRemoved(session) => {
            view.users.remove(&session);
        }
        SessionEvent::Limits(limits) => {
            if let Some(bandwidth) = limits.max_bandwidth {
                view.max_bandwidth = bandwidth;
            }
        }
        SessionEvent::Connected {
            session,
            max_bandwidth,
        } => {
            view.own = Some(session);
            view.max_bandwidth = max_bandwidth;
            tracing::info!(
                "{}: connected to {}:{}",
                config.name,
                config.host,
                config.port
            );
            let url = escape_html(&format!(
                "{}/",
                bot.config.web.public_url.trim_end_matches('/')
            ));
            let comment = format!(
                "<p>Nuschl music bot</p><p>Web interface: <a href=\"{url}\">{url}</a></p>\
                 <p>Send me <code>!web</code> in a private message for your personal \
                 playback, queue and history link. Keep it private. \
                 <code>!web new</code> replaces it.</p>"
            );
            if let Err(error) = commands.try_send(Command::SetComment(comment)) {
                tracing::warn!("{}: cannot publish bot comment: {error}", config.name);
            }
            match view.find(&config.path()) {
                Some(channel) if Some(channel) != view.own_channel() => {
                    let _ = commands.try_send(Command::Join(channel));
                }
                Some(_) => {}
                None => {
                    let similar = view.similar(&config.path());
                    let hint = match similar.as_slice() {
                        [] => String::new(),
                        paths => format!(
                            " Channels are paths from the root; did you mean {}?",
                            paths
                                .iter()
                                .map(|path| format!("{path:?}"))
                                .collect::<Vec<_>>()
                                .join(" or ")
                        ),
                    };
                    tracing::warn!(
                        "{}: there is no channel {:?}.{hint}",
                        config.name,
                        config.channel
                    );
                    view.problem = Some(format!(
                        "There is no channel {:?} on this server.{hint}",
                        config.channel
                    ));
                }
            }
        }
        SessionEvent::Text(text) if !text.outgoing => answer(bot, index, commands, text),
        SessionEvent::PermissionDenied(reason) => {
            // Typically: the bot may not enter its channel, or not speak there.
            tracing::warn!("{}: permission denied: {reason}", config.name);
            view.problem = Some("The server denied a requested action.".to_owned());
        }
        SessionEvent::Notice(notice) => tracing::info!("{}: {notice}", config.name),
        _ => {}
    }
}

/// `!web` sends the user their link privately; `!web new` replaces it.
fn answer(bot: &Arc<Bot>, index: usize, commands: &mpsc::Sender<Command>, text: IncomingText) {
    let words = plain_text(&text.body);
    let mut words = words.split_whitespace();
    if words.next() != Some("!web") {
        return;
    }
    let renew = words.next() == Some("new");
    let Some(peer) = text.peer else { return };
    let owner = Owner {
        server: bot.connections[index].server.clone(),
        name: peer.name.clone(),
        certificate: peer.certificate_hash.clone(),
    };
    let body = match bot.tokens.issue(owner, renew) {
        Ok(token) => {
            let link = format!(
                "{}/u/{token}/",
                bot.config.web.public_url.trim_end_matches('/')
            );
            if peer.certificate_hash.is_some() {
                format!(
                    "Your link to the music queue: {link} (keep it to yourself; \"!web new\" replaces it)"
                )
            } else {
                format!("Your link to the music queue: {link} (keep it to yourself)")
            }
        }
        Err(error) => {
            tracing::error!("cannot save the links: {error}");
            "Sorry, I cannot make a link right now.".to_owned()
        }
    };
    let _ = commands.try_send(Command::SendText {
        target: TextTarget::Direct(peer.session),
        body,
        expected_peer: Some(peer.instance),
    });
}

/// The text of a message that may be Mumble HTML: tags dropped, the common
/// entities decoded.
fn plain_text(body: &str) -> String {
    let mut text = String::with_capacity(body.len());
    let mut in_tag = false;
    for c in body.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                text.push(' ');
            }
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    text.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(id: u32, parent: Option<u32>, name: &str) -> Channel {
        Channel {
            id,
            parent,
            name: name.into(),
            ..Channel::default()
        }
    }

    fn user(session: u32, channel: u32) -> User {
        User {
            session,
            channel,
            ..User::default()
        }
    }

    #[test]
    fn channel_paths_resolve_from_the_root() {
        let mut view = View::default();
        for channel in [
            channel(0, None, "Root"),
            channel(1, Some(0), "Games"),
            channel(2, Some(1), "Music"),
            channel(3, Some(0), "Music"),
        ] {
            view.channels.insert(channel.id, channel);
        }
        assert_eq!(view.find(&[]), Some(0));
        assert_eq!(view.find(&["Music"]), Some(3));
        assert_eq!(view.find(&["Games", "Music"]), Some(2));
        assert_eq!(view.find(&["Games", "Nope"]), None);
        // A subchannel's name alone is not a path; the hint names the full paths.
        assert_eq!(view.find(&["Shooters"]), None);
        view.channels.insert(4, channel(4, Some(1), "Shooters"));
        assert_eq!(view.similar(&["shooters "]), ["Games/Shooters"]);
        assert_eq!(view.similar(&["Music"]), ["Games/Music", "Music"]);
        assert!(view.similar(&["Nope"]).is_empty());
    }

    #[test]
    fn listeners_exclude_the_bot_and_deafened_users() {
        let mut view = View {
            own: Some(1),
            ..View::default()
        };
        view.users.insert(1, user(1, 5));
        view.users.insert(2, user(2, 5));
        view.users.insert(3, user(3, 6));
        let mut deaf = user(4, 5);
        deaf.self_deafened = true;
        view.users.insert(4, deaf);
        assert_eq!(view.listeners(), 1);
    }

    #[test]
    fn channel_subscriptions_count_remote_users_once() {
        let mut view = View {
            own: Some(1),
            ..View::default()
        };
        let mut bot = user(1, 5);
        bot.listening.insert(5);
        view.users.insert(1, bot);
        let mut resident = user(2, 5);
        resident.listening.insert(5);
        view.users.insert(2, resident);
        let mut remote = user(3, 6);
        remote.listening.insert(5);
        view.users.insert(3, remote);
        let mut elsewhere = user(4, 7);
        elsewhere.listening.insert(6);
        view.users.insert(4, elsewhere);
        assert_eq!(view.listeners(), 2);
        view.users.get_mut(&3).unwrap().self_deafened = true;
        assert_eq!(view.listeners(), 1);
        view.users.get_mut(&3).unwrap().self_deafened = false;
        view.users.get_mut(&3).unwrap().deafened = true;
        assert_eq!(view.listeners(), 1);
        view.users.get_mut(&3).unwrap().deafened = false;
        view.users.get_mut(&3).unwrap().listening.remove(&5);
        assert_eq!(view.listeners(), 1);
    }

    #[test]
    fn commands_in_mumble_html() {
        assert_eq!(
            plain_text("<p>!web&nbsp;new</p>")
                .split_whitespace()
                .collect::<Vec<_>>(),
            ["!web", "new"]
        );
        assert_eq!(plain_text("a &lt;b&gt; &amp;"), "a <b> &");
    }
}
