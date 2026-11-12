// SPDX-License-Identifier: EUPL-1.2
//! A scriptable real client for diagnostics and stock-server interoperability tests.
use clap::Parser;
use nuschl_protocol::{nuschl::v1, voice::VoicePacket};
use nuschl_session::{
    Error,
    config::{Profile, ServerBookmark},
    connection::{
        self, Command, ConnectOptions, MessageRate, NuschlEvent, NuschlRequest, SessionEvent,
        TextTarget,
    },
    identity::{CryptoHandle, IdentityCommand, IdentityEvent, IdentitySetup},
    mls::{MlsCommand, MlsEvent},
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
    /// Ask privacy/1 servers not to disclose this connection's IP to other clients.
    #[arg(long)]
    relay_only: bool,
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
    /// Refuse to log in unless the server speaks the Nuschl protocol (what an
    /// invite's `policy=nuschl` or an earlier connection would require).
    #[arg(long)]
    require_nuschl: bool,
    /// The crypto worker's directory: publish and check identities on servers with
    /// key transparency (`kt/1`). The worker runs in its sandbox, beside this binary.
    #[arg(long)]
    identity_dir: Option<PathBuf>,
    /// Require this key-transparency witness (a C2SP verifier key from
    /// `nuschl-witness vkey`) of every server; repeatable (docs/witness.md).
    #[arg(long = "kt-witness")]
    kt_witnesses: Vec<String>,
    /// How old the witnesses' cosignatures may be, in hours.
    #[arg(long, default_value_t = 24)]
    kt_witness_max_age_hours: u32,
    /// Do not connect: serve device-linking commands (`link_offer`, `link_join`,
    /// `link_approve`, `link_complete`) with the crypto worker of `--identity-dir`
    /// only. `--host` and `--username` are not used.
    #[arg(long)]
    offline: bool,
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
    /// Nuschl servers: send to a conversation (32 hex digits); `key` makes a
    /// retry idempotent.
    ChatSend {
        request_id: u64,
        conversation: String,
        content: String,
        key: Option<String>,
    },
    /// Nuschl servers: a history page; `before`/`after`/`around` a seq, else the latest.
    History {
        request_id: u64,
        conversation: String,
        before: Option<u64>,
        after: Option<u64>,
        around: Option<u64>,
        #[serde(default)]
        limit: u32,
    },
    Subscribe {
        request_id: u64,
        conversations: Vec<String>,
    },
    /// Nuschl servers: replace our message `target` (a seq) in a conversation.
    ChatEdit {
        request_id: u64,
        conversation: String,
        target: u64,
        content: String,
    },
    /// Nuschl servers: delete message `target` (ours, or as a moderator).
    ChatDelete {
        request_id: u64,
        conversation: String,
        target: u64,
    },
    /// Nuschl servers: we are typing in a conversation.
    Typing {
        conversation: String,
    },
    /// Nuschl servers: open (or create) the direct conversation with a
    /// registered user, and list ours.
    OpenDirect {
        request_id: u64,
        user_id: u32,
    },
    ListDirect {
        request_id: u64,
    },
    /// Nuschl servers: we read a conversation up to `seq`.
    ReadMarker {
        conversation: String,
        seq: u64,
    },
    /// Nuschl servers: text to a direct conversation (32 hex digits), sent
    /// like the desktop's (paced, idempotent, acknowledged).
    DirectText {
        conversation: String,
        body: String,
    },
    /// Key transparency (with `--identity-dir`): check a registered user.
    IdentityCheck {
        user_id: u32,
        #[serde(default)]
        direct: bool,
        /// The name the server shows for them, checked against its `name:` label.
        #[serde(default)]
        name: Option<String>,
    },
    /// Mark an account (32 hex digits) verified, or accept its new identity.
    IdentityVerify {
        account: String,
    },
    IdentityAccept {
        account: String,
    },
    IdentityOwn,
    /// End-to-end encryption (`mls/1`, with `--identity-dir`): open (creating it if
    /// needed) the encrypted direct conversation with a registered user.
    E2eeOpen {
        user_id: u32,
    },
    /// Send text in an end-to-end conversation (32 hex digits); `key` makes a retry
    /// idempotent.
    E2eeSend {
        conversation: String,
        text: String,
        key: Option<String>,
    },
    /// The conversation's lock, members and whether sending is blocked.
    E2eeStatus {
        conversation: String,
    },
    /// Catch up an end-to-end conversation from the server's stream.
    E2eeSync {
        conversation: String,
    },
    /// Open a channel's end-to-end conversation (its conversation id from
    /// `channel_info`, 32 hex digits): join its group, creating it if needed.
    E2eeHistory {
        conversation: String,
        before: Option<u64>,
    },
    E2eeOpenChannel {
        conversation: String,
    },
    /// Device linking (`--offline` only; docs/e2ee.md §3.4): on a device with the
    /// identity, a code to show and the first message for the new device.
    LinkOffer,
    /// On the new device: the code and the existing device's message (hex).
    LinkJoin {
        code: String,
        pake: String,
        device_name: Option<String>,
    },
    /// On the existing device: the new device's messages (hex).
    LinkApprove {
        pake: String,
        device_keys: String,
    },
    /// On the new device: the sealed bundle (hex).
    LinkComplete {
        bundle: String,
    },
    Disconnect,
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn unhex(text: &str) -> Vec<u8> {
    (0..text.len() / 2)
        .filter_map(|index| u8::from_str_radix(text.get(index * 2..index * 2 + 2)?, 16).ok())
        .collect()
}
fn message_json(message: &v1::Message) -> serde_json::Value {
    let author = message.author.clone().unwrap_or_default();
    json!({"seq":message.seq, "kind":message.kind, "target":message.target, "author":author.name,
        "user_id":author.user_id, "format":message.format, "content":message.content,
        "deleted":message.deleted_ms != 0, "edits":message.edits, "delete_reason":message.delete_reason})
}
fn direct_json(direct: &v1::Direct) -> serde_json::Value {
    json!({"conversation":hex(&direct.conversation), "user_id":direct.user_id, "name":direct.name,
        "head":direct.head, "read":direct.read})
}
fn nuschl_json(event: &NuschlEvent) -> serde_json::Value {
    match event {
        NuschlEvent::Error(error) => {
            json!({"kind":"nuschl_error", "request_id":error.request_id, "code":error.code, "detail":error.detail})
        }
        NuschlEvent::Ack(ack) => {
            json!({"kind":"chat_ack", "request_id":ack.request_id, "conversation":hex(&ack.conversation), "seq":ack.seq})
        }
        NuschlEvent::Chat(event) => {
            json!({"kind":"chat", "conversation":hex(&event.conversation), "message":event.message.as_ref().map(message_json)})
        }
        NuschlEvent::Page(page) | NuschlEvent::DecryptedPage { page, .. } => {
            json!({"kind":"history_page", "request_id":page.request_id, "conversation":hex(&page.conversation),
            "messages":page.messages.iter().map(message_json).collect::<Vec<_>>(), "more_before":page.more_before,
            "more_after":page.more_after, "restricted":page.restricted, "head":page.head})
        }
        NuschlEvent::Subscribed(subscribed) => {
            json!({"kind":"subscribed", "request_id":subscribed.request_id,
            "conversations":subscribed.conversations.iter().map(|state| json!({"conversation":hex(&state.conversation), "head":state.head, "read":state.read})).collect::<Vec<_>>()})
        }
        NuschlEvent::Unsubscribed(gone) => {
            json!({"kind":"unsubscribed", "conversation":hex(&gone.conversation)})
        }
        NuschlEvent::Typing(typing) => {
            json!({"kind":"typing", "conversation":hex(&typing.conversation), "session":typing.session})
        }
        NuschlEvent::DirectOpened(opened) => {
            json!({"kind":"direct_opened", "request_id":opened.request_id, "direct":opened.direct.as_ref().map(direct_json)})
        }
        NuschlEvent::DirectList(list) => {
            json!({"kind":"direct_list", "request_id":list.request_id, "directs":list.directs.iter().map(direct_json).collect::<Vec<_>>()})
        }
        NuschlEvent::ReadMarker(marker) => {
            json!({"kind":"read_marker", "conversation":hex(&marker.conversation), "seq":marker.seq})
        }
        other => json!({"kind":"nuschl", "event":format!("{other:?}")}),
    }
}
fn identity_json(event: &IdentityEvent) -> serde_json::Value {
    match event {
        IdentityEvent::Own(view) => json!({"kind":"identity_own", "view":view}),
        IdentityEvent::RecoveryKey(_) => json!({"kind":"identity_recovery_key"}),
        IdentityEvent::Log(outcome) => json!({"kind":"identity_log", "outcome":outcome}),
        IdentityEvent::Published { current } => {
            json!({"kind":"identity_published", "current":current})
        }
        IdentityEvent::Monitor(outcome) => json!({"kind":"identity_monitor", "outcome":outcome}),
        IdentityEvent::Contact { user_id, view } => json!({"kind":"identity_contact",
            "user_id":user_id, "account":hex(&view.account), "state":view.state,
            "identity":view.identity, "safety_number":view.safety_number, "notice":view.notice}),
        IdentityEvent::Failed(reason) => json!({"kind":"identity_failed", "reason":reason}),
    }
}
fn mls_json(event: &MlsEvent) -> serde_json::Value {
    match event {
        MlsEvent::Conversation { view, peer } => json!({"kind":"e2ee_conversation",
            "conversation":hex(&view.conversation), "peer":peer, "view":view}),
        MlsEvent::Received {
            conversation,
            received,
            author,
        } => json!({"kind":"e2ee_received",
            "conversation":hex(conversation), "received":received,
            "author":author.as_ref().map(|author| json!({"account":hex(&author.account),
                "user_id":author.user_id, "name":author.name}))}),
        MlsEvent::Activity { conversation, seq } => json!({"kind":"e2ee_activity",
            "conversation":hex(conversation), "seq":seq}),
        MlsEvent::Sent {
            conversation,
            key,
            seq,
            ..
        } => json!({"kind":"e2ee_sent",
            "conversation":hex(conversation), "key":hex(key), "seq":seq}),
        MlsEvent::NotSent {
            conversation,
            key,
            reason,
        } => json!({"kind":"e2ee_not_sent",
            "conversation":hex(conversation), "key":hex(key), "reason":reason}),
        MlsEvent::PlaintextRefused { conversation, seq } => json!({"kind":"e2ee_plaintext_refused",
            "conversation":hex(conversation), "seq":seq}),
        MlsEvent::KeyPackages { one_time } => {
            json!({"kind":"e2ee_key_packages", "one_time":one_time})
        }
        MlsEvent::Failed(reason) => json!({"kind":"e2ee_failed", "reason":reason}),
    }
}
/// `--offline`: device linking with the crypto worker only (docs/e2ee.md §3.4). The
/// caller relays the messages between the two devices' CLIs.
async fn offline(
    crypto: CryptoHandle,
    username: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use nuschl_ipc::crypto::{CryptoRequest, CryptoResponse};
    let (lines, mut input) = mpsc::channel::<Vec<u8>>(8);
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        loop {
            let mut line = Vec::new();
            match Read::by_ref(&mut stdin)
                .take(4 * 1024 * 1024)
                .read_until(b'\n', &mut line)
            {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if lines.blocking_send(line).is_err() {
                        break;
                    }
                }
            }
        }
    });
    output(json!({"kind":"offline"}))?;
    while let Some(line) = input.recv().await {
        let request = match serde_json::from_slice::<Input>(&line) {
            Ok(Input::LinkOffer) => CryptoRequest::LinkOffer,
            Ok(Input::LinkJoin {
                code,
                pake,
                device_name,
            }) => CryptoRequest::LinkJoin {
                code,
                pake: unhex(&pake),
                device_name: device_name.unwrap_or_else(|| format!("{username} (nuschl-cli)")),
            },
            Ok(Input::LinkApprove { pake, device_keys }) => CryptoRequest::LinkApprove {
                pake: unhex(&pake),
                device_keys: unhex(&device_keys),
            },
            Ok(Input::LinkComplete { bundle }) => CryptoRequest::LinkComplete {
                bundle: unhex(&bundle),
            },
            Ok(Input::IdentityOwn) => CryptoRequest::Identity,
            Ok(Input::Disconnect) => break,
            Ok(_) => {
                output(json!({"kind":"error", "message":"not available with --offline"}))?;
                continue;
            }
            Err(error) => {
                output(json!({"kind":"error", "message":format!("bad command: {error}")}))?;
                continue;
            }
        };
        let event = match crypto.request(request).await {
            Ok(CryptoResponse::LinkOffer { code, pake }) => {
                json!({"kind":"link_offer", "code":code, "pake":hex(&pake)})
            }
            Ok(CryptoResponse::LinkJoin { pake, device_keys }) => {
                json!({"kind":"link_join", "pake":hex(&pake), "device_keys":hex(&device_keys)})
            }
            Ok(CryptoResponse::LinkBundle(bundle)) => {
                json!({"kind":"link_bundle", "bundle":hex(&bundle)})
            }
            Ok(CryptoResponse::Identity(view)) => json!({"kind":"identity_own", "view":view}),
            Ok(other) => json!({"kind":"error", "message":format!("unexpected answer {other:?}")}),
            Err(error) => json!({"kind":"link_failed", "reason":error.to_string()}),
        };
        output(event)?;
    }
    output(json!({"kind":"disconnected", "message":"Disconnected"}))?;
    Ok(())
}
fn account(text: &str) -> Option<[u8; 16]> {
    unhex(text).try_into().ok()
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
    if args.offline {
        let directory = args
            .identity_dir
            .clone()
            .ok_or("--offline needs --identity-dir")?;
        let crypto = CryptoHandle::start(
            nuschl_session::workers::sibling("nuschl-crypto-worker")
                .unwrap_or_else(|_| "nuschl-crypto-worker".into()),
            directory,
        );
        return offline(crypto, &args.username).await;
    }
    let profile = Profile::open(args.profile_dir.clone())?;
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
        voice_route: Default::default(),
        server,
        identity: profile.identity()?,
        password,
        force_tcp: args.force_tcp,
        relay_only: args.relay_only,
        muted: true,
        deafened: false,
        channel: None,
        message_rate: MessageRate {
            burst: args.message_burst,
            per_second: args.message_limit,
        },
        resend: Vec::new(),
        required: nuschl_session::sticky::Requirement {
            nuschl: args.require_nuschl,
            ..Default::default()
        },
        crypto: args.identity_dir.clone().map(|directory| IdentitySetup {
            crypto: CryptoHandle::start(
                nuschl_session::workers::sibling("nuschl-crypto-worker")
                    .unwrap_or_else(|_| "nuschl-crypto-worker".into()),
                directory,
            ),
            device_name: format!("{} (nuschl-cli)", args.username),
            recovery: nuschl_ipc::crypto::RecoveryPolicy::Never,
            witnessing: nuschl_ipc::crypto::KeyTransparencySettings {
                witnesses: args
                    .kt_witnesses
                    .iter()
                    .map(|key| nuschl_ipc::crypto::WitnessSetting {
                        key: key.clone(),
                        enabled: true,
                    })
                    .collect(),
                max_age_hours: args.kt_witness_max_age_hours,
                ..Default::default()
            },
        }),
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
                Ok(Input::IdentityCheck {
                    user_id,
                    direct,
                    name,
                }) => Command::Identity(IdentityCommand::Check {
                    user_id: Some(user_id),
                    account: None,
                    direct,
                    name,
                }),
                Ok(Input::IdentityVerify { account: id }) => match account(&id) {
                    Some(account) => Command::Identity(IdentityCommand::MarkVerified { account }),
                    None => {
                        let _ = output(json!({"kind":"error", "message":"bad account id"}));
                        continue;
                    }
                },
                Ok(Input::IdentityAccept { account: id }) => match account(&id) {
                    Some(account) => {
                        Command::Identity(IdentityCommand::AcceptKeyChange { account })
                    }
                    None => {
                        let _ = output(json!({"kind":"error", "message":"bad account id"}));
                        continue;
                    }
                },
                Ok(Input::IdentityOwn) => Command::Identity(IdentityCommand::Own),
                Ok(Input::E2eeOpen { user_id }) => Command::Mls(MlsCommand::OpenDirect { user_id }),
                Ok(Input::E2eeSend {
                    conversation,
                    text,
                    key,
                }) => match (
                    account(&conversation),
                    key.map_or(Some(connection::random_key()), |key| account(&key)),
                ) {
                    (Some(conversation), Some(key)) => Command::Mls(MlsCommand::Send {
                        conversation,
                        text,
                        key,
                    }),
                    _ => {
                        let _ =
                            output(json!({"kind":"error", "message":"bad conversation or key"}));
                        continue;
                    }
                },
                Ok(Input::E2eeStatus { conversation }) => match account(&conversation) {
                    Some(conversation) => Command::Mls(MlsCommand::Status { conversation }),
                    None => {
                        let _ = output(json!({"kind":"error", "message":"bad conversation"}));
                        continue;
                    }
                },
                Ok(Input::E2eeSync { conversation }) => match account(&conversation) {
                    Some(conversation) => Command::Mls(MlsCommand::Sync { conversation }),
                    None => {
                        let _ = output(json!({"kind":"error", "message":"bad conversation"}));
                        continue;
                    }
                },
                Ok(Input::E2eeHistory {
                    conversation,
                    before,
                }) => match account(&conversation) {
                    Some(conversation) => Command::Mls(MlsCommand::History {
                        conversation,
                        before,
                    }),
                    None => {
                        let _ =
                            output(json!({"kind":"error", "message":"invalid conversation id"}));
                        continue;
                    }
                },
                Ok(Input::E2eeOpenChannel { conversation }) => match account(&conversation) {
                    Some(conversation) => Command::Mls(MlsCommand::OpenChannel { conversation }),
                    None => {
                        let _ = output(json!({"kind":"error", "message":"bad conversation"}));
                        continue;
                    }
                },
                Ok(
                    Input::LinkOffer
                    | Input::LinkJoin { .. }
                    | Input::LinkApprove { .. }
                    | Input::LinkComplete { .. },
                ) => {
                    let _ = output(json!({"kind":"error", "message":"linking needs --offline"}));
                    continue;
                }
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
                Ok(Input::ChatSend {
                    request_id,
                    conversation,
                    content,
                    key,
                }) => Command::Nuschl(NuschlRequest::Send(v1::ChatSend {
                    request_id,
                    conversation: unhex(&conversation),
                    idempotency_key: key
                        .map(|key| unhex(&key))
                        .unwrap_or_else(|| connection::random_key().to_vec()),
                    format: v1::Format::Text as i32,
                    content,
                    reply_to: 0,
                })),
                Ok(Input::History {
                    request_id,
                    conversation,
                    before,
                    after,
                    around,
                    limit,
                }) => {
                    use v1::history_request::Anchor;
                    let anchor = match (before, after, around) {
                        (Some(seq), _, _) => Anchor::Before(seq),
                        (_, Some(seq), _) => Anchor::After(seq),
                        (_, _, Some(seq)) => Anchor::Around(seq),
                        _ => Anchor::Latest(true),
                    };
                    Command::Nuschl(NuschlRequest::History(v1::HistoryRequest {
                        request_id,
                        conversation: unhex(&conversation),
                        anchor: Some(anchor),
                        limit,
                    }))
                }
                Ok(Input::Subscribe {
                    request_id,
                    conversations,
                }) => Command::Nuschl(NuschlRequest::Subscribe(v1::Subscribe {
                    request_id,
                    conversations: conversations.iter().map(|id| unhex(id)).collect(),
                })),
                Ok(Input::ChatEdit {
                    request_id,
                    conversation,
                    target,
                    content,
                }) => Command::Nuschl(NuschlRequest::Edit(v1::ChatEdit {
                    request_id,
                    conversation: unhex(&conversation),
                    target,
                    format: v1::Format::Text as i32,
                    content,
                })),
                Ok(Input::ChatDelete {
                    request_id,
                    conversation,
                    target,
                }) => Command::Nuschl(NuschlRequest::Delete(v1::ChatDelete {
                    request_id,
                    conversation: unhex(&conversation),
                    target,
                })),
                Ok(Input::Typing { conversation }) => {
                    Command::Nuschl(NuschlRequest::Typing(v1::Typing {
                        conversation: unhex(&conversation),
                        session: 0,
                    }))
                }
                Ok(Input::OpenDirect {
                    request_id,
                    user_id,
                }) => Command::Nuschl(NuschlRequest::OpenDirect(v1::OpenDirect {
                    request_id,
                    user_id,
                })),
                Ok(Input::ReadMarker { conversation, seq }) => {
                    Command::Nuschl(NuschlRequest::ReadMarker(v1::ReadMarker {
                        conversation: unhex(&conversation),
                        seq,
                    }))
                }
                Ok(Input::ListDirect { request_id }) => {
                    Command::Nuschl(NuschlRequest::ListDirect(v1::ListDirect { request_id }))
                }
                Ok(Input::DirectText { conversation, body }) => {
                    match <[u8; 16]>::try_from(unhex(&conversation).as_slice()) {
                        Ok(id) => Command::SendText {
                            target: TextTarget::DirectConversation(id),
                            body,
                            expected_peer: None,
                        },
                        Err(_) => {
                            let _ = output(
                                json!({"kind":"error", "message":"conversation must be 32 hex digits"}),
                            );
                            continue;
                        }
                    }
                }
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
                SessionEvent::Negotiated(negotiated) => json!({"kind":"negotiated", "nuschl":negotiated.nuschl, "capabilities":negotiated.capabilities,
                    "policy":negotiated.policy.map(|policy| json!({"moderators_delete":policy.moderators_delete, "edit_limit":policy.edit_limit}))}),
                SessionEvent::Channel(channel) => json!({"kind":"channel", "channel":channel}),
                SessionEvent::ChannelRemoved(id) => json!({"kind":"channel_removed", "channel":id}),
                SessionEvent::User(user) => json!({"kind":"user", "user":user}),
                SessionEvent::UserRemoved(session) => json!({"kind":"user_removed", "session":session}),
                SessionEvent::Connected { session, max_bandwidth } => json!({"kind":"connected", "session":session, "max_bandwidth":max_bandwidth}),
                SessionEvent::Text(message) => json!({"kind":"text", "target":message.target, "author":message.author, "body":message.body, "html":message.html, "outgoing":message.outgoing, "seq":message.seq}),
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
                SessionEvent::AclReply(reply) => match reply.result {
                    Ok(acl) => json!({"kind":"acl_reply", "request":reply.request, "acl":acl}),
                    Err(error) => json!({"kind":"acl_reply", "request":reply.request, "error":error}),
                },
                SessionEvent::UserNames(names) => json!({"kind":"user_names", "names":names}),
                SessionEvent::UnsupportedCodec { session } => json!({"kind":"unsupported_codec", "session":session}),
                SessionEvent::Outbox { sent, total } => json!({"kind":"outbox", "sent":sent, "total":total}),
                SessionEvent::ChannelInfo { channel, info } => json!({"kind":"channel_info", "channel":channel, "conversation":hex(&info.conversation), "visibility":info.visibility}),
                SessionEvent::Nuschl(event) => nuschl_json(&event),
                SessionEvent::ChatPending(pending) => json!({"kind":"chat_pending", "target":pending.target, "key":hex(&pending.key)}),
                SessionEvent::ChatAcked { key, seq } => json!({"kind":"chat_acked", "key":hex(&key), "seq":seq}),
                SessionEvent::ChatFailed { key, reason } => json!({"kind":"chat_failed", "key":hex(&key), "reason":reason}),
                SessionEvent::ConversationState { channel, head, read } => json!({"kind":"conversation_state", "channel":channel, "head":head, "read":read}),
                SessionEvent::Identity(event) => identity_json(&event),
                SessionEvent::Mls(event) => mls_json(&event),
            })?,
        }
    }
    output(json!({"kind":"disconnected", "message":"Disconnected"}))?;
    Ok(())
}
