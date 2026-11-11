// SPDX-License-Identifier: EUPL-1.2
//! The quick switcher (Ctrl+K, Cmd+K on macOS): channels, people and direct
//! conversations of every server, found by a few typed letters. People are
//! listed per server: a Mumble server only claims who someone is (even their
//! certificate hash), so the same name or hash on two servers is never taken
//! for one person.
use crate::{
    assets::icon,
    state::{Desktop, ServerView, Status, accessible},
    theme::{self, *},
};
use gpui_kit::{
    base::input::{Input, InputAccessibility, InputEvent, InputState, MoveDown, MoveUp},
    prelude::*,
    *,
};
use nuschel_client::Conversation;
use nuschel_session::{config::ClientSettings, connection::TextTarget};
use std::{collections::HashMap, ops::Range};

/// Conversations remembered for ranking, most recent first.
const RECENT: usize = 20;
const ROW: f32 = 48.;
const VISIBLE_ROWS: usize = 8;

/// The switcher while it is open, and where the user went recently.
#[derive(Default)]
pub struct Finder {
    open: Option<Open>,
    recent: Vec<Conversation>,
}

struct Open {
    input: Entity<InputState>,
    /// The highlighted result, kept by identity while the list changes.
    selected: Option<(String, Target)>,
    /// What the last frame listed, for keys and clicks to act on.
    items: Vec<Item>,
    /// The input's accessible label, which announces the results.
    label: String,
    scroll: UniformListScrollHandle,
    _changes: Subscription,
}

#[derive(Clone, PartialEq, Eq)]
enum Target {
    Channel(u32),
    /// Someone online, by session.
    Person(u32),
    /// A direct conversation, by its channel view's ID.
    Direct(u32),
}

#[derive(Clone)]
struct Item {
    server: String,
    target: Target,
    name: String,
    /// The channels above a channel, or where a person is.
    path: String,
    /// What else finds it, less well: a channel's path and name, the real
    /// name of someone with a nickname.
    alias: String,
    /// The matched characters of `name`.
    highlights: Vec<Range<usize>>,
    score: i32,
    unread: usize,
}

impl Finder {
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Note a conversation the user opened, for ranking.
    pub fn visited(&mut self, conversation: &Conversation) {
        self.recent.retain(|recent| recent != conversation);
        self.recent.insert(0, conversation.clone());
        self.recent.truncate(RECENT);
    }
}

/// Whether a keystroke is the primary modifier (Ctrl, Cmd on macOS) plus `key`.
impl Desktop {
    /// The switcher's and search's shortcuts (configurable in Settings →
    /// Shortcuts; by default Ctrl+K, Ctrl+F for this conversation and
    /// Ctrl+Shift+F for all servers, Cmd on macOS). Returns whether the
    /// keystroke was one.
    pub fn find_shortcut(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        use nuschel_session::config::ShortcutAction;
        match crate::shortcuts::app_action(&self.settings, keystroke) {
            Some(ShortcutAction::Find) => {
                if self.finder.is_open() {
                    self.close_finder();
                } else {
                    self.open_finder(window, cx);
                }
                true
            }
            Some(ShortcutAction::Search) => {
                self.open_search(false, window, cx);
                true
            }
            Some(ShortcutAction::SearchAll) => {
                self.open_search(true, window, cx);
                true
            }
            _ => false,
        }
    }

    /// Open the switcher, unless a dialog is open.
    pub fn open_finder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dialog_open() {
            return;
        }
        self.menu = None;
        self.emoji = None;
        let input = cx.new(|cx| {
            accessible(
                InputState::new(window, cx)
                    .placeholder("Where to? A channel, person or conversation"),
                "Find a channel or person",
                false,
                false,
            )
        });
        input.update(cx, |input, cx| input.focus(window, cx));
        let changes = cx.subscribe_in(&input, window, |this, _, event, window, cx| {
            match event {
                InputEvent::Change => {
                    if let Some(open) = &mut this.finder.open {
                        open.selected = None;
                        open.scroll.scroll_to_item(0, ScrollStrategy::Top);
                    }
                }
                InputEvent::PressEnter { shift, .. } => this.choose(None, *shift, window, cx),
                _ => return,
            }
            cx.notify();
        });
        self.finder.open = Some(Open {
            input,
            selected: None,
            items: Vec::new(),
            label: String::new(),
            scroll: UniformListScrollHandle::new(),
            _changes: changes,
        });
        cx.notify();
    }

    /// Close the switcher; focus returns to the composer.
    pub fn close_finder(&mut self) {
        self.finder.open = None;
    }

    /// Move the highlight by `step` results.
    fn finder_move(&mut self, step: isize) {
        let Some(open) = &mut self.finder.open else {
            return;
        };
        if open.items.is_empty() {
            return;
        }
        let current = selected_index(&open.selected, &open.items);
        let last = open.items.len() - 1;
        let next = current.saturating_add_signed(step).min(last);
        let item = &open.items[next];
        open.selected = Some((item.server.clone(), item.target.clone()));
        open.scroll.scroll_to_item(next, ScrollStrategy::Nearest);
    }

    /// Go to a result (the highlighted one for `None`): browse a channel or
    /// conversation, or open a direct conversation with a person. `secondary`
    /// (Shift+Enter) joins a channel's voice, or shows the channel a person
    /// is in.
    fn choose(
        &mut self,
        index: Option<usize>,
        secondary: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = &self.finder.open else {
            return;
        };
        let item = match index {
            Some(index) => open.items.get(index).cloned(),
            // What the field holds now, which a frame may not have shown yet.
            None => {
                let items = self.finder_items(&open.input.read(cx).value());
                let at = selected_index(&open.selected, &items);
                items.into_iter().nth(at)
            }
        };
        let Some(item) = item else {
            return;
        };
        self.close_finder();
        let Some(server) = self
            .servers
            .iter()
            .position(|server| server.id == item.server)
        else {
            return;
        };
        match item.target {
            Target::Channel(id) => {
                self.browse_channel(server, id, window, cx);
                if secondary {
                    self.join_selected();
                }
            }
            Target::Person(session) if secondary => {
                if let Some(channel) = self.servers[server]
                    .users
                    .get(&session)
                    .map(|user| user.channel)
                {
                    self.browse_channel(server, channel, window, cx);
                }
            }
            Target::Person(session) => self.open_direct(server, session, window, cx),
            Target::Direct(id) => self.browse_channel(server, id, window, cx),
        }
        cx.notify();
    }

    /// Everything the switcher can go to, best match for `query` first.
    fn finder_items(&self, query: &str) -> Vec<Item> {
        let query: Vec<char> = query.trim().chars().map(fold).collect();
        let selected = self.navigation.selection();
        let mut items = Vec::new();
        for server in &self.servers {
            let connected = server.status == Status::Connected;
            let voice = server.voice_channel();
            let places = Places::new(server);
            let rank = |channel: Option<u32>, unread: usize, mention: bool| {
                let conversation = channel.map(|channel| Conversation {
                    server: server.id.clone(),
                    channel,
                });
                let recent = conversation.as_ref().and_then(|conversation| {
                    self.finder
                        .recent
                        .iter()
                        .position(|recent| recent == conversation)
                });
                let mut score = recent.map_or(0, |at| 120 - 6 * at as i32);
                score += if mention {
                    80
                } else if unread > 0 {
                    40
                } else {
                    0
                };
                score += if connected { 20 } else { 0 };
                if conversation.is_some() && conversation.as_ref() == selected {
                    score -= 40;
                }
                (score, recent.is_some() || unread > 0)
            };
            for channel in &server.channels {
                let (target, path, name, alias) = match &channel.target {
                    TextTarget::Channel(id) => {
                        let path = places.path(channel.parent).join(" › ");
                        let alias = format!("{path} {}", channel.name);
                        (Target::Channel(*id), path, channel.name.clone(), alias)
                    }
                    // Someone online is listed as a person instead.
                    TextTarget::Direct(session)
                        if connected
                            && server.users.get(session).is_some_and(|user| {
                                channel
                                    .direct_peer
                                    .as_ref()
                                    .is_none_or(|peer| peer.instance == user.instance)
                            }) =>
                    {
                        continue;
                    }
                    TextTarget::Direct(_) => {
                        let (name, alias) = match &channel.direct_peer {
                            Some(peer) => person(peer, &self.settings),
                            None => (channel.name.trim_start_matches("@ ").into(), String::new()),
                        };
                        let path = "Direct conversation".to_string();
                        (Target::Direct(channel.id), path, name, alias)
                    }
                };
                let (mut score, suggested) =
                    rank(Some(channel.id), channel.unread, channel.unread_mention);
                if matches!(target, Target::Channel(id) if Some(id) == voice) {
                    score += 30;
                }
                items.push(Item {
                    server: server.id.clone(),
                    target,
                    name,
                    path,
                    alias,
                    highlights: Vec::new(),
                    score: if suggested || !query.is_empty() {
                        score
                    } else {
                        i32::MIN
                    },
                    unread: channel.unread,
                });
            }
            if !connected {
                continue;
            }
            for user in server.users.values() {
                if Some(user.session) == server.own_session {
                    continue;
                }
                // Their direct conversation, if one is open.
                let direct = server.channels.iter().find(|channel| {
                    channel.target == TextTarget::Direct(user.session)
                        && channel
                            .direct_peer
                            .as_ref()
                            .is_none_or(|peer| peer.instance == user.instance)
                });
                let (mut score, suggested) = rank(
                    direct.map(|channel| channel.id),
                    direct.map_or(0, |channel| channel.unread),
                    direct.is_some_and(|channel| channel.unread_mention),
                );
                if server.talking.contains(&user.session) {
                    score += 20;
                }
                if Some(user.channel) == voice {
                    score += 30;
                }
                let (name, alias) = person(user, &self.settings);
                let mut path = places.path(Some(user.channel));
                if path.is_empty() {
                    path.push(places.name(user.channel));
                }
                items.push(Item {
                    server: server.id.clone(),
                    target: Target::Person(user.session),
                    path: format!("in {}", path.join(" › ")),
                    name,
                    alias,
                    highlights: Vec::new(),
                    score: if suggested || !query.is_empty() {
                        score
                    } else {
                        i32::MIN
                    },
                    unread: direct.map_or(0, |channel| channel.unread),
                });
            }
        }
        if query.is_empty() {
            // Suggestions: where the user was lately, and what is unread.
            items.retain(|item| item.score != i32::MIN);
        } else {
            items.retain_mut(|item| {
                let found = fuzzy(&query, &item.name).or_else(|| {
                    fuzzy(&query, &item.alias).map(|(score, _)| (score - 150, Vec::new()))
                });
                let Some((score, highlights)) = found else {
                    return false;
                };
                item.score += score;
                item.highlights = highlights;
                true
            });
        }
        items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.name.cmp(&b.name)));
        items
    }
}

/// A person's shown name (your nickname for them, else theirs), and their
/// own name behind a nickname.
fn person(user: &nuschel_session::connection::User, settings: &ClientSettings) -> (String, String) {
    let name = crate::names::display_name(user, settings);
    let real = if name == user.name { "" } else { &user.name };
    (name.to_string(), real.to_string())
}

/// Where the highlight is in `items`: the selected result, or the first.
fn selected_index(selected: &Option<(String, Target)>, items: &[Item]) -> usize {
    selected
        .as_ref()
        .and_then(|(server, target)| {
            items
                .iter()
                .position(|item| &item.server == server && &item.target == target)
        })
        .unwrap_or(0)
}

/// One server's channel names and parents, for paths.
struct Places<'a>(HashMap<u32, (&'a str, Option<u32>)>);
impl<'a> Places<'a> {
    fn new(server: &'a ServerView) -> Self {
        Self(
            server
                .channels
                .iter()
                .filter(|channel| matches!(channel.target, TextTarget::Channel(_)))
                .map(|channel| (channel.id, (channel.name.as_str(), channel.parent)))
                .collect(),
        )
    }

    fn name(&self, id: u32) -> &'a str {
        self.0.get(&id).map_or("", |(name, _)| name)
    }

    /// The names from the top down to `channel`, without the root (the
    /// server itself).
    fn path(&self, channel: Option<u32>) -> Vec<&'a str> {
        let mut names = Vec::new();
        let mut current = channel;
        // Bounded, should a server send a cycle.
        while let Some(id) = current
            && names.len() < 32
        {
            let Some(&(name, parent)) = self.0.get(&id) else {
                break;
            };
            if parent.is_none() {
                break;
            }
            names.push(name);
            current = parent;
        }
        names.reverse();
        names
    }
}

/// Case folding for matching, one character for one.
fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// How well `query` (folded, not empty) matches `text`, and the byte ranges
/// of `text` that matched. Best is all of it, then its start, the start of a
/// word, any run of it, then letters in order (word initials first).
fn fuzzy(query: &[char], text: &str) -> Option<(i32, Vec<Range<usize>>)> {
    let original: Vec<(usize, char)> = text.char_indices().collect();
    let chars: Vec<char> = original.iter().map(|&(_, c)| fold(c)).collect();
    let (n, m) = (chars.len(), query.len());
    if m == 0 || m > n {
        return None;
    }
    let offset = |k: usize| original.get(k).map_or(text.len(), |&(at, _)| at);
    let word_start = |k: usize| {
        k == 0 || {
            let (before, here) = (original[k - 1].1, original[k].1);
            (!before.is_alphanumeric() && here.is_alphanumeric())
                || (before.is_lowercase() && here.is_uppercase())
        }
    };
    let runs: Vec<usize> = (0..=n - m)
        .filter(|&start| chars[start..start + m] == *query)
        .collect();
    if let Some(&start) = runs
        .iter()
        .find(|&&start| word_start(start))
        .or(runs.first())
    {
        let score = match start {
            0 if m == n => 1000,
            0 => 900 - (n - m).min(99) as i32,
            _ if word_start(start) => 700 - start.min(99) as i32,
            _ => 500 - start.min(99) as i32,
        };
        let run = offset(start)..offset(start + m);
        return Some((score, Vec::from([run])));
    }
    // Letters in order: each at the next word start if there is one, else
    // the next occurrence; failing that, plainly the next occurrence.
    let pick = |prefer_words: bool| {
        let mut positions = Vec::with_capacity(m);
        let mut from = 0;
        for &wanted in query {
            let next = (from..n)
                .find(|&k| prefer_words && chars[k] == wanted && word_start(k))
                .or_else(|| (from..n).find(|&k| chars[k] == wanted))?;
            positions.push(next);
            from = next + 1;
        }
        Some(positions)
    };
    let positions = pick(true).or_else(|| pick(false))?;
    let score = if positions.iter().all(|&k| word_start(k)) {
        400
    } else {
        let spread = positions[m - 1] - positions[0] + 1 - m;
        200 - spread.min(150) as i32
    };
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for k in positions {
        match ranges.last_mut() {
            Some(range) if range.end == offset(k) => range.end = offset(k + 1),
            _ => ranges.push(offset(k)..offset(k + 1)),
        }
    }
    Some((score, ranges))
}

/// What a result is, for its icon and accessible label.
fn kind(target: &Target) -> &'static str {
    match target {
        Target::Channel(_) => "channel",
        Target::Person(_) => "person",
        Target::Direct(_) => "direct conversation",
    }
}

fn row(
    this: &Desktop,
    index: usize,
    item: &Item,
    selected: bool,
    count: usize,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let server = this.servers.iter().find(|server| server.id == item.server);
    let server_name = server.map_or("", |server| server.name.as_str()).to_string();
    let place = match &item.target {
        Target::Channel(_) if !item.path.is_empty() => format!(" in {}", item.path),
        Target::Person(_) => format!(" {}", item.path),
        _ => String::new(),
    };
    let unread = match item.unread {
        0 => String::new(),
        1 => ", 1 unread message".into(),
        count => format!(", {count} unread messages"),
    };
    let label = format!(
        "{}, {}{place}, {server_name}{unread}",
        item.name,
        kind(&item.target)
    );
    let marker = match &item.target {
        Target::Channel(_) => icon("hash", 18., color(TEXT_FAINT)).into_any_element(),
        Target::Direct(_) => icon("message", 18., color(TEXT_FAINT)).into_any_element(),
        Target::Person(session) => {
            let user = server.and_then(|server| server.users.get(session));
            let image = server.and_then(|server| crate::view::avatar_image(server, user));
            let talking = server.is_some_and(|server| server.talking.contains(session));
            crate::view::avatar(&item.name, 24., talking, ("finder-avatar", index), image)
        }
    };
    let name =
        StyledText::new(item.name.clone()).with_highlights(item.highlights.iter().map(|range| {
            (
                range.clone(),
                HighlightStyle {
                    color: Some(color(TEXT).into()),
                    font_weight: Some(FontWeight::BOLD),
                    underline: Some(UnderlineStyle {
                        thickness: px(1.),
                        color: Some(color(ACCENT).into()),
                        wavy: false,
                    }),
                    ..Default::default()
                },
            )
        }));
    let channel = matches!(item.target, Target::Channel(_));
    let server_badge = div()
        .flex_shrink_0()
        .max_w(px(160.))
        .flex()
        .items_center()
        .gap(px(6.))
        .text_xs()
        .text_color(color(TEXT_FAINT))
        .child(match server.and_then(|server| server.icon.clone()) {
            Some(image) => img(image).size(px(16.)).rounded(px(4.)).into_any_element(),
            None => div()
                .size(px(16.))
                .flex_shrink_0()
                .rounded(px(4.))
                .bg(theme::avatar(&server_name))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(8.))
                .font_weight(FontWeight::BOLD)
                .text_color(color(WHITE))
                .child(Text::new_inaccessible(theme::initials(&server_name).into()))
                .into_any_element(),
        })
        .child(
            div()
                .min_w_0()
                .truncate()
                .child(Text::new_inaccessible(server_name.into())),
        );
    div()
        .id(("finder-item", index))
        .role(Role::ListBoxOption)
        .aria_label(label)
        .aria_selected(selected)
        .aria_position_in_set(index + 1)
        .aria_size_of_set(count)
        .group("finder-item")
        .w_full()
        .h(px(ROW))
        .px_3()
        .flex()
        .items_center()
        .gap_3()
        .rounded(px(8.))
        .cursor_pointer()
        .when(selected, |row| row.bg(color(SELECTED)))
        .hover(|style| style.bg(color(HOVER)))
        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
            this.choose(Some(index), event.modifiers().shift, window, cx);
        }))
        .child(
            div()
                .w(px(24.))
                .flex_shrink_0()
                .flex()
                .justify_center()
                .child(marker),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_baseline()
                .gap_2()
                .child(
                    div()
                        .flex_shrink_0()
                        .max_w(relative(0.7))
                        .truncate()
                        .text_color(color(if selected { TEXT } else { TEXT_MUTED }))
                        .child(name),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .text_color(color(TEXT_FAINT))
                        .child(Text::new_inaccessible(item.path.clone().into())),
                ),
        )
        .when(item.unread > 0, |row| {
            row.child(
                div()
                    .flex_shrink_0()
                    .px(px(6.))
                    .rounded_full()
                    .bg(color(ACCENT))
                    .text_xs()
                    .font_weight(FontWeight::BOLD)
                    .text_color(color(WHITE))
                    .child(Text::new_inaccessible(
                        item.unread.min(999).to_string().into(),
                    )),
            )
        })
        .when(channel, |row| {
            row.child(
                div()
                    .id(("finder-join", index))
                    .role(Role::Button)
                    .aria_label("Join voice")
                    .size(px(28.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.))
                    .when(!selected, |button| {
                        button
                            .invisible()
                            .group_hover("finder-item", |style| style.visible())
                    })
                    .hover(|style| style.bg(color(BORDER_STRONG)))
                    .tooltip(|_, cx| {
                        cx.new(|_| crate::dialogs::Tooltip("Join voice (Shift+Enter)".into()))
                            .into()
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.choose(Some(index), true, window, cx);
                    }))
                    .child(icon("volume", 16., color(TEXT_MUTED))),
            )
        })
        .child(server_badge)
        .into_any_element()
}

/// The open switcher over the window; a click outside closes it. Lists
/// what matches now (people come and go while it is open), keeps keyboard
/// focus in its field, and names the result count and highlighted result to
/// assistive technology through that field.
pub fn overlay(
    this: &mut Desktop,
    window: &mut Window,
    cx: &mut Context<Desktop>,
) -> Option<AnyElement> {
    let input = this.finder.open.as_ref()?.input.clone();
    let query = input.read(cx).value().to_string();
    let items = this.finder_items(&query);
    let open = this.finder.open.as_mut()?;
    open.items = items;
    let selected = selected_index(&open.selected, &open.items);
    let count = open.items.len();
    let label = match open.items.get(selected) {
        None if query.trim().is_empty() => "Find a channel or person".to_string(),
        None => "Find a channel or person: no results".to_string(),
        Some(item) => format!(
            "Find a channel or person: {count} {}, {} ({}) highlighted",
            if count == 1 { "result" } else { "results" },
            item.name,
            kind(&item.target)
        ),
    };
    if open.label != label {
        open.label = label.clone();
        input.update(cx, |input, _| {
            input.set_accessibility(
                InputAccessibility::new(Some(Role::TextInput)).with_label(Some(label.into())),
            )
        });
    }
    let scroll = open.scroll.clone();
    if !input.read(cx).focus_handle(cx).is_focused(window) {
        input.update(cx, |input, cx| input.focus(window, cx));
    }
    // Focus returns to the composer once it closes (see keep_dialog_focus).
    this.modal_shown.set(Some("finder"));
    let empty = if query.trim().is_empty() {
        "Type to find a channel, person or conversation on any server."
    } else {
        "Nothing matches. Messages are found with Ctrl+F."
    };
    let list = uniform_list(
        "finder-results",
        count,
        cx.processor(move |this, range: Range<usize>, _, cx| {
            let Some(open) = &mut this.finder.open else {
                return Vec::new();
            };
            // Out for the rows, which read the rest of the desktop.
            let items = std::mem::take(&mut open.items);
            let rows = range
                .filter_map(|index| items.get(index).map(|item| (index, item)))
                .map(|(index, item)| row(this, index, item, index == selected, items.len(), cx))
                .collect();
            if let Some(open) = &mut this.finder.open {
                open.items = items;
            }
            rows
        }),
    )
    .track_scroll(&scroll)
    .w_full()
    .h(px(ROW * count.clamp(1, VISIBLE_ROWS) as f32));
    Some(
        div()
            .id("finder-backdrop")
            .absolute()
            .inset_0()
            .bg(color(SCRIM))
            .flex()
            .flex_col()
            .items_center()
            .pt(relative(0.12))
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.close_finder();
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("finder")
                    .role(Role::Dialog)
                    .aria_label("Quick switcher")
                    .track_focus(&this.modal_focus)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    // Ahead of the field, which would move its cursor.
                    .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                        this.finder_move(-1);
                        cx.stop_propagation();
                        cx.notify();
                    }))
                    .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                        this.finder_move(1);
                        cx.stop_propagation();
                        cx.notify();
                    }))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        let step = match event.keystroke.key.as_str() {
                            "escape" => {
                                this.close_finder();
                                0
                            }
                            "pageup" => -(VISIBLE_ROWS as isize),
                            "pagedown" => VISIBLE_ROWS as isize,
                            _ => return,
                        };
                        this.finder_move(step);
                        cx.stop_propagation();
                        cx.notify();
                    }))
                    .w(px(600.))
                    .max_w(relative(0.92))
                    .flex()
                    .flex_col()
                    .rounded(px(12.))
                    .bg(color(ELEVATED))
                    .border_1()
                    .border_color(color(BORDER_STRONG))
                    .shadow_lg()
                    .overflow_hidden()
                    .child(
                        div()
                            .m_3()
                            .px_3()
                            .py(px(10.))
                            .rounded(px(8.))
                            .bg(color(INPUT))
                            .border_1()
                            .border_color(color(ACCENT))
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(icon("search", 16., color(TEXT_MUTED)))
                            .child(div().flex_1().child(Input::new(&input))),
                    )
                    .child(
                        div()
                            .id("finder-list")
                            .role(Role::ListBox)
                            .aria_label(match count {
                                1 => "1 result".to_string(),
                                count => format!("{count} results"),
                            })
                            .px_2()
                            .when(count == 0, |list| {
                                list.h(px(ROW))
                                    .px_4()
                                    .flex()
                                    .items_center()
                                    .text_sm()
                                    .text_color(color(TEXT_MUTED))
                                    .child(text!(empty).with_id("finder-empty"))
                            })
                            .when(count > 0, |view| view.child(list)),
                    )
                    .child(
                        div()
                            .mt_2()
                            .px_4()
                            .py_2()
                            .border_t_1()
                            .border_color(color(BORDER))
                            .flex()
                            .gap_4()
                            .text_xs()
                            .text_color(color(TEXT_FAINT))
                            .child(text!("↑↓ choose").with_id("finder-hint-move"))
                            .child(text!("Enter open").with_id("finder-hint-open"))
                            .child(text!("Shift+Enter join voice").with_id("finder-hint-join"))
                            .child(text!("Esc close").with_id("finder-hint-close")),
                    ),
            )
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::fuzzy;

    fn score(query: &str, text: &str) -> Option<i32> {
        let query: Vec<char> = query.chars().map(super::fold).collect();
        fuzzy(&query, text).map(|(score, _)| score)
    }

    #[test]
    fn prefixes_and_word_starts_beat_other_matches() {
        assert!(score("dev", "Dev") > score("dev", "Development"));
        assert!(score("dev", "Development") > score("dev", "Web Development"));
        assert!(score("dev", "Web Development") > score("dev", "Abdevel"));
        assert!(score("dev", "Abdevel") > score("wd", "Web Development"));
        assert!(score("wd", "Web Development") > score("wbd", "Web Development"));
        assert_eq!(score("xyz", "Web Development"), None);
        assert_eq!(score("ÄR", "Ärger"), Some(900 - 3));
        // CamelCase counts as words.
        assert_eq!(score("gt", "GameTime"), Some(400));
    }

    #[test]
    fn highlights_are_byte_ranges_of_the_matched_letters() {
        let query: Vec<char> = "wde".chars().collect();
        let (_, ranges) = fuzzy(&query, "Wö Development").unwrap();
        assert_eq!(ranges, [0..1, 4..6]);
    }
}
