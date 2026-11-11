// SPDX-License-Identifier: EUPL-1.2
//! The full screen: channels and people on the left, the open conversation
//! on the right with who is typing and the composer under it, two status
//! lines at the bottom, and overlays (help, switcher, search, questions).
//! Nothing is told by colour alone: states are words ("talking", "muted",
//! "3 unread"), the row or message the keyboard is on starts with ">", and
//! the terminal's cursor sits where the keyboard is, so screen readers that
//! follow it read the right line.
use crate::{
    app::{App, Overlay, Pane, Row},
    document::{self, clean},
    keys,
};
use nuschl_client::{
    Client,
    conversation::Entry,
    model::{TextTarget, User},
    names,
    search::Scope,
    server::{ServerState, Status},
    timeline::{Message, MessageStatus},
};
use nuschl_core::model::RejectKind;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph},
};
use ratatui_image::sliced::{SlicedImage, SlicedProtocol};
use std::sync::Arc;
use unicode_width::UnicodeWidthStr;

/// How long a note stays in the status line (warnings longer).
const NOTE_MS: u64 = 15_000;

const FOCUSED: Style = Style::new().add_modifier(Modifier::REVERSED);

/// "Ada (you), talking, muted": a person and their state, in words.
pub fn person_label(client: &Client, server: &ServerState, user: &User) -> String {
    let mut label = clean(names::display_name(user, &client.preferences));
    let mut states = Vec::new();
    if server.own_session == Some(user.session) {
        states.push("you");
    }
    if server.talking.contains(&user.session) {
        states.push("talking");
    }
    if user.deafened || user.self_deafened {
        states.push("deafened");
    } else if user.muted || user.self_muted {
        states.push("muted");
    }
    if user.suppressed {
        states.push("suppressed");
    }
    if user.priority_speaker {
        states.push("priority speaker");
    }
    if user.recording {
        states.push("recording");
    }
    if !states.is_empty() {
        label.push_str(&format!(" ({})", states.join(", ")));
    }
    label
}

fn unread(count: usize, mention: bool) -> String {
    match (count, mention) {
        (0, _) => String::new(),
        (count, true) => format!(" ({count} unread, mention)"),
        (count, false) => format!(" ({count} unread)"),
    }
}

/// A row of the channel pane as text.
pub fn row_text(app: &App, row: &Row) -> String {
    let client = &app.client;
    match *row {
        Row::Server(index) => {
            let server = &client.servers[index];
            let mut text = format!("{}: {}", clean(&server.name), server.status_label());
            if server.certificate.is_some() {
                text.push_str(" (certificate: t to trust)");
            } else if server.status != Status::Connected
                && let Some(reconnecting) = &server.reconnecting
            {
                text.push_str(&format!(" ({})", clean(reconnecting)));
            }
            text
        }
        Row::Channel { server, row } => {
            let state = &client.servers[server];
            let channel = &state.channels[row.index];
            let mut text = "  ".repeat(row.depth);
            text.push_str(match (row.has_children, row.expanded) {
                (false, _) => "  ",
                (true, true) => "- ",
                (true, false) => "+ ",
            });
            text.push_str(&clean(&channel.name));
            if let Some(voice) = state.voice_channel()
                && TextTarget::Channel(voice) == channel.target
            {
                text.push_str(" (your voice)");
            }
            if row.hidden_users > 0 {
                text.push_str(&format!(
                    " ({} inside{})",
                    row.hidden_users,
                    if row.hidden_talking { ", talking" } else { "" }
                ));
            }
            text.push_str(&unread(channel.unread, channel.unread_mention));
            text
        }
        Row::User {
            server,
            session,
            depth,
            listener,
        } => {
            let state = &client.servers[server];
            let Some(user) = state.users.get(&session) else {
                return String::new();
            };
            format!(
                "{}{}{}",
                "  ".repeat(depth + 1),
                person_label(client, state, user),
                if listener { " (listening)" } else { "" }
            )
        }
        Row::Direct { server, index } => {
            let channel = &client.servers[server].channels[index];
            format!(
                "  {}{}",
                clean(&channel.name),
                unread(channel.unread, channel.unread_mention)
            )
        }
    }
}

/// The first line of a message: time, author, its state.
fn header(app: &App, server: &ServerState, message: &Message, peer: Option<&User>) -> String {
    let author = names::author(&message.author, server.users.values(), peer);
    let author = names::author_name(&message.author, author, &app.client.preferences);
    let mut text = format!("{} {}", app.time(message.observed_at_ms), clean(author));
    if message.edited_ms.is_some() && message.deleted.is_none() {
        text.push_str(" (edited)");
    }
    if let Some(label) = message.status.label()
        && message.status != MessageStatus::Sent
    {
        text.push_str(&format!(" ({label})"));
    }
    text.push(':');
    text
}

/// A message as lines: the header and its first line, then the rest
/// indented; a file offer as text.
pub fn message_lines(
    app: &App,
    server: &ServerState,
    peer: Option<&User>,
    message: &Message,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let header = header(app, server, message, peer);
    let mut body = if let Some(deletion) = message.deleted {
        vec![Line::styled(
            if deletion.moderator {
                "(deleted by a moderator)"
            } else {
                "(deleted)"
            },
            Style::new().add_modifier(Modifier::ITALIC),
        )]
    } else {
        let mut links = Vec::new();
        let mut body = document::lines(&message.document, &message.source, &mut links);
        for (number, link) in links.iter().enumerate() {
            body.push(Line::styled(
                format!("[{}] {link}", number + 1),
                Style::new().add_modifier(Modifier::DIM),
            ));
        }
        body
    };
    if let Some(offer) = &message.offer {
        body.push(Line::from(format!(
            "(a file offered: {}; file transfers are not in the terminal client yet)",
            clean(&offer.name)
        )));
    }
    let mut first = vec![Span::styled(
        header,
        Style::new().add_modifier(Modifier::BOLD),
    )];
    if let Some(line) = body.first_mut() {
        first.push(Span::raw(" "));
        first.append(&mut line.spans);
    }
    lines.push(Line::from(first));
    for line in body.into_iter().skip(1) {
        let mut spans = vec![Span::raw("  ")];
        spans.extend(line.spans);
        lines.push(Line::from(spans));
    }
    lines
}

/// The open conversation as wrapped lines, each with the serial of the
/// message it belongs to.
type Picture = (usize, Arc<SlicedProtocol>);
type TimelineLines = (Vec<(Option<u64>, Line<'static>)>, Vec<Picture>);

fn timeline_lines(app: &App, width: usize) -> TimelineLines {
    let Some((server, channel)) = app.open_conversation() else {
        return (
            vec![(
                None,
                Line::from("Open a channel or conversation: Tab to the channels, or Ctrl+K."),
            )],
            Vec::new(),
        );
    };
    let mut out = Vec::new();
    let mut pictures = Vec::new();
    let timeline = &channel.timeline;
    let top = if timeline.loading {
        Some("Loading…")
    } else if timeline.next_before.is_some() {
        Some("Older messages: PageUp or Home")
    } else if timeline.beginning && !timeline.messages.is_empty() {
        Some("The beginning of the conversation")
    } else if timeline.messages.is_empty() && channel.activity.is_empty() {
        Some("No messages yet.")
    } else {
        None
    };
    if let Some(top) = top {
        out.push((
            None,
            Line::styled(top, Style::new().add_modifier(Modifier::DIM)),
        ));
    }
    let jumped = app
        .jumped
        .as_ref()
        .filter(|(conversation, _)| {
            conversation.server == server.id && conversation.channel == channel.id
        })
        .map(|(_, id)| *id);
    for entry in channel.entries(|_| true) {
        match entry {
            Entry::Run { .. } => {}
            Entry::Activity(index) => {
                let line = &channel.activity[index];
                let marker = if line.warning { "! " } else { "· " };
                let style = if line.warning {
                    Style::new().add_modifier(Modifier::BOLD)
                } else {
                    Style::new().add_modifier(Modifier::DIM)
                };
                let text = format!(
                    "  {marker}{} {}",
                    app.time(line.observed_at_ms),
                    clean(&line.text)
                );
                for wrapped in document::wrap(Line::styled(text, style), width, 4) {
                    out.push((None, wrapped));
                }
            }
            Entry::Message { position, .. } => {
                let message = &timeline.messages[position];
                let selected =
                    app.pane == Pane::Timeline && app.timeline.selected == Some(message.serial);
                let marker = if selected {
                    "> "
                } else if jumped.is_some() && message.id == jumped {
                    "» "
                } else {
                    "  "
                };
                for (index, line) in
                    message_lines(app, server, channel.direct_peer.as_ref(), message)
                        .into_iter()
                        .enumerate()
                {
                    let mut spans = vec![Span::raw(if index == 0 { marker } else { "  " })];
                    spans.extend(line.spans);
                    let mut line = Line::from(spans);
                    if selected {
                        line = line.patch_style(FOCUSED);
                    }
                    for wrapped in document::wrap(line, width, 4) {
                        out.push((Some(message.serial), wrapped));
                    }
                }
                if message.deleted.is_none() {
                    for picture in app.graphics.borrow_mut().pictures(
                        &message.document,
                        width.saturating_sub(4).min(120) as u16,
                        false,
                    ) {
                        pictures.push((out.len(), picture.clone()));
                        for _ in 0..picture.size().height {
                            out.push((Some(message.serial), Line::default()));
                        }
                    }
                }
            }
        }
    }
    (out, pictures)
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [main, note, state] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let tree_width = (main.width / 3).clamp(24.min(main.width), 40);
    let [tree, right] =
        Layout::horizontal([Constraint::Length(tree_width), Constraint::Fill(1)]).areas(main);
    let composer_lines = app.composer.text().split('\n').count().clamp(1, 6) as u16;
    let [timeline, typing, composer] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(composer_lines + 2),
    ])
    .areas(right);
    let mut cursor = None;
    draw_tree(frame, app, tree, &mut cursor);
    draw_timeline(frame, app, timeline, &mut cursor);
    draw_typing(frame, app, typing);
    draw_composer(frame, app, composer, &mut cursor);
    draw_status(frame, app, note, state);
    if app.overlay.is_some() {
        cursor = None;
        draw_overlay(frame, app, &mut cursor);
    }
    if let Some(position) = cursor {
        frame.set_cursor_position(position);
    }
}

fn pane_block(title: String, focused: bool) -> Block<'static> {
    let title = if focused {
        format!(" {title} (active) ")
    } else {
        format!(" {title} ")
    };
    let block = Block::bordered().title(title);
    if focused {
        block.border_style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD))
    } else {
        block.border_style(Style::new().add_modifier(Modifier::DIM))
    }
}

fn draw_tree(frame: &mut Frame, app: &App, area: Rect, cursor: &mut Option<Position>) {
    let focused = app.pane == Pane::Tree;
    let block = pane_block("Channels".into(), focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows = app.rows();
    let at = app.tree_index();
    let height = usize::from(inner.height);
    let start = at
        .saturating_sub(height.saturating_sub(1) / 2)
        .min(rows.len().saturating_sub(height));
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(start)
        .take(height)
        .map(|(index, row)| {
            let selected = focused && index == at;
            let text = format!("{}{}", if selected { ">" } else { " " }, row_text(app, row));
            let mut style = match row {
                Row::Server(server) => Style::new()
                    .fg(if app.client.servers[*server].status == Status::Connected {
                        Color::Cyan
                    } else {
                        Color::DarkGray
                    })
                    .add_modifier(Modifier::BOLD),
                Row::User {
                    server, session, ..
                } if app.client.servers[*server].talking.contains(session) => {
                    Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)
                }
                Row::Channel { server, row }
                    if app.client.servers[*server].channels[row.index].unread > 0 =>
                {
                    Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                }
                Row::Channel { .. } | Row::Direct { .. } => Style::new().fg(Color::Blue),
                Row::Direct { server, index }
                    if app.client.servers[*server].channels[*index].unread > 0 =>
                {
                    Style::new().add_modifier(Modifier::BOLD)
                }
                _ => Style::new(),
            };
            if selected {
                style = style.patch(FOCUSED);
            }
            Line::styled(text, style)
        })
        .collect();
    if focused && at >= start {
        *cursor = Some(Position::new(inner.x, inner.y + (at - start) as u16));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_timeline(frame: &mut Frame, app: &mut App, area: Rect, cursor: &mut Option<Position>) {
    let focused = app.pane == Pane::Timeline;
    let title = match app.open_conversation() {
        Some((server, channel)) => {
            let mut title = format!("{} on {}", clean(&channel.name), clean(&server.name));
            if let Some(voice) = server.voice_channel()
                && channel.target == TextTarget::Channel(voice)
            {
                title.push_str(", your voice channel");
            }
            title
        }
        None => "Messages".into(),
    };
    let block = pane_block(title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = usize::from(inner.width);
    let height = usize::from(inner.height);
    let (lines, pictures) = timeline_lines(app, width);
    let total = lines.len();
    let max = total.saturating_sub(height);
    app.timeline.max = max;
    app.timeline.height = height;
    let mut scroll = app.timeline.scroll.min(max);
    // The selected message stays in view.
    if focused
        && let Some(serial) = app.timeline.selected
        && let Some(first) = lines.iter().position(|(owner, _)| *owner == Some(serial))
    {
        let last = lines
            .iter()
            .rposition(|(owner, _)| *owner == Some(serial))
            .unwrap_or(first);
        let bottom = total - scroll;
        let top = bottom.saturating_sub(height);
        if first < top {
            scroll = total.saturating_sub(first + height).min(max);
        } else if last >= bottom {
            scroll = total.saturating_sub(last + 1);
        }
    }
    app.timeline.scroll = scroll;
    let bottom = total - scroll;
    let top = bottom.saturating_sub(height);
    // A short conversation sits at the bottom, by the composer.
    let padding = height.saturating_sub(bottom - top);
    if focused
        && let Some(serial) = app.timeline.selected
        && let Some(first) = lines[top..bottom]
            .iter()
            .position(|(owner, _)| *owner == Some(serial))
    {
        *cursor = Some(Position::new(inner.x, inner.y + (padding + first) as u16));
    }
    let visible: Vec<Line> = std::iter::repeat_n(Line::default(), padding)
        .chain(lines[top..bottom].iter().map(|(_, line)| line.clone()))
        .collect();
    frame.render_widget(Paragraph::new(visible), inner);
    // Request only messages actually in the viewport, preventing a long
    // history from flooding the workers/cache with off-screen formulas.
    if let Some((_, channel)) = app.open_conversation() {
        let owners: std::collections::BTreeSet<_> = lines[top..bottom]
            .iter()
            .filter_map(|(owner, _)| *owner)
            .collect();
        for message in &channel.timeline.messages {
            if owners.contains(&message.serial) && message.deleted.is_none() {
                app.graphics.borrow_mut().pictures(
                    &message.document,
                    width.saturating_sub(4).min(120) as u16,
                    true,
                );
            }
        }
    }
    if inner.width > 4 && inner.height > 0 {
        // A signed origin lets the library clip rows above/below the viewport.
        // Every image is generated from validated worker pixels, never escapes
        // received from a server. Widths are fitted before encoding.
        let viewport = Rect::new(inner.x + 4, inner.y, inner.width - 4, inner.height);
        for (at, picture) in pictures {
            let y = at as i64 + padding as i64 - top as i64;
            if y < i64::from(inner.height) && y + i64::from(picture.size().height) > 0 {
                frame.render_widget(SlicedImage::new(&picture, (0, y as i16).into()), viewport);
            }
        }
    }
}

fn draw_typing(frame: &mut Frame, app: &App, area: Rect) {
    let Some((server, channel)) = app.open_conversation() else {
        return;
    };
    let text = if app.timeline.scroll > 0 && channel.new_messages > 0 {
        Some(format!(
            "{} new message{} below: End to read them",
            channel.new_messages,
            if channel.new_messages == 1 { "" } else { "s" }
        ))
    } else {
        server
            .typing_line(channel, &app.client.preferences, crate::app::monotonic_ms())
            .map(|line| clean(&line))
    };
    if let Some(text) = text {
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!(" {text}"),
                Style::new().add_modifier(Modifier::ITALIC),
            )),
            area,
        );
    }
}

fn draw_composer(frame: &mut Frame, app: &App, area: Rect, cursor: &mut Option<Position>) {
    let focused = app.pane == Pane::Composer;
    let title = if let Some(label) = app.clipboard.label() {
        label.to_string()
    } else if app.editing.is_some() {
        "Editing your message: Enter saves, Esc cancels".to_string()
    } else {
        match app.open_conversation() {
            Some((server, _)) if server.status != Status::Connected => {
                format!("Message (not connected to {})", clean(&server.name))
            }
            Some((_, channel)) if app.client.sending() => {
                format!("Message to {} (sending…)", clean(&channel.name))
            }
            Some((_, channel)) => format!("Message to {}", clean(&channel.name)),
            None => "Message".into(),
        }
    };
    let block = pane_block(title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let (column, line) = app.composer.cursor_position();
    let height = usize::from(inner.height.max(1));
    let skip = line.saturating_sub(height - 1);
    let width = usize::from(inner.width.max(1));
    // A long line scrolls sideways to keep the cursor in view.
    let shift = column.saturating_sub(width.saturating_sub(1));
    let lines: Vec<Line> = app
        .composer
        .text()
        .split('\n')
        .skip(skip)
        .take(height)
        .map(|text| Line::from(skip_columns(text, shift)))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
    if focused {
        *cursor = Some(Position::new(
            inner.x + (column - shift) as u16,
            inner.y + (line - skip) as u16,
        ));
    }
}

/// `text` without its first `columns` columns.
fn skip_columns(text: &str, columns: usize) -> String {
    let mut used = 0;
    let mut out = String::new();
    for c in text.chars() {
        if used >= columns {
            out.push(c);
        } else {
            used += c.to_string().width();
        }
    }
    out
}

fn draw_status(frame: &mut Frame, app: &App, note_area: Rect, state_area: Rect) {
    let now = crate::app::monotonic_ms();
    if let Some(note) = app.notes.back().filter(|note| {
        now.saturating_sub(note.at_ms) < if note.warning { NOTE_MS * 4 } else { NOTE_MS }
    }) {
        let style = if note.warning {
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::Cyan)
        };
        let text = note.text.replace('\n', " ");
        frame.render_widget(
            Paragraph::new(Line::styled(format!(" {text}"), style)),
            note_area,
        );
    }
    let mut state = String::new();
    if let Some(index) = app.current_server() {
        let server = &app.client.servers[index];
        state.push_str(&format!(
            " {}: {}",
            clean(&server.name),
            server.status_label()
        ));
        if server.status == Status::Connected {
            state.push_str(match (server.muted, server.deafened) {
                (_, true) => ", deafened",
                (true, false) => ", muted",
                (false, false) => ", unmuted",
            });
            if !app.options.audio {
                state.push_str(" (voice off)");
            } else if !server.deafened {
                state.push_str(&format!(", {}", clean(&server.audio_status)));
            }
        }
        if let Some(notice) = &server.notice {
            state.push_str(&format!(". {}", clean(notice)));
        }
    }
    let hints = " F1 help · Ctrl+K find · Ctrl+F search · Ctrl+Q quit";
    let room = usize::from(state_area.width).saturating_sub(hints.width());
    let state = if state.width() > room {
        let mut cut = String::new();
        for c in state.chars() {
            if cut.width() + 2 > room {
                break;
            }
            cut.push(c);
        }
        cut.push('…');
        cut
    } else {
        format!("{state:room$}")
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(state, Style::new().fg(Color::Cyan)),
            Span::styled(hints, Style::new().add_modifier(Modifier::DIM)),
        ])),
        state_area,
    );
}

/// A centred area of at most `width` × `height`.
fn centred(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

/// A dialog's frame and its inside.
fn dialog(frame: &mut Frame, title: &str, width: u16, height: u16) -> Rect {
    let area = centred(frame.area(), width, height);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(format!(" {title} "))
        .border_style(Style::new().add_modifier(Modifier::BOLD));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

/// An input line at `area`; the cursor goes to its cursor.
fn input_line(
    frame: &mut Frame,
    area: Rect,
    label: &str,
    text: &str,
    column: usize,
    cursor: &mut Option<Position>,
) {
    let label_width = label.width();
    let room = usize::from(area.width).saturating_sub(label_width + 1);
    let shift = column.saturating_sub(room);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(label.to_string(), Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(skip_columns(text, shift)),
        ])),
        area,
    );
    *cursor = Some(Position::new(
        area.x + (label_width + column - shift) as u16,
        area.y,
    ));
}

fn list_lines(items: Vec<String>, selected: usize, height: usize) -> Vec<Line<'static>> {
    let start = selected.saturating_sub(height.saturating_sub(1));
    items
        .into_iter()
        .enumerate()
        .skip(start)
        .take(height)
        .map(|(index, text)| {
            if index == selected {
                Line::styled(format!(">{text}"), FOCUSED)
            } else {
                Line::from(format!(" {text}"))
            }
        })
        .collect()
}

fn draw_overlay(frame: &mut Frame, app: &App, cursor: &mut Option<Position>) {
    let area = frame.area();
    match &app.overlay {
        None => {}
        Some(Overlay::Help { scroll }) => {
            let inner = dialog(frame, "Help: Esc closes", 84, area.height.saturating_sub(2));
            let lines: Vec<Line> = keys::KEYS
                .iter()
                .chain([""].iter())
                .chain(keys::COMMANDS.iter())
                .map(|line| {
                    if line.starts_with(' ') || line.is_empty() {
                        Line::from(*line)
                    } else {
                        Line::styled(*line, Style::new().add_modifier(Modifier::BOLD))
                    }
                })
                .collect();
            let scroll = (*scroll).min(lines.len().saturating_sub(usize::from(inner.height)));
            *cursor = Some(Position::new(inner.x, inner.y));
            frame.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), inner);
        }
        Some(Overlay::Finder { input, selected }) => {
            let inner = dialog(frame, "Find a channel, person or conversation", 76, 16);
            let items = app.finder_items(input.text());
            let at = nuschl_client::finder::selected_index(selected.as_ref(), &items);
            let [field, hint, list] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Fill(1),
            ])
            .areas(inner);
            frame.render_widget(
                Paragraph::new(Line::styled(
                    match items.len() {
                        0 if input.is_empty() => {
                            "Type a name. Enter opens, Alt+Enter joins voice.".to_string()
                        }
                        0 => "Nothing matches. Messages are found with Ctrl+F.".to_string(),
                        1 => "1 result. Enter opens, Alt+Enter joins voice.".to_string(),
                        count => format!("{count} results. Enter opens, Alt+Enter joins voice."),
                    },
                    Style::new().add_modifier(Modifier::DIM),
                )),
                hint,
            );
            let texts = items
                .iter()
                .map(|item| {
                    let server = app
                        .client
                        .server_index(&item.server)
                        .map(|index| clean(&app.client.servers[index].name))
                        .unwrap_or_default();
                    let place = match &item.target {
                        _ if item.path.is_empty() => String::new(),
                        nuschl_client::finder::Target::Channel(_) => {
                            format!(" in {}", clean(&item.path))
                        }
                        _ => format!(" {}", clean(&item.path)),
                    };
                    format!(
                        "{}, {}{place}, {server}{}",
                        clean(&item.name),
                        item.target.kind(),
                        unread(item.unread, false)
                    )
                })
                .collect();
            frame.render_widget(
                Paragraph::new(list_lines(texts, at, usize::from(list.height))),
                list,
            );
            let (column, _) = input.cursor_position();
            input_line(frame, field, "Find: ", input.text(), column, cursor);
        }
        Some(Overlay::Search(search)) => {
            let inner = dialog(
                frame,
                "Search messages",
                90,
                area.height.saturating_sub(4).clamp(8, 24),
            );
            let [field, hint, list] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Fill(1),
            ])
            .areas(inner);
            let scope = match search.scope {
                Scope::Conversation => "this conversation",
                Scope::Server => "this server",
                Scope::All => "all servers",
            };
            let state = if let Some(error) = &search.error {
                clean(error)
            } else if search.loading {
                "Searching…".into()
            } else if search.results.is_empty() && !search.input.is_empty() {
                "Nothing found.".into()
            } else {
                format!(
                    "{} result{}{}",
                    search.results.len(),
                    if search.results.len() == 1 { "" } else { "s" },
                    if search.next.is_some() {
                        ", more below"
                    } else {
                        ""
                    }
                )
            };
            frame.render_widget(
                Paragraph::new(Line::styled(
                    format!("In {scope} (Tab changes). {state}. Enter opens the message."),
                    Style::new().add_modifier(Modifier::DIM),
                )),
                hint,
            );
            let texts = search
                .results
                .iter()
                .map(|hit| {
                    format!(
                        "{} {} in {}: {}",
                        app.time(hit.observed_at_ms),
                        clean(&hit.author),
                        clean(&hit.place),
                        clean(&hit.snippet)
                    )
                })
                .collect();
            frame.render_widget(
                Paragraph::new(list_lines(texts, search.selected, usize::from(list.height))),
                list,
            );
            let (column, _) = search.input.cursor_position();
            input_line(
                frame,
                field,
                "Search: ",
                search.input.text(),
                column,
                cursor,
            );
        }
        Some(Overlay::Delete { ours, .. }) => {
            let inner = dialog(frame, "Delete message", 64, 6);
            let detail = if *ours {
                "It is removed from the server's history for everyone. People using stock Mumble keep seeing it."
            } else {
                "You delete it as a moderator; others see that a moderator removed it."
            };
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled(
                        "Delete this message? y: delete, n: keep",
                        Style::new().add_modifier(Modifier::BOLD),
                    ),
                    Line::from(detail),
                ])
                .wrap(ratatui::widgets::Wrap { trim: true }),
                inner,
            );
            *cursor = Some(Position::new(inner.x, inner.y));
        }
        Some(Overlay::Prompt {
            kind,
            server,
            input,
            reason,
        }) => {
            let name = app
                .client
                .servers
                .get(*server)
                .map(|server| clean(&server.name))
                .unwrap_or_default();
            let inner = dialog(frame, &format!("Log in to {name}"), 70, 5);
            let [why, field] =
                Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(inner);
            frame.render_widget(
                Paragraph::new(clean(reason)).wrap(ratatui::widgets::Wrap { trim: true }),
                why,
            );
            let password = kind.needs_password();
            let shown = if password {
                "*".repeat(input.text().chars().count())
            } else {
                input.text().to_string()
            };
            let (column, _) = input.cursor_position();
            let column = if password {
                input.text().chars().count().min(column)
            } else {
                column
            };
            input_line(
                frame,
                field,
                if password || matches!(kind, RejectKind::Other) {
                    "Password: "
                } else {
                    "Name: "
                },
                &shown,
                column,
                cursor,
            );
        }
        Some(Overlay::Links { links, selected }) => {
            let inner = dialog(
                frame,
                "Links: Enter opens, Esc closes",
                90,
                (links.len() as u16 + 2).min(16),
            );
            let texts = links
                .iter()
                .enumerate()
                .map(|(index, link)| format!("{}. {}", index + 1, clean(link)))
                .collect();
            frame.render_widget(
                Paragraph::new(list_lines(texts, *selected, usize::from(inner.height))),
                inner,
            );
            *cursor = Some(Position::new(
                inner.x,
                inner.y + (*selected).min(usize::from(inner.height).saturating_sub(1)) as u16,
            ));
        }
        Some(Overlay::Certificate(index)) => {
            let Some(server) = app.client.servers.get(*index) else {
                return;
            };
            let Some(prompt) = &server.certificate else {
                return;
            };
            let inner = dialog(frame, "Server certificate", 76, 9);
            let mut lines = vec![if prompt.changed {
                Line::styled(
                    format!("The certificate of {} CHANGED.", clean(&server.name)),
                    Style::new().add_modifier(Modifier::BOLD),
                )
            } else {
                Line::styled(
                    format!("First connection to {}.", clean(&server.name)),
                    Style::new().add_modifier(Modifier::BOLD),
                )
            }];
            if !prompt.reason.is_empty() {
                lines.push(Line::from(clean(&prompt.reason)));
            }
            lines.push(Line::from(format!("SHA-256: {}", clean(&prompt.sha256))));
            lines.push(Line::from(
                "Trust it (y) only if it is the server you mean; n disconnects, Esc decides later.",
            ));
            frame.render_widget(
                Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }),
                inner,
            );
            *cursor = Some(Position::new(inner.x, inner.y));
        }
    }
}
