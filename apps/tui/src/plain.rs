// SPDX-License-Identifier: EUPL-1.2
//! Plain lines (`--plain`): what happens is printed as it happens, one line
//! each, and what is typed is a command or a message. Nothing is redrawn, so
//! screen readers (and logs, pipes and `script`) read each line once. The
//! state is the same as the full screen's; a transcript compares it with
//! what was printed already.
use crate::{
    app::App,
    document::{self, clean},
    view,
};
use nuschl_client::{
    Conversation,
    conversation::Entry,
    server::Status,
    timeline::{Message, MessageStatus},
};
use std::collections::{BTreeSet, HashMap, VecDeque};

/// What identifies a message across reloads of its timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Key {
    Seq(u64),
    Id(i64),
    Serial(u64),
}

impl Key {
    fn of(message: &Message) -> Self {
        match (message.seq, message.id) {
            (Some(seq), _) => Key::Seq(seq),
            (None, Some(id)) => Key::Id(id),
            (None, None) => Key::Serial(message.serial),
        }
    }
}

/// How a printed message looked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shown {
    edited: Option<i64>,
    deleted: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ServerSeen {
    status: Option<Status>,
    notice: Option<String>,
    reconnecting: Option<String>,
    audio: Option<String>,
}

/// What was printed, to print only what changed.
#[derive(Default)]
pub struct Transcript {
    servers: HashMap<String, ServerSeen>,
    open: Option<Conversation>,
    messages: HashMap<Key, Shown>,
    activity: BTreeSet<u64>,
    typing: Option<String>,
    unread: HashMap<(String, u32), usize>,
    voice: Option<Conversation>,
}

impl Transcript {
    /// The lines to print since the last call.
    pub fn lines(&mut self, app: &mut App) -> Vec<String> {
        let mut out = Vec::new();
        let start = app.notes.len() - app.unprinted.min(app.notes.len());
        for note in app.notes.iter().skip(start) {
            let mark = if note.warning { "!" } else { "*" };
            for (index, line) in note.text.split('\n').enumerate() {
                out.push(if index == 0 {
                    format!("{mark} {line}")
                } else {
                    format!("  {line}")
                });
            }
        }
        app.unprinted = 0;
        self.servers(app, &mut out);
        self.voice(app, &mut out);
        self.conversation(app, &mut out);
        self.unread(app, &mut out);
        out
    }

    fn servers(&mut self, app: &App, out: &mut Vec<String>) {
        for server in &app.client.servers {
            let seen = self.servers.entry(server.id.clone()).or_default();
            let name = clean(&server.name);
            if seen.status != Some(server.status) {
                if seen.status.is_some() || server.status != Status::Disconnected {
                    out.push(format!("* {name}: {}", server.status_label()));
                }
                seen.status = Some(server.status);
            }
            if seen.notice != server.notice {
                if let Some(notice) = &server.notice {
                    out.push(format!("! {name}: {}", clean(notice)));
                }
                seen.notice = server.notice.clone();
            }
            if seen.reconnecting != server.reconnecting {
                if let Some(reconnecting) = &server.reconnecting {
                    out.push(format!("! {name}: {}", clean(reconnecting)));
                }
                seen.reconnecting = server.reconnecting.clone();
            }
            let audio = (server.status == Status::Connected && !server.deafened)
                .then(|| server.audio_status.clone());
            if seen.audio != audio {
                if let Some(audio) = &audio {
                    out.push(format!("* {name}: audio: {}", clean(audio)));
                }
                seen.audio = audio;
            }
        }
    }

    fn voice(&mut self, app: &App, out: &mut Vec<String>) {
        let voice = app.client.navigation.voice().cloned();
        if voice == self.voice {
            return;
        }
        if let Some(voice) = &voice
            && let Some(index) = app.client.server_index(&voice.server)
        {
            let server = &app.client.servers[index];
            let channel = server
                .channels
                .iter()
                .find(|channel| channel.id == voice.channel)
                .map(|channel| clean(&channel.name))
                .unwrap_or_default();
            out.push(format!(
                "* Your voice is in {channel} on {}{}.",
                clean(&server.name),
                match (server.muted, server.deafened) {
                    (_, true) => ", deafened",
                    (true, false) => ", muted",
                    (false, false) => "",
                }
            ));
        }
        self.voice = voice;
    }

    fn conversation(&mut self, app: &App, out: &mut Vec<String>) {
        let selected = app.client.navigation.selection().cloned();
        if selected != self.open {
            self.open = selected;
            self.messages.clear();
            self.activity.clear();
            self.typing = None;
            if let Some((server, channel)) = app.open_conversation() {
                out.push(format!(
                    "--- {} on {} ---",
                    clean(&channel.name),
                    clean(&server.name)
                ));
            }
        }
        let Some((server, channel)) = app.open_conversation() else {
            return;
        };
        for entry in channel.entries(|_| true) {
            match entry {
                Entry::Run { .. } => {}
                Entry::Activity(index) => {
                    let line = &channel.activity[index];
                    if self.activity.insert(line.serial) {
                        out.push(format!(
                            "{} {} {}",
                            if line.warning { "!" } else { "·" },
                            app.time(line.observed_at_ms),
                            clean(&line.text)
                        ));
                    }
                }
                Entry::Message { position, .. } => {
                    let message = &channel.timeline.messages[position];
                    // Ours is printed once the server has it.
                    if message.status == MessageStatus::Sending {
                        continue;
                    }
                    let shown = Shown {
                        edited: message.edited_ms,
                        deleted: message.deleted.is_some(),
                    };
                    let before = self.messages.insert(Key::of(message), shown);
                    if before == Some(shown) {
                        continue;
                    }
                    let lines =
                        view::message_lines(app, server, channel.direct_peer.as_ref(), message);
                    // A change to a message printed before shows again, with
                    // "(edited)" or "(deleted)" in it.
                    out.extend(lines.iter().map(document::plain));
                }
            }
        }
        let typing =
            server.typing_line(channel, &app.client.preferences, crate::app::monotonic_ms());
        if typing != self.typing {
            if let Some(typing) = &typing {
                out.push(format!("* {}", clean(typing)));
            }
            self.typing = typing;
        }
    }

    fn unread(&mut self, app: &App, out: &mut Vec<String>) {
        let open = app.client.navigation.selection();
        for server in &app.client.servers {
            for channel in &server.channels {
                let key = (server.id.clone(), channel.id);
                let before = self.unread.insert(key, channel.unread).unwrap_or(0);
                let here =
                    open.is_some_and(|open| open.server == server.id && open.channel == channel.id);
                if before == 0 && channel.unread > 0 && !here {
                    out.push(format!(
                        "* {} unread in {} on {}{} (/go {})",
                        channel.unread,
                        clean(&channel.name),
                        clean(&server.name),
                        if channel.unread_mention {
                            ", for you"
                        } else {
                            ""
                        },
                        clean(channel.name.trim_start_matches("@ "))
                    ));
                }
            }
        }
    }
}

/// Run the plain-line mode until /quit or the end of the input.
pub async fn run(
    mut app: App,
    mut events: tokio::sync::mpsc::Receiver<nuschl_client::native::Event>,
) -> std::io::Result<()> {
    use std::io::Write;
    let (lines, mut typed) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            match stdin.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if lines
                        .send(line.trim_end_matches(['\r', '\n']).to_string())
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
    let mut transcript = Transcript::default();
    let mut outbox = VecDeque::<String>::new();
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
    let mut input_open = true;
    println!("Nuschl in plain lines. Type a message, or a command: /help lists them.");
    loop {
        // Messages go one at a time, in the order typed.
        if !app.client.sending()
            && let Some(body) = outbox.pop_front()
        {
            app.send(body);
            app.apply_updates();
        }
        {
            let mut stdout = std::io::stdout().lock();
            for line in transcript.lines(&mut app) {
                writeln!(stdout, "{}", clean(&line))?;
            }
            stdout.flush()?;
        }
        if app.quit || (!input_open && outbox.is_empty() && !app.client.sending()) {
            break;
        }
        tokio::select! {
            Some(event) = events.recv() => {
                let mut batch = vec![event];
                while batch.len() < 256 && let Ok(event) = events.try_recv() {
                    batch.push(event);
                }
                app.events(batch);
            }
            line = typed.recv(), if input_open => match line {
                Some(line) if line.starts_with('/') && !line.starts_with("//") => {
                    app.command(&line[1..]);
                }
                Some(line) if line.trim().is_empty() => {}
                Some(line) => {
                    let body = line.strip_prefix('/').map(str::to_string).unwrap_or(line);
                    outbox.push_back(body);
                }
                None => input_open = false,
            },
            _ = tick.tick() => app.tick(),
        }
    }
    app.tick();
    Ok(())
}
