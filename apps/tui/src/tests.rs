// SPDX-License-Identifier: EUPL-1.2
//! The terminal client's state and drawing without a server: the SDK's
//! client over a platform that records what it is asked, inputs as a
//! connection would report them, and the screen read back from ratatui's
//! test backend.
use crate::{
    app::{App, Options, Overlay, Pane},
    plain::Transcript,
    view,
};
use nuschl_client::{
    Client,
    input::{Input, ServerEvent},
    model::{Channel, TextTarget, User},
    platform::{CacheKey, ChangeRequest, Platform, RenderKey, Request},
    timeline::{Deletion, IncomingMessage, Page},
};
use nuschl_core::{
    ChatMessage, ContentBlock, MessageDocument, SearchCursor, Span, TextRole, audio::Sound,
    bookmark::ServerBookmark,
};
use ratatui::{
    Terminal,
    backend::TestBackend,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
};
use std::sync::{Arc, Mutex};

/// A connection asked for: its password, whether deafened, and the pin.
type Connect = (Option<String>, bool, Option<String>);

/// A platform that records what the client asked of it.
#[derive(Default)]
struct Recorder {
    connects: Mutex<Vec<Connect>>,
    requests: Mutex<Vec<Request>>,
    changes: Mutex<Vec<ChangeRequest>>,
    pages: Mutex<Vec<(nuschl_client::HistoryTicket, Page)>>,
}

impl Platform for Recorder {
    fn connect(
        &self,
        bookmark: ServerBookmark,
        password: Option<String>,
        deafened: bool,
    ) -> Result<u64, String> {
        let mut connects = self.connects.lock().unwrap();
        connects.push((password, deafened, bookmark.certificate_sha256));
        Ok(connects.len() as u64)
    }
    fn disconnect(&self, _: &str) {}
    fn request(&self, _: &str, request: Request) -> Result<(), String> {
        self.requests.lock().unwrap().push(request);
        Ok(())
    }
    fn local_history(
        &self,
        _: String,
        _: String,
        _: String,
        ticket: nuschl_client::HistoryTicket,
        page: Page,
    ) -> Result<(), String> {
        self.pages.lock().unwrap().push((ticket, page));
        Ok(())
    }
    fn server_history(
        &self,
        _: String,
        _: [u8; 16],
        _: nuschl_client::HistoryTicket,
        _: Option<CacheKey>,
        _: Page,
    ) -> Result<(), String> {
        Ok(())
    }
    fn change_message(&self, change: ChangeRequest) -> Result<(), String> {
        self.changes.lock().unwrap().push(change);
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

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

fn options() -> Options {
    Options {
        audio: true,
        opener: None,
        plain: false,
        ssh: false,
    }
}

#[test]
fn clipboard_images_use_html_and_freeze_the_destination() {
    let (mut app, recorder, _) = connected();
    app.clipboard = crate::clipboard::Clipboard::with_test_image(&app);
    app.composer.insert("a caption");
    app.submit();
    let requests = recorder.requests.lock().unwrap();
    let (target, body) = requests
        .iter()
        .find_map(|request| match request {
            Request::SendHtml {
                html,
                target,
                expected_peer: None,
            } if html.len() == 1 && html[0].contains("a caption") && html[0].contains("<img ") => {
                Some((target.clone(), html[0].clone()))
            }
            _ => None,
        })
        .unwrap();
    drop(requests);
    assert!(app.clipboard.attached());
    assert_eq!(app.composer.text(), "a caption");
    app.clipboard_input(&Input::Sent {
        server: app.client.servers[0].id.clone(),
        generation: 999,
        target: target.clone(),
        body: body.clone(),
    });
    assert!(app.clipboard.attached());
    app.clipboard_input(&Input::Sent {
        server: app.client.servers[0].id.clone(),
        generation: 1,
        target,
        body,
    });
    assert!(!app.clipboard.attached());
    assert!(app.composer.is_empty());
}

#[test]
fn prepared_images_do_not_follow_navigation_reconnects_or_certificate_changes() {
    for change in ["navigation", "generation", "certificate", "mls", "policy"] {
        let (mut app, recorder, _) = connected();
        app.clipboard = crate::clipboard::Clipboard::with_test_image(&app);
        app.composer.insert("keep this draft");
        match change {
            "navigation" => app.browse(0, 2),
            "generation" => app.client.servers[0].generation += 1,
            "certificate" => app.client.servers[0].peer_sha256 = Some("cd".repeat(32)),
            "mls" => app.client.servers[0].mls = true,
            "policy" => {
                // A changed image limit must be checked when sending too.
                app.client.servers[0].limits = Some(nuschl_core::model::ServerLimits {
                    allow_html: true,
                    message_length: 5000,
                    image_message_length: 1,
                    max_users: None,
                    recording_allowed: false,
                    max_bandwidth: None,
                });
            }
            _ => unreachable!(),
        }
        recorder.requests.lock().unwrap().clear();
        app.submit();
        assert!(
            !recorder
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| matches!(request, Request::SendHtml { .. })),
            "{change}"
        );
        assert!(app.clipboard.attached(), "{change}");
        if change == "navigation" {
            app.browse(0, 1);
        }
        assert_eq!(app.composer.text(), "keep this draft", "{change}");
    }
}

#[test]
fn private_image_follows_the_same_certificate_but_rejects_a_reused_presence() {
    for identity in ["same certificate", "different certificate", "anonymous"] {
        let (mut app, recorder, _) = connected();
        if identity != "anonymous" {
            app.client.servers[0]
                .users
                .get_mut(&2)
                .unwrap()
                .certificate_hash = Some("bob-cert".into());
        }
        app.command("dm Bob");
        app.clipboard = crate::clipboard::Clipboard::with_test_image(&app);
        app.composer.insert("private caption");
        let peer = app.client.servers[0].users.get_mut(&2).unwrap();
        peer.instance = 99;
        if identity == "different certificate" {
            peer.certificate_hash = Some("someone-else".into());
        }
        app.client.servers[0].generation += 1;
        recorder.requests.lock().unwrap().clear();
        app.submit();
        let sent = recorder.requests.lock().unwrap().iter().any(|request| {
            matches!(
                request,
                Request::SendHtml {
                    expected_peer: Some(99),
                    ..
                }
            )
        });
        assert_eq!(sent, identity == "same certificate", "{identity}");
        assert!(app.clipboard.attached());
        assert_eq!(app.composer.text(), "private caption");
    }
}

#[test]
fn ssh_clipboard_action_never_starts_a_host_clipboard_read() {
    let (mut app, _, _) = connected();
    app.options.ssh = true;
    // No runtime or clipboard exists in this test. The SSH guard must happen
    // before starting either the helper or worker.
    app.command("paste-image");
    assert!(!app.clipboard.attached());
    assert!(
        app.notes
            .back()
            .unwrap()
            .text
            .contains("remote machine's OS clipboard is not read")
    );
}

/// An app connected to "Test" as Ada (session 1) with Bob (2) beside
/// her in Lobby, and Lobby open.
fn connected() -> (App, Arc<Recorder>, String) {
    let recorder = Arc::new(Recorder::default());
    let mut bookmark = ServerBookmark::new("Test", "localhost", 64738, "Ada").unwrap();
    bookmark.certificate_sha256 = Some("ab".repeat(32));
    let id = bookmark.id.clone();
    let client = Client::new(
        Box::new(recorder.clone()),
        vec![bookmark],
        Default::default(),
    );
    let mut app = App::new(client, None, options());
    app.tz = jiff::tz::TimeZone::UTC;
    app.client.connect(0, None, false);
    let event = |event| Input::Server {
        server: id.clone(),
        generation: 1,
        event,
    };
    let channel = |id: u32, parent: Option<u32>, name: &str| {
        ServerEvent::Channel(Channel {
            id,
            parent,
            name: name.into(),
            ..Default::default()
        })
    };
    let user = |session: u32, name: &str| User {
        session,
        name: name.into(),
        channel: 1,
        user_id: Some(session + 10),
        ..Default::default()
    };
    for input in [
        event(channel(0, None, "Root")),
        event(channel(1, Some(0), "Lobby")),
        event(channel(2, Some(0), "Gaming")),
        event(ServerEvent::User(User {
            self_deafened: true,
            self_muted: true,
            ..user(1, "Ada")
        })),
        event(ServerEvent::User(User {
            self_muted: true,
            ..user(2, "Bob")
        })),
        event(ServerEvent::Connected { session: 1 }),
        Input::Talking {
            server: id.clone(),
            generation: 1,
            session: 2,
            talking: true,
        },
    ] {
        app.client.handle(input);
    }
    app.settle();
    app.browse(0, 1);
    (app, recorder, id)
}

fn message(
    server: &str,
    channel: u32,
    author: &str,
    body: &str,
    document: Option<MessageDocument>,
) -> Input {
    static ID: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);
    let id = ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Input::Message {
        server: server.into(),
        generation: 1,
        target: TextTarget::Channel(channel),
        storage_key: channel.to_string(),
        peer: None,
        message: IncomingMessage::plain(
            Some(id),
            ChatMessage {
                server: server.into(),
                channel: channel.to_string(),
                author: author.into(),
                observed_at_ms: now_ms() + id,
                body: body.into(),
            },
            document.unwrap_or(MessageDocument {
                blocks: vec![ContentBlock::text(body)],
            }),
            author == "Ada",
        ),
    }
}

/// The screen as lines of text.
fn screen(app: &mut App, width: u16, height: u16) -> (Vec<String>, Option<(u16, u16)>) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let frame = terminal.draw(|frame| view::draw(frame, app)).unwrap();
    let buffer = frame.buffer;
    let lines = (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    let cursor = terminal
        .get_cursor_position()
        .ok()
        .map(|position| (position.x, position.y));
    (lines, cursor)
}

fn shows(lines: &[String], text: &str) -> bool {
    lines.iter().any(|line| line.contains(text))
}

fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    app.key(KeyEvent::new(code, modifiers));
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c), KeyModifiers::NONE);
    }
}

#[test]
fn people_and_their_states_are_words_beside_the_open_conversation() {
    let (mut app, _, id) = connected();
    app.client
        .handle(message(&id, 1, "Bob", "Hello from Bob", None));
    app.client.handle(message(&id, 1, "Ada", "Hi Bob", None));
    app.settle();
    let (lines, _) = screen(&mut app, 110, 24);
    let text = lines.join("\n");
    assert!(shows(&lines, "Test: Connected"), "{text}");
    assert!(shows(&lines, "Lobby (your voice)"), "{text}");
    assert!(shows(&lines, "Ada (you, deafened)"), "{text}");
    assert!(shows(&lines, "Bob (talking, muted)"), "{text}");
    assert!(shows(&lines, "Lobby on Test, your voice channel"), "{text}");
    assert!(shows(&lines, "Bob: Hello from Bob"), "{text}");
    assert!(shows(&lines, "Ada: Hi Bob"), "{text}");
    assert!(shows(&lines, "Message to Lobby (active)"), "{text}");
    // The newest message is at the bottom, by the composer.
    let newest = lines
        .iter()
        .position(|line| line.contains("Ada: Hi Bob"))
        .unwrap();
    let composer = lines
        .iter()
        .position(|line| line.contains("Message to Lobby"))
        .unwrap();
    assert!(composer - newest <= 3, "{text}");
}

#[test]
fn unread_messages_are_counted_in_the_tree_and_the_cursor_marks_the_row() {
    let (mut app, _, id) = connected();
    app.client
        .handle(message(&id, 2, "Bob", "In Gaming, Ada", None));
    app.settle();
    app.pane = Pane::Tree;
    let gaming = app
        .rows()
        .iter()
        .position(|row| view::row_text(&app, row).contains("Gaming"))
        .unwrap();
    for _ in 0..gaming {
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    }
    let (lines, cursor) = screen(&mut app, 110, 24);
    let row = lines
        .iter()
        .position(|line| line.contains("Gaming (1 unread, mention)"))
        .unwrap_or_else(|| panic!("{}", lines.join("\n")));
    assert!(
        lines[row].contains(">  "),
        "the row is marked: {}",
        lines[row]
    );
    assert_eq!(
        cursor.map(|(_, y)| y),
        Some(row as u16),
        "the cursor is on it"
    );
    // Enter opens it: read.
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let (lines, _) = screen(&mut app, 110, 24);
    assert!(shows(&lines, "Gaming on Test"));
    assert!(!shows(&lines, "unread"), "{}", lines.join("\n"));
}

#[test]
fn edits_deletions_typing_and_states_are_spelled_out() {
    let (mut app, _, id) = connected();
    for body in ["first", "second"] {
        app.client.handle(message(&id, 1, "Bob", body, None));
    }
    app.settle();
    let (server, channel) = app.selected().unwrap();
    let state = &mut app.client.servers[server];
    let messages = &mut state.channels[channel].timeline.messages;
    messages[0].edited_ms = Some(now_ms());
    messages[1].deleted = Some(Deletion {
        at_ms: now_ms(),
        moderator: true,
    });
    state.channels[channel]
        .typing
        .insert(2, crate::app::monotonic_ms());
    let (lines, _) = screen(&mut app, 110, 24);
    let text = lines.join("\n");
    assert!(shows(&lines, "Bob (edited): first"), "{text}");
    assert!(shows(&lines, "Bob: (deleted by a moderator)"), "{text}");
    assert!(shows(&lines, "Bob is typing…"), "{text}");
}

#[test]
fn images_formulas_and_links_are_placeholders_and_numbers() {
    let (mut app, _, id) = connected();
    let document = MessageDocument {
        blocks: vec![
            ContentBlock::Text {
                text: "Look: the plan".into(),
                spans: vec![Span {
                    start: 10,
                    end: 14,
                    link: Some("https://example.org/plan".into()),
                    ..Default::default()
                }],
                role: TextRole::Paragraph,
            },
            ContentBlock::Image {
                width: 640,
                height: 480,
                rgba: String::new(),
                source: None,
            },
            ContentBlock::Math {
                source: "$$a^2+b^2=c^2$$".into(),
                tex: "a^2+b^2=c^2".into(),
            },
        ],
    };
    app.client
        .handle(message(&id, 1, "Bob", "", Some(document)));
    app.settle();
    let (lines, _) = screen(&mut app, 110, 24);
    let text = lines.join("\n");
    assert!(shows(&lines, "Bob: Look: the plan[1]"), "{text}");
    assert!(shows(&lines, "[image, 640×480]"), "{text}");
    assert!(shows(&lines, "[formula] $$a^2+b^2=c^2$$"), "{text}");
    assert!(shows(&lines, "[1] https://example.org/plan"), "{text}");
    assert_eq!(app.recent_links(), ["https://example.org/plan"]);
    // Without an opener the link is only shown.
    app.command("open 1");
    assert_eq!(
        app.notes.back().map(|note| note.text.as_str()),
        Some("Link: https://example.org/plan")
    );
}

#[test]
fn the_switcher_opens_and_joins_and_commands_do_the_same() {
    let (mut app, recorder, _) = connected();
    press(&mut app, KeyCode::Char('k'), KeyModifiers::CONTROL);
    type_text(&mut app, "gam");
    let (lines, cursor) = screen(&mut app, 110, 24);
    assert!(shows(&lines, "Find: gam"), "{}", lines.join("\n"));
    assert!(
        shows(&lines, ">Gaming, channel, Test"),
        "{}",
        lines.join("\n")
    );
    let field = lines
        .iter()
        .position(|line| line.contains("Find: gam"))
        .unwrap();
    assert_eq!(
        cursor.map(|(_, y)| y),
        Some(field as u16),
        "the cursor is in the field"
    );
    press(&mut app, KeyCode::Enter, KeyModifiers::ALT);
    assert!(app.overlay.is_none());
    assert!(
        recorder
            .requests
            .lock()
            .unwrap()
            .contains(&Request::Join(2))
    );
    assert_eq!(
        app.open_conversation().map(|(_, channel)| channel.id),
        Some(2)
    );
    app.command("go lobby");
    assert_eq!(
        app.open_conversation().map(|(_, channel)| channel.id),
        Some(1)
    );
    app.command("join");
    assert!(
        recorder
            .requests
            .lock()
            .unwrap()
            .contains(&Request::Join(1))
    );
    app.command("mute");
    assert_eq!(
        recorder.requests.lock().unwrap().last(),
        Some(&Request::SetSelf {
            muted: false,
            deafened: false
        })
    );
}

#[test]
fn without_audio_voice_stays_off() {
    let (mut app, recorder, _) = connected();
    app.options.audio = false;
    let before = recorder.requests.lock().unwrap().len();
    let voice_state = (app.client.servers[0].muted, app.client.servers[0].deafened);
    app.set_mute(true);
    assert_eq!(
        recorder.requests.lock().unwrap().len(),
        before,
        "nothing undeafens"
    );
    assert!(
        app.notes
            .back()
            .is_some_and(|note| note.text.contains("--no-audio"))
    );
    for command in ["mute", "deafen", "speak both", "speak here"] {
        app.command(command);
        assert_eq!(
            recorder.requests.lock().unwrap().len(),
            before,
            "{command} must not enable audio in a --no-audio or SSH session"
        );
        assert_eq!(
            (app.client.servers[0].muted, app.client.servers[0].deafened),
            voice_state
        );
        assert!(
            app.notes
                .back()
                .is_some_and(|note| note.text.contains("--no-audio"))
        );
    }
    app.connect(0);
    assert_eq!(
        recorder
            .connects
            .lock()
            .unwrap()
            .last()
            .map(|connect| connect.1),
        Some(true)
    );
}

#[test]
fn speaking_on_another_server_waits_for_the_terminal_confirmation() {
    let (mut app, recorder, first) = connected();
    app.command("mute");
    assert!(!app.client.servers[0].muted);

    let bookmark = ServerBookmark::new("Other", "localhost", 64739, "Ada").unwrap();
    let second = bookmark.id.clone();
    let index = app.client.add_server(bookmark);
    app.client.connect(index, None, false);
    for event in [
        ServerEvent::Channel(Channel {
            id: 0,
            name: "Root".into(),
            ..Default::default()
        }),
        ServerEvent::Connected { session: 3 },
    ] {
        app.client.handle(Input::Server {
            server: second.clone(),
            generation: 2,
            event,
        });
    }
    app.browse(index, 0);
    let before = recorder.requests.lock().unwrap().len();
    app.command("mute");
    assert_eq!(recorder.requests.lock().unwrap().len(), before);
    assert!(app.client.servers[index].muted);
    assert!(!app.client.servers[0].muted);
    assert!(app.notes.back().unwrap().text.contains("/speak both"));

    app.command("speak both");
    assert!(!app.client.servers[index].muted);
    assert!(!app.client.servers[0].muted);
    assert!(app.client.voice.is_talk(&first));
    assert!(app.client.voice.is_talk(&second));

    app.command("speak here");
    assert!(app.client.servers[0].muted);
    assert!(!app.client.servers[index].muted);
    assert!(!app.client.voice.is_talk(&first));
    assert!(app.client.voice.is_talk(&second));
}

#[test]
fn a_new_certificate_asks_and_y_trusts_it() {
    let (mut app, recorder, id) = connected();
    app.client.servers[0].bookmark.certificate_sha256 = None;
    app.client.handle(Input::Certificate {
        server: id,
        generation: 1,
        sha256: "cd".repeat(32),
        changed: false,
        reason: String::new(),
    });
    app.settle();
    assert!(matches!(app.overlay, Some(Overlay::Certificate(0))));
    let (lines, _) = screen(&mut app, 110, 24);
    assert!(
        shows(&lines, "First connection to Test."),
        "{}",
        lines.join("\n")
    );
    assert!(shows(&lines, &format!("SHA-256: {}", "cd".repeat(32))));
    press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
    assert!(app.overlay.is_none());
    let connects = recorder.connects.lock().unwrap();
    assert_eq!(
        connects.last().and_then(|connect| connect.2.clone()),
        Some("cd".repeat(32))
    );
}

#[test]
fn up_in_the_empty_composer_edits_the_last_own_message_on_a_nuschl_server() {
    let (mut app, recorder, id) = connected();
    // A Nuschl server keeping this channel's history.
    app.client.handle(Input::Server {
        server: id.clone(),
        generation: 1,
        event: ServerEvent::Negotiated {
            capabilities: vec!["history/1".into()],
            policy: None,
        },
    });
    app.client.handle(Input::Server {
        server: id.clone(),
        generation: 1,
        event: ServerEvent::ChannelInfo {
            channel: 1,
            conversation: Some([7; 16]),
        },
    });
    app.settle();
    app.browse(0, 1);
    let mut mine = message(&id, 1, "Ada", "Typo hree", None);
    if let Input::Message { message, .. } = &mut mine {
        message.seq = Some(5);
        message.id = Some(5);
        message.author_id = Some(11);
    }
    app.client.handle(mine);
    app.settle();
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.composer.text(), "Typo hree");
    let (lines, _) = screen(&mut app, 110, 24);
    assert!(shows(
        &lines,
        "Editing your message: Enter saves, Esc cancels"
    ));
    press(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
    type_text(&mut app, "Typo here");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    let change = recorder.changes.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        (change.seq, change.content.as_deref()),
        (5, Some("Typo here"))
    );
    assert!(app.editing.is_none() && app.composer.is_empty());
}

#[test]
fn plain_lines_print_each_message_once_and_again_when_it_changes() {
    let (mut app, _, id) = connected();
    app.options.plain = true;
    let mut transcript = Transcript::default();
    app.client.handle(message(&id, 1, "Bob", "Hello", None));
    app.settle();
    let first = transcript.lines(&mut app).join("\n");
    assert!(first.contains("--- Lobby on Test ---"), "{first}");
    assert!(first.contains("Bob: Hello"), "{first}");
    assert!(transcript.lines(&mut app).is_empty(), "nothing new");
    let (server, channel) = app.selected().unwrap();
    app.client.servers[server].channels[channel]
        .timeline
        .messages[0]
        .edited_ms = Some(now_ms());
    let again = transcript.lines(&mut app);
    assert_eq!(again.len(), 1, "{again:?}");
    assert!(again[0].ends_with("Bob (edited): Hello"), "{again:?}");
    // Elsewhere: counted once.
    app.client.handle(message(&id, 2, "Bob", "Over here", None));
    app.settle();
    let unread = transcript.lines(&mut app).join("\n");
    assert!(unread.contains("1 unread in Gaming on Test"), "{unread}");
}

#[test]
fn the_help_lists_keys_and_commands() {
    let (mut app, _, _) = connected();
    app.pane = Pane::Tree;
    press(&mut app, KeyCode::Char('?'), KeyModifiers::NONE);
    let (lines, _) = screen(&mut app, 110, 40);
    assert!(shows(&lines, "Help: Esc closes"));
    assert!(shows(
        &lines,
        "Ctrl+K           find a channel, person or conversation"
    ));
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.overlay.is_none());
}

#[test]
fn page_up_at_the_top_loads_the_older_page_and_keeps_the_place() {
    let (mut app, recorder, id) = connected();
    let (ticket, page) = recorder.pages.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        page,
        Page::Before(None),
        "opening Lobby asked for its latest page"
    );
    let start = now_ms();
    let messages = (100..140)
        .map(|n| {
            let Input::Message { mut message, .. } =
                message(&id, 1, "Bob", &format!("line {n}"), None)
            else {
                unreachable!()
            };
            message.id = Some(n);
            message.message.observed_at_ms = start + n;
            message
        })
        .collect();
    app.client.handle(Input::History {
        server: id.clone(),
        ticket,
        before: None,
        messages,
        next_before: Some(100),
    });
    app.settle();
    let (lines, _) = screen(&mut app, 110, 24);
    assert!(shows(&lines, "Bob: line 139"), "the newest at the bottom");
    assert!(!shows(&lines, "Bob: line 100"));
    for _ in 0..10 {
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        let _ = screen(&mut app, 110, 24);
    }
    let (lines, _) = screen(&mut app, 110, 24);
    assert!(
        shows(&lines, "Older messages: PageUp or Home") || shows(&lines, "Loading…"),
        "{}",
        lines.join("\n")
    );
    assert!(shows(&lines, "Bob: line 100"), "{}", lines.join("\n"));
    assert_eq!(
        recorder.pages.lock().unwrap().last().map(|(_, page)| *page),
        Some(Page::Before(Some(100))),
        "at the top, the page before the oldest was asked for"
    );
    // End goes back to the newest.
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.pane, Pane::Timeline);
    press(&mut app, KeyCode::End, KeyModifiers::NONE);
    let (lines, _) = screen(&mut app, 110, 24);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("> ") && line.contains("Bob: line 139")),
        "the newest message, selected: {}",
        lines.join("\n")
    );
    assert_eq!(app.timeline.scroll, 0);
}
