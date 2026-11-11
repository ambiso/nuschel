// SPDX-License-Identifier: EUPL-1.2
//! A scriptable real client for diagnostics and stock-server interoperability tests.
use clap::Parser;
use nuschel_protocol::voice::VoicePacket;
use nuschel_session::{
    Error,
    config::{Profile, ServerBookmark},
    connection::{self, Command, ConnectOptions, MessageRate, SessionEvent, TextTarget},
    model::{UserAction, VoiceTargetSpec},
};
use serde::Deserialize;
use serde_json::json;
use std::{
    io::{self, BufRead, Read, Write},
    path::PathBuf,
};
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(
    version,
    about = "Mumble client: newline-delimited JSON commands on stdin and events on stdout"
)]
struct Arguments {
    #[arg(long)]
    host: String,
    #[arg(long, default_value_t = 64738)]
    port: u16,
    #[arg(long)]
    username: String,
    #[arg(long)]
    password_file: Option<PathBuf>,
    /// Explicit SHA-256 pin, useful for an independently verified self-signed certificate.
    #[arg(long)]
    certificate_sha256: Option<String>,
    #[arg(long)]
    profile_dir: Option<PathBuf>,
    /// Keep voice in the TLS tunnel instead of encrypted UDP.
    #[arg(long)]
    force_tcp: bool,
    /// Access token (repeatable).
    #[arg(long = "token")]
    tokens: Vec<String>,
    /// Import a PKCS#12 client certificate (e.g. exported by Mumble) into the profile first.
    #[arg(long)]
    import_certificate: Option<PathBuf>,
    #[arg(long, default_value = "")]
    certificate_password: String,
    /// Export the profile's client certificate as PKCS#12 (for Mumble) and exit.
    #[arg(long)]
    export_certificate: Option<PathBuf>,
    /// The server's text rate limit (murmur.ini `messagelimit`, per second);
    /// text waits for it instead of being dropped by the server.
    #[arg(long, default_value_t = MessageRate::default().per_second)]
    message_limit: u32,
    /// The server's text burst (murmur.ini `messageburst`).
    #[arg(long, default_value_t = MessageRate::default().burst)]
    message_burst: u32,
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Input {
    CreateChannel {
        parent: u32,
        name: String,
        #[serde(default = "yes")]
        temporary: bool,
    },
    Register,
    Html {
        channel: u32,
        html: String,
    },
    Avatar {
        path: PathBuf,
    },
    Comment {
        text: String,
    },
    Tokens {
        tokens: Vec<String>,
    },
    Listen {
        channel: u32,
        listen: bool,
    },
    Link {
        channel: u32,
        #[serde(default)]
        add: Vec<u32>,
        #[serde(default)]
        remove: Vec<u32>,
    },
    Whisper {
        id: u32,
        #[serde(default)]
        sessions: Vec<u32>,
        channel: Option<u32>,
        #[serde(default)]
        children: bool,
    },
    Permissions {
        channel: u32,
    },
    Stats {
        session: u32,
    },
    Blobs {
        #[serde(default)]
        textures: Vec<u32>,
        #[serde(default)]
        comments: Vec<u32>,
        #[serde(default)]
        descriptions: Vec<u32>,
    },
    Moderate {
        session: u32,
        action: String,
        reason: Option<String>,
        channel: Option<u32>,
    },
    RegisteredUsers,
    Bans,
    Acl {
        channel: u32,
    },
    Join {
        channel: u32,
    },
    Text {
        channel: u32,
        body: String,
    },
    Direct {
        session: u32,
        body: String,
    },
    Mute {
        muted: bool,
        #[serde(default)]
        deafened: bool,
    },
    Voice {
        #[serde(default)]
        target: u32,
        sequence: u64,
        opus: Vec<u8>,
        #[serde(default)]
        end: bool,
    },
    Disconnect,
}
fn yes() -> bool {
    true
}
fn output(value: serde_json::Value) -> io::Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, &value)?;
    writeln!(out)?;
    out.flush()
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = Arguments::parse();
    let profile = Profile::open(args.profile_dir)?;
    if let Some(path) = &args.export_certificate {
        profile
            .export_identity_to(path, &args.certificate_password)
            .map_err(io::Error::other)?;
        output(json!({"kind":"certificate_exported", "hash":profile.identity()?.mumble_hash()}))?;
        return Ok(());
    }
    if let Some(path) = &args.import_certificate {
        let bytes = std::fs::read(path)?;
        let identity = profile
            .import_identity(&bytes, &args.certificate_password)
            .map_err(io::Error::other)?;
        output(json!({"kind":"certificate_imported", "hash":identity.mumble_hash()}))?;
    }
    let mut server =
        ServerBookmark::new("", &args.host, args.port, &args.username).map_err(io::Error::other)?;
    if let Some(pin) = &args.certificate_sha256
        && (pin.len() != 64 || !pin.bytes().all(|c| c.is_ascii_hexdigit()))
    {
        return Err("expected a 64-character SHA-256 certificate fingerprint".into());
    }
    server.certificate_sha256 = args.certificate_sha256;
    server.tokens = args.tokens;
    let password = args
        .password_file
        .map(std::fs::read_to_string)
        .transpose()?
        .map(|p| p.trim_end_matches(['\r', '\n']).to_owned());
    let options = ConnectOptions {
        server,
        identity: profile.identity()?,
        password,
        force_tcp: args.force_tcp,
        muted: true,
        deafened: false,
        channel: None,
        message_rate: MessageRate {
            burst: args.message_burst,
            per_second: args.message_limit,
        },
    };
    let (tx, rx) = mpsc::channel(32);
    let (events_tx, mut events) = mpsc::channel(64);
    std::thread::spawn(move || {
        let mut input = io::stdin().lock();
        loop {
            let mut line = Vec::new();
            match Read::by_ref(&mut input)
                .take(128 * 1024)
                .read_until(b'\n', &mut line)
            {
                Ok(0) | Err(_) => break,
                Ok(_) if line.len() >= 128 * 1024 => {
                    let _ =
                        output(json!({"kind":"error", "message":"command exceeds input limit"}));
                    break;
                }
                _ => {}
            }
            let command = match serde_json::from_slice::<Input>(&line) {
                Ok(Input::CreateChannel {
                    parent,
                    name,
                    temporary,
                }) => Command::CreateChannel {
                    parent,
                    name,
                    temporary,
                },
                Ok(Input::Register) => Command::RegisterSelf,
                Ok(Input::Html { channel, html }) => Command::SendHtml {
                    target: TextTarget::Channel(channel),
                    html: vec![html],
                    expected_peer: None,
                },
                Ok(Input::Avatar { path }) => match std::fs::read(&path) {
                    Ok(bytes) if bytes.len() <= 1024 * 1024 => Command::SetAvatar(bytes),
                    _ => {
                        let _ = output(json!({"kind":"error", "message":"cannot read avatar"}));
                        continue;
                    }
                },
                Ok(Input::Comment { text }) => Command::SetComment(text),
                Ok(Input::Tokens { tokens }) => Command::SetTokens(tokens),
                Ok(Input::Listen { channel, listen }) => Command::Listen { channel, listen },
                Ok(Input::Link {
                    channel,
                    add,
                    remove,
                }) => Command::LinkChannels {
                    channel,
                    add,
                    remove,
                },
                Ok(Input::Whisper {
                    id,
                    sessions,
                    channel,
                    children,
                }) => Command::SetVoiceTarget {
                    id,
                    targets: match channel {
                        Some(channel) => vec![VoiceTargetSpec::Channel {
                            channel,
                            links: false,
                            children,
                            group: None,
                        }],
                        None => vec![VoiceTargetSpec::Users(sessions)],
                    },
                },
                Ok(Input::Permissions { channel }) => Command::RequestPermissions(channel),
                Ok(Input::Stats { session }) => Command::RequestStats(session),
                Ok(Input::Blobs {
                    textures,
                    comments,
                    descriptions,
                }) => Command::RequestBlobs {
                    textures,
                    comments,
                    descriptions,
                },
                Ok(Input::Moderate {
                    session,
                    action,
                    reason,
                    channel,
                }) => {
                    let action = match action.as_str() {
                        "kick" => UserAction::Kick,
                        "ban" => UserAction::Ban,
                        "mute" => UserAction::Mute(true),
                        "unmute" => UserAction::Mute(false),
                        "deafen" => UserAction::Deafen(true),
                        "undeafen" => UserAction::Deafen(false),
                        "priority" => UserAction::PrioritySpeaker(true),
                        "move" => UserAction::Move(channel.unwrap_or(0)),
                        "register" => UserAction::Register,
                        "reset_comment" => UserAction::ResetComment,
                        "reset_avatar" => UserAction::ResetAvatar,
                        other => {
                            let _ = output(
                                json!({"kind":"error", "message":format!("unknown action {other}")}),
                            );
                            continue;
                        }
                    };
                    Command::User {
                        session,
                        action,
                        reason,
                    }
                }
                Ok(Input::RegisteredUsers) => Command::QueryRegisteredUsers,
                Ok(Input::Bans) => Command::QueryBans,
                Ok(Input::Acl { channel }) => Command::QueryAcl(channel),
                Ok(Input::Join { channel }) => Command::Join(channel),
                Ok(Input::Text { channel, body }) => Command::SendText {
                    target: TextTarget::Channel(channel),
                    body,
                    expected_peer: None,
                },
                Ok(Input::Direct { session, body }) => Command::SendText {
                    target: TextTarget::Direct(session),
                    body,
                    expected_peer: None,
                },
                Ok(Input::Mute { muted, deafened }) => Command::SetSelf { muted, deafened },
                Ok(Input::Voice {
                    target,
                    sequence,
                    opus,
                    end,
                }) => Command::Voice {
                    target,
                    packet: VoicePacket {
                        sender: 0,
                        sequence,
                        opus,
                        end,
                        volume: 1.,
                    },
                },
                Ok(Input::Disconnect) => Command::Disconnect,
                Err(error) => {
                    let _ = output(json!({"kind":"error", "message":error.to_string()}));
                    continue;
                }
            };
            if tx.blocking_send(command).is_err() {
                break;
            }
        }
    });
    let mut session = tokio::spawn(connection::run(options, rx, events_tx));
    loop {
        tokio::select! {
            result = &mut session => match result? {
                Ok(()) => break,
                Err(Error::Certificate { sha256, changed, reason }) => {
                    output(json!({"kind":"certificate_required", "sha256":sha256, "changed":changed, "reason":reason}))?;
                    return Err("server certificate requires explicit confirmation".into());
                },
                Err(Error::Rejected { kind, reason }) => { output(json!({"kind":"rejected", "reject":kind, "message":reason}))?; return Err("rejected".into()); },
                Err(error) => { output(json!({"kind":"disconnected", "message":error.to_string()}))?; return Err(error.into()); },
            },
            Some(event) = events.recv() => output(match event {
                SessionEvent::PeerCertificate(sha256) => json!({"kind":"peer_certificate", "sha256":sha256}),
                SessionEvent::Channel(channel) => json!({"kind":"channel", "channel":channel}),
                SessionEvent::ChannelRemoved(id) => json!({"kind":"channel_removed", "channel":id}),
                SessionEvent::User(user) => json!({"kind":"user", "user":user}),
                SessionEvent::UserRemoved(session) => json!({"kind":"user_removed", "session":session}),
                SessionEvent::Connected { session, max_bandwidth } => json!({"kind":"connected", "session":session, "max_bandwidth":max_bandwidth}),
                SessionEvent::Text(message) => json!({"kind":"text", "target":message.target, "author":message.author, "body":message.body, "html":message.html, "outgoing":message.outgoing}),
                SessionEvent::Welcome(body) => json!({"kind":"welcome", "body":body}),
                SessionEvent::Notice(message) => json!({"kind":"notice", "message":message}),
                SessionEvent::PermissionDenied(message) => json!({"kind":"permission_denied", "message":message}),
                SessionEvent::Audio(packet) => json!({"kind":"audio", "sender":packet.sender, "sequence":packet.sequence, "opus":packet.opus, "end":packet.end}),
                SessionEvent::Transport(stats) => json!({"kind":"transport", "stats":stats}),
                SessionEvent::Permissions { channel, permissions, flush } => json!({"kind":"permissions", "channel":channel, "permissions":permissions, "flush":flush}),
                SessionEvent::Activity(activity) => json!({"kind":"activity", "activity":activity}),
                SessionEvent::Limits(limits) => json!({"kind":"limits", "limits":limits}),
                SessionEvent::Stats(stats) => json!({"kind":"stats", "stats":stats}),
                SessionEvent::Suggest { push_to_talk, positional, minimum_version } => json!({"kind":"suggest", "push_to_talk":push_to_talk, "positional":positional, "minimum_version":minimum_version}),
                SessionEvent::ContextActions(actions) => json!({"kind":"context_actions", "actions":actions}),
                SessionEvent::RegisteredUsers(users) => json!({"kind":"registered_users", "users":users}),
                SessionEvent::Bans(bans) => json!({"kind":"bans", "bans":bans}),
                SessionEvent::Acl(acl) => json!({"kind":"acl", "acl":acl}),
                SessionEvent::UserNames(names) => json!({"kind":"user_names", "names":names}),
                SessionEvent::UnsupportedCodec { session } => json!({"kind":"unsupported_codec", "session":session}),
                SessionEvent::Outbox { sent, total } => json!({"kind":"outbox", "sent":sent, "total":total}),
            })?,
        }
    }
    output(json!({"kind":"disconnected", "message":"Disconnected"}))?;
    Ok(())
}
