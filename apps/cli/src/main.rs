// SPDX-License-Identifier: EUPL-1.2
//! A scriptable real client for diagnostics and stock-server interoperability tests.
use clap::Parser;
use nuschel_protocol::voice::VoicePacket;
use nuschel_session::{
    Error,
    config::{Profile, ServerBookmark},
    connection::{self, Command, ConnectOptions, SessionEvent, TextTarget},
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
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Input {
    CreateChannel {
        parent: u32,
        name: String,
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
    let mut server =
        ServerBookmark::new("", &args.host, args.port, &args.username).map_err(io::Error::other)?;
    if let Some(pin) = &args.certificate_sha256
        && (pin.len() != 64 || !pin.bytes().all(|c| c.is_ascii_hexdigit()))
    {
        return Err("expected a 64-character SHA-256 certificate fingerprint".into());
    }
    server.certificate_sha256 = args.certificate_sha256;
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
                Ok(Input::CreateChannel { parent, name }) => {
                    Command::CreateChannel { parent, name }
                }
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
            })?,
        }
    }
    output(json!({"kind":"disconnected", "message":"Disconnected"}))?;
    Ok(())
}
