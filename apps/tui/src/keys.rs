// SPDX-License-Identifier: EUPL-1.2
//! The full screen's keys, and the help that lists them with the commands.
//! Keys that type text never do anything else in the composer; elsewhere
//! single letters act. Everything a key does is also a command, so the
//! plain-line mode can do it too.
use crate::{
    app::{App, Overlay, Pane, Row},
    input::Input,
};
use nuschl_client::search::Scope;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// The keys, as the help shows them.
pub const KEYS: &[&str] = &[
    "Everywhere",
    "  Tab, Shift+Tab   move between channels, messages and composer",
    "  Ctrl+K           find a channel, person or conversation",
    "  Ctrl+F           search messages (Tab: this conversation, server, all)",
    "  PageUp, PageDown scroll the messages; older ones load at the top",
    "  Alt+M, Alt+D     mute, deafen",
    "  F1               this help (also ? outside the composer)",
    "  Esc              back to the composer; cancels an edit",
    "  Ctrl+Q, Ctrl+C   quit",
    "Channels",
    "  Up, Down, Home, End   move",
    "  Enter            open the channel or conversation; message a person;",
    "                   connect a server",
    "  J, Alt+Enter     join the channel's voice (a person's channel)",
    "  Left, Right, Space   close or open a branch",
    "  c, x             connect, disconnect the server",
    "  t                trust the server's certificate (first connection)",
    "  m, d             mute, deafen",
    "Messages",
    "  Up, Down         previous, next message",
    "  Home, End        oldest loaded (older load), newest",
    "  Enter, o         the message's links, to open one",
    "  e                edit your message (Nuschl servers)",
    "  Delete, x        delete your message (Nuschl servers)",
    "Composer",
    "  Enter            send; a line starting with / is a command",
    "  Alt+Enter        new line",
    "  Ctrl+V           attach a PNG image from the local Linux clipboard",
    "  Up               (empty) edit your last message",
    "  Ctrl+A, Ctrl+E, Ctrl+W, Ctrl+U   start, end, delete word, delete to start",
];

/// The commands, typed in the composer (and the plain-line mode).
pub const COMMANDS: &[&str] = &[
    "Commands (type them in the composer; //text sends /text):",
    "  /servers                 saved servers and their state",
    "  /connect [N|NAME]        connect (the current server, or saved server N)",
    "  /disconnect [N|NAME]     disconnect",
    "  /url mumble://NAME@HOST[:PORT][/CHANNEL]   connect to a link",
    "  /trust                   trust the certificate a server showed",
    "  /password SECRET, /username NAME   log in again after a refusal",
    "  /register                register your name on this server",
    "  /go NAME                 open a channel, person or conversation",
    "  /join [NAME]             join a channel's voice (the open one)",
    "  /dm NAME                 direct messages with someone",
    "  /who, /channels, /status who is here, the channel tree, your state",
    "  /mute, /deafen           toggle",
    "  /bitrate [64|96|128|192|256]   voice bitrate in kb/s: requested, and what each server allows",
    "  /speak both|here         when unmuting asks: speak on both servers, or only here",
    "  /edit [TEXT]             edit your last message (with TEXT: replace it)",
    "  /delete                  delete your last message",
    "  /search WORDS            search this conversation (/searchserver, /searchall)",
    "  /result N, /more         open result N, more results",
    "  /older                   load older messages",
    "  /paste-image, /send-image, /discard-image   attach local PNG, send, remove",
    "  /links, /open N          the newest messages' links, open link N",
    "  /quit",
];

impl App {
    /// A key in the full screen.
    pub fn key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('c' | 'q') if control => {
                self.quit = true;
                return;
            }
            KeyCode::F(1) => {
                self.overlay = match self.overlay {
                    Some(Overlay::Help { .. }) => None,
                    _ => Some(Overlay::Help { scroll: 0 }),
                };
                return;
            }
            _ => {}
        }
        if self.overlay.is_some() {
            return self.overlay_key(key);
        }
        match key.code {
            KeyCode::Char('k') if control => {
                self.overlay = Some(Overlay::Finder {
                    input: Input::default(),
                    selected: None,
                });
            }
            KeyCode::Char('f') if control => self.open_search(Scope::Conversation, None),
            KeyCode::Char('m') if alt => self.set_mute(false),
            KeyCode::Char('d') if alt => self.set_mute(true),
            KeyCode::Tab => self.cycle(true),
            KeyCode::BackTab => self.cycle(false),
            KeyCode::PageUp => self.scroll(true),
            KeyCode::PageDown => self.scroll(false),
            KeyCode::Esc => {
                if !self.cancel_edit() {
                    self.pane = Pane::Composer;
                }
            }
            _ => match self.pane {
                Pane::Composer => self.composer_key(key),
                Pane::Tree => self.tree_key(key),
                Pane::Timeline => self.timeline_key(key),
            },
        }
    }

    /// Pasted text goes where text is typed.
    pub fn paste(&mut self, text: &str) {
        match &mut self.overlay {
            Some(Overlay::Finder { input, selected }) => {
                input.insert(text);
                *selected = None;
            }
            Some(Overlay::Search(search)) => {
                search.input.insert(text);
                self.run_search();
            }
            Some(Overlay::Prompt { input, .. }) => input.insert(text),
            Some(_) => {}
            None => {
                self.pane = Pane::Composer;
                self.composer.insert(text);
                self.composer_changed();
            }
        }
    }

    fn cycle(&mut self, forward: bool) {
        self.pane = match (self.pane, forward) {
            (Pane::Tree, true) | (Pane::Composer, false) => Pane::Timeline,
            (Pane::Timeline, true) | (Pane::Tree, false) => Pane::Composer,
            (Pane::Composer, true) | (Pane::Timeline, false) => Pane::Tree,
        };
        if self.pane == Pane::Timeline && self.timeline.selected.is_none() {
            self.timeline.selected = self.newest_message();
        }
    }

    fn newest_message(&self) -> Option<u64> {
        let (_, channel) = self.open_conversation()?;
        channel
            .timeline
            .messages
            .last()
            .map(|message| message.serial)
    }

    /// A page up or down; at the top, older messages load.
    fn scroll(&mut self, up: bool) {
        let page = self.timeline.height.max(2) - 1;
        if up {
            self.timeline.scroll = (self.timeline.scroll + page).min(self.timeline.max);
            if self.timeline.scroll >= self.timeline.max {
                self.older();
            }
        } else {
            self.timeline.scroll = self.timeline.scroll.saturating_sub(page);
            if self.timeline.scroll == 0 {
                self.newer();
            }
        }
        self.timeline.selected = None;
        self.settle();
    }

    fn composer_key(&mut self, key: KeyEvent) {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.paste_image()
            }
            KeyCode::Enter if alt || key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.composer.insert("\n");
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Up if self.composer.is_empty() && self.editing.is_none() => {
                self.start_edit(None);
            }
            KeyCode::Up => self.pane = Pane::Timeline,
            _ => {
                let before = self.composer.text().len();
                if self.composer.key(key) && self.composer.text().len() != before {
                    self.composer_changed();
                }
            }
        }
    }

    fn tree_key(&mut self, key: KeyEvent) {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Up => self.move_tree(-1),
            KeyCode::Down => self.move_tree(1),
            KeyCode::Home => self.tree_home(false),
            KeyCode::End => self.tree_home(true),
            KeyCode::Enter => self.activate(alt),
            KeyCode::Char('J') => self.activate(true),
            KeyCode::Left => self.expand(Some(false)),
            KeyCode::Right => self.expand(Some(true)),
            KeyCode::Char(' ') => self.expand(None),
            KeyCode::Char('c') => {
                if let Some(server) = self.tree_server() {
                    self.connect(server);
                }
            }
            KeyCode::Char('x') => {
                if let Some(server) = self.tree_server() {
                    self.disconnect(server);
                }
            }
            KeyCode::Char('t') => {
                if let Some(server) = self.tree_server() {
                    self.trust(server);
                }
            }
            KeyCode::Char('m') => self.set_mute(false),
            KeyCode::Char('d') => self.set_mute(true),
            KeyCode::Char('?') => self.overlay = Some(Overlay::Help { scroll: 0 }),
            KeyCode::Char('/') => {
                self.pane = Pane::Composer;
                self.composer.set("/");
            }
            _ => {}
        }
    }

    fn tree_server(&self) -> Option<usize> {
        match self.rows().get(self.tree_index())? {
            Row::Server(server)
            | Row::Channel { server, .. }
            | Row::User { server, .. }
            | Row::Direct { server, .. } => Some(*server),
        }
    }

    fn timeline_key(&mut self, key: KeyEvent) {
        let serials: Vec<u64> = self
            .open_conversation()
            .map(|(_, channel)| {
                channel
                    .timeline
                    .messages
                    .iter()
                    .map(|message| message.serial)
                    .collect()
            })
            .unwrap_or_default();
        let at = self
            .timeline
            .selected
            .and_then(|serial| serials.iter().position(|&known| known == serial));
        match key.code {
            KeyCode::Up => match at {
                Some(0) => {
                    if !self.older() {
                        self.say("This is the oldest message.");
                    }
                }
                Some(at) => self.timeline.selected = Some(serials[at - 1]),
                None if serials.is_empty() => {
                    self.older();
                }
                None => self.timeline.selected = serials.last().copied(),
            },
            KeyCode::Down => match at {
                Some(at) if at + 1 < serials.len() => {
                    self.timeline.selected = Some(serials[at + 1])
                }
                _ => {
                    if !self.newer() {
                        self.latest();
                        self.pane = Pane::Composer;
                    }
                }
            },
            KeyCode::Home => {
                self.timeline.selected = serials.first().copied();
                self.older();
            }
            KeyCode::End => {
                self.latest();
                self.timeline.selected = self.newest_message();
            }
            KeyCode::Enter | KeyCode::Char('o') => {
                if let Some(serial) = self.timeline.selected {
                    let links = self.links_of(serial);
                    match links.as_slice() {
                        [] => self.say("This message has no links."),
                        [one] => {
                            let one = one.clone();
                            self.open_link(&one);
                        }
                        _ => {
                            self.overlay = Some(Overlay::Links { links, selected: 0 });
                        }
                    }
                }
            }
            KeyCode::Char('e') => {
                if let Some(serial) = self.timeline.selected {
                    self.start_edit(Some(serial));
                }
            }
            KeyCode::Delete | KeyCode::Char('x') => {
                if let Some(serial) = self.timeline.selected {
                    self.delete(Some(serial));
                }
            }
            KeyCode::Char('m') => self.set_mute(false),
            KeyCode::Char('d') => self.set_mute(true),
            KeyCode::Char('?') => self.overlay = Some(Overlay::Help { scroll: 0 }),
            KeyCode::Char('/') => {
                self.pane = Pane::Composer;
                self.composer.set("/");
            }
            _ => {}
        }
        self.settle();
    }

    fn overlay_key(&mut self, key: KeyEvent) {
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let Some(overlay) = self.overlay.take() else {
            return;
        };
        self.overlay = match overlay {
            Overlay::Help { scroll } => match key.code {
                KeyCode::Esc | KeyCode::Char('q' | '?') | KeyCode::Enter => None,
                KeyCode::Up => Some(Overlay::Help {
                    scroll: scroll.saturating_sub(1),
                }),
                KeyCode::Down => Some(Overlay::Help { scroll: scroll + 1 }),
                KeyCode::PageUp => Some(Overlay::Help {
                    scroll: scroll.saturating_sub(10),
                }),
                KeyCode::PageDown => Some(Overlay::Help {
                    scroll: scroll + 10,
                }),
                _ => Some(Overlay::Help { scroll }),
            },
            Overlay::Finder {
                mut input,
                mut selected,
            } => {
                let items = self.finder_items(input.text());
                let at = nuschl_client::finder::selected_index(selected.as_ref(), &items);
                let pick = |step: isize, selected: &mut Option<_>| {
                    if let Some(item) = items.get(
                        at.saturating_add_signed(step)
                            .min(items.len().saturating_sub(1)),
                    ) {
                        *selected = Some((item.server.clone(), item.target.clone()));
                    }
                };
                match key.code {
                    KeyCode::Esc => None,
                    KeyCode::Enter => {
                        if let Some(item) = items.get(at) {
                            let item = item.clone();
                            self.choose(&item, alt);
                        }
                        return;
                    }
                    KeyCode::Up => {
                        pick(-1, &mut selected);
                        Some(Overlay::Finder { input, selected })
                    }
                    KeyCode::Down => {
                        pick(1, &mut selected);
                        Some(Overlay::Finder { input, selected })
                    }
                    KeyCode::PageUp => {
                        pick(-8, &mut selected);
                        Some(Overlay::Finder { input, selected })
                    }
                    KeyCode::PageDown => {
                        pick(8, &mut selected);
                        Some(Overlay::Finder { input, selected })
                    }
                    _ => {
                        if input.key(key) {
                            selected = None;
                        }
                        Some(Overlay::Finder { input, selected })
                    }
                }
            }
            Overlay::Search(mut search) => match key.code {
                KeyCode::Esc => None,
                KeyCode::Enter => {
                    let number = search.selected;
                    self.overlay = Some(Overlay::Search(search));
                    self.open_result(number);
                    return;
                }
                KeyCode::Tab => {
                    search.scope = match search.scope {
                        Scope::Conversation => Scope::Server,
                        Scope::Server => Scope::All,
                        Scope::All => Scope::Conversation,
                    };
                    self.overlay = Some(Overlay::Search(search));
                    self.run_search();
                    return;
                }
                KeyCode::Up => {
                    search.selected = search.selected.saturating_sub(1);
                    Some(Overlay::Search(search))
                }
                KeyCode::Down | KeyCode::PageDown => {
                    let step = if key.code == KeyCode::Down { 1 } else { 8 };
                    let last = search.results.len().saturating_sub(1);
                    let more = search.selected + step > last;
                    search.selected = (search.selected + step).min(last);
                    self.overlay = Some(Overlay::Search(search));
                    if more {
                        self.more_results();
                    }
                    return;
                }
                KeyCode::PageUp => {
                    search.selected = search.selected.saturating_sub(8);
                    Some(Overlay::Search(search))
                }
                _ => {
                    let changed = search.input.key(key);
                    self.overlay = Some(Overlay::Search(search));
                    if changed {
                        self.run_search();
                    }
                    return;
                }
            },
            Overlay::Delete {
                server,
                position,
                seq,
                ours,
            } => match key.code {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                    self.confirm_delete(server, position, seq);
                    None
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => None,
                _ => Some(Overlay::Delete {
                    server,
                    position,
                    seq,
                    ours,
                }),
            },
            Overlay::Prompt {
                kind,
                server,
                mut input,
                reason,
            } => match key.code {
                KeyCode::Esc => None,
                KeyCode::Enter => {
                    let value = input.take();
                    self.login(server, kind, value);
                    return;
                }
                _ => {
                    input.key(key);
                    Some(Overlay::Prompt {
                        kind,
                        server,
                        input,
                        reason,
                    })
                }
            },
            Overlay::Links { links, selected } => match key.code {
                KeyCode::Esc => None,
                KeyCode::Up => Some(Overlay::Links {
                    selected: selected.saturating_sub(1),
                    links,
                }),
                KeyCode::Down => Some(Overlay::Links {
                    selected: (selected + 1).min(links.len().saturating_sub(1)),
                    links,
                }),
                KeyCode::Enter | KeyCode::Char('o') => {
                    if let Some(link) = links.get(selected) {
                        self.open_link(&link.clone());
                    }
                    return;
                }
                KeyCode::Char(digit @ '1'..='9') => {
                    let number = digit as usize - '1' as usize;
                    if let Some(link) = links.get(number) {
                        self.open_link(&link.clone());
                        return;
                    }
                    Some(Overlay::Links { links, selected })
                }
                _ => Some(Overlay::Links { links, selected }),
            },
            Overlay::Certificate(server) => match key.code {
                KeyCode::Char('y' | 'Y' | 't') => {
                    self.trust(server);
                    return;
                }
                KeyCode::Char('n' | 'N') => {
                    self.disconnect(server);
                    None
                }
                KeyCode::Esc => None,
                _ => Some(Overlay::Certificate(server)),
            },
        };
    }
}
