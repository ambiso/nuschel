// SPDX-License-Identifier: EUPL-1.2
//! Selectable message text. GPUI has no selectable text element: [`BlockText`]
//! lays a block's text out like `StyledText`, with the selection as a
//! background highlight under the glyphs, and exposes it to screen readers
//! like `text!`. [`SelectionArea`] wraps a message's blocks and turns presses
//! and drags into carets, and clicks on links into opening them. A selection
//! runs from one message to another: while the button is down, every message
//! on screen offers the caret nearest the pointer and the nearest message's
//! wins, so a drag reaches past the message it started in, past line ends and
//! beyond the timeline's edges (which then scroll). The selection lives in
//! [`Desktop`], so Ctrl+C and the message menu can copy it.
use crate::{
    document::{Document, InlineMath, Links},
    menus::{Menu, MenuTarget},
    state::{Desktop, Status},
    theme::{ACCENT, tint},
};
use gpui_kit::*;
use nuschel_client::timeline::Message;
use nuschel_session::{
    config::MumbleUrl,
    connection::{Command, TextTarget},
};
use std::{cell::Cell, ops::Range, rc::Rc, sync::Arc, time::Duration};

/// A place in a message: a block and a byte offset into its text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Caret {
    pub block: usize,
    pub offset: usize,
}

impl Caret {
    /// Past the last block of a message.
    const END: Caret = Caret {
        block: usize::MAX,
        offset: usize::MAX,
    };
}

/// Where a message is in the timeline: by when it was seen, then by which
/// was made first. Older history arrives after newer messages, so the serial
/// alone is not reading order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Order {
    pub observed_at_ms: i64,
    pub serial: u64,
}

pub fn order(message: &Message) -> Order {
    Order {
        observed_at_ms: message.observed_at_ms,
        serial: message.serial,
    }
}

/// A caret in a message of the timeline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Place {
    message: Order,
    caret: Caret,
}

/// Selected text, from a place in one message to one in the same or another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextSelection {
    /// What the press took in: a caret, or the word or line of a double or
    /// triple click. Dragging extends it and never drops it.
    origin: (Place, Place),
    head: Place,
    /// The button is still down: the head follows the pointer.
    dragging: bool,
}

impl TextSelection {
    /// Start and end, in reading order.
    pub fn range(&self) -> (Place, Place) {
        (self.origin.0.min(self.head), self.origin.1.max(self.head))
    }

    /// The selected part of one message, if any of it is selected.
    pub fn carets(&self, message: Order) -> Option<(Caret, Caret)> {
        let (start, end) = self.range();
        if message < start.message || message > end.message {
            return None;
        }
        let from = if message == start.message {
            start.caret
        } else {
            Caret::default()
        };
        let to = if message == end.message {
            end.caret
        } else {
            Caret::END
        };
        (from < to).then_some((from, to))
    }
}

/// The selected bytes of block `block`, whose text is `len` bytes long, of
/// the part of a message from `start` to `end`.
pub fn block_range((start, end): (Caret, Caret), block: usize, len: usize) -> Option<Range<usize>> {
    if !(start.block..=end.block).contains(&block) {
        return None;
    }
    let from = if block == start.block {
        start.offset
    } else {
        0
    };
    let to = if block == end.block {
        end.offset.min(len)
    } else {
        len
    };
    (from < to).then_some(from..to)
}

/// The best caret for the pointer offered so far in one mouse move: each
/// message offers its nearest, with how far the pointer is from the message.
#[derive(Clone, Copy)]
pub struct Candidate {
    position: Point<Pixels>,
    distance: Pixels,
    place: Place,
}

/// What a message's context menu copies, taken when it opens.
pub struct MessageMenu {
    /// The message, by its serial.
    pub serial: u64,
    pub text: SharedString,
    pub selection: Option<SharedString>,
    pub link: Option<SharedString>,
}

/// The link under the pointer.
pub struct HoveredLink {
    pub message: u64,
    pub block: usize,
    pub link: usize,
    pub url: SharedString,
    /// Where the pointer came onto it, for the tooltip of a link whose text
    /// is not its address.
    pub position: Point<Pixels>,
    pub deceptive: bool,
}

/// Message text state: the selection, the hovered link, the message a context
/// menu is for, the code last copied with a code block's button (shown as a
/// check for a moment), the focus target that takes Ctrl+C while text is
/// selected, and while dragging, the caret offered for the pointer and the
/// timeline scrolling past its edge.
pub struct TextState {
    pub selection: Option<TextSelection>,
    pub hovered: Option<HoveredLink>,
    pub menu: Option<MessageMenu>,
    pub copied: Option<SharedString>,
    pub focus: FocusHandle,
    candidate: Rc<Cell<Option<Candidate>>>,
    scrolling: Option<Task<()>>,
}

impl TextState {
    pub fn new(cx: &mut App) -> Self {
        Self {
            selection: None,
            hovered: None,
            menu: None,
            copied: None,
            // The timeline's: a tab stop, so the keyboard reaches its entries.
            focus: cx.focus_handle().tab_stop(true),
            candidate: Rc::default(),
            scrolling: None,
        }
    }
}

impl Desktop {
    /// A message of the open conversation.
    fn message(&self, serial: u64) -> Option<&Message> {
        let (server, channel) = self.selected_indices()?;
        self.servers[server].channels[channel]
            .timeline
            .messages
            .iter()
            .find(|message| message.serial == serial)
    }

    /// The selected text, when some is selected in the open conversation:
    /// the selected part of each message, one message per line.
    pub fn selected_text(&self) -> Option<String> {
        let selection = self.text.selection?;
        let (server, channel) = self.selected_indices()?;
        let parts: Vec<String> = self.servers[server].channels[channel]
            .timeline
            .messages
            .iter()
            .filter_map(|message| {
                let (start, end) = selection.carets(order(message))?;
                let document = &self.presentation.message(message.serial).document;
                Some(selected_text(document, start, end))
            })
            .filter(|part| !part.is_empty())
            .collect();
        let text = parts.join("\n");
        (!text.is_empty()).then_some(text)
    }

    /// The pointer moved with the button down: the head goes to `place`, and
    /// past the timeline's top or bottom the timeline scrolls on its own.
    fn drag_to(
        &mut self,
        place: Place,
        pointer: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selection) = self
            .text
            .selection
            .as_mut()
            .filter(|selection| selection.dragging)
        else {
            return;
        };
        if selection.head != place {
            selection.head = place;
            cx.notify();
        }
        if self.text.scrolling.is_none() && overshoot(self.scroll.bounds(), pointer.y) != 0. {
            self.text.scrolling = Some(cx.spawn_in(window, async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(16))
                        .await;
                    let Ok(Some(position)) =
                        this.update_in(cx, |this, window, cx| this.scroll_step(window, cx))
                    else {
                        break;
                    };
                    // The text moved under the pointer: move the head as if
                    // the pointer had moved over it.
                    let moved = PlatformInput::MouseMove(MouseMoveEvent {
                        position,
                        pressed_button: Some(MouseButton::Left),
                        modifiers: Modifiers::default(),
                    });
                    if cx
                        .update(|window, cx| window.dispatch_event(moved, cx))
                        .is_err()
                    {
                        break;
                    }
                }
                let _ = this.update(cx, |this, _| this.text.scrolling = None);
            }));
        }
    }

    /// Scroll the timeline toward a pointer dragged past its edge; returns
    /// where the pointer is while it is still there.
    fn scroll_step(&mut self, window: &Window, cx: &mut Context<Self>) -> Option<Point<Pixels>> {
        let dragging = self
            .text
            .selection
            .is_some_and(|selection| selection.dragging);
        let pointer = window.mouse_position();
        let over = overshoot(self.scroll.bounds(), pointer.y);
        if !dragging || over == 0. {
            return None;
        }
        let mut offset = self.scroll.offset();
        let lowest = -f32::from(self.scroll.max_offset().y);
        offset.y = px((f32::from(offset.y) + (over / 2.).clamp(-40., 40.)).clamp(lowest, 0.));
        self.scroll.set_offset(offset);
        cx.notify();
        Some(pointer)
    }

    /// Copy the selection; returns whether there was one.
    pub fn copy_selection(&self, cx: &mut App) -> bool {
        let Some(text) = self.selected_text() else {
            return false;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }

    /// A code block's copy button: the icon shows a check for a moment.
    pub fn copy_code(&mut self, code: SharedString, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(code.to_string()));
        self.text.copied = Some(code.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1500))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.text.copied.as_ref() == Some(&code) {
                    this.text.copied = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// A click on a link: mumble links are Nuschel's own, others go to the system.
    pub fn open_link(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !nuschel_core::allowed_link(url) {
            return;
        }
        if !url
            .get(..9)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("mumble://"))
        {
            cx.open_url(url);
            return;
        }
        let mut url = match MumbleUrl::parse(url) {
            Ok(url) => url,
            Err(error) => {
                self.error = Some(format!("Cannot open {url}: {error}"));
                return;
            }
        };
        let saved = self.servers.iter().position(|server| {
            server.bookmark.host == url.host && server.bookmark.port == url.port
        });
        match saved.map(|index| (index, self.servers[index].status)) {
            // Already there: go to the channel without connecting again.
            Some((index, Status::Connected)) => {
                let server = &self.servers[index];
                let channel = url.channel.iter().try_fold(0, |parent, name| {
                    server
                        .channels
                        .iter()
                        .find(|channel| {
                            channel.parent == Some(parent)
                                && channel.name == *name
                                && matches!(channel.target, TextTarget::Channel(_))
                        })
                        .map(|channel| channel.id)
                });
                match channel {
                    Some(channel) => {
                        self.browse_channel(index, channel, window, cx);
                        self.run(index, Command::Join(channel));
                    }
                    None => {
                        self.servers[index].notice =
                            Some(format!("Channel /{} was not found.", url.channel.join("/")))
                    }
                }
            }
            Some((index, Status::Connecting)) => {
                self.servers[index].pending_channel = url.channel;
                self.browse_server(index, window, cx);
            }
            Some(_) => self.open_url(url, window, cx),
            // A server that is not saved opens the form to confirm, rather than
            // being saved and connected to from someone's message.
            None => {
                url.username = None;
                self.open_url(url, window, cx);
            }
        }
    }

    /// Right click on a message: Copy link (the one under the pointer), Copy
    /// (the selection in it) and Copy message.
    pub fn open_message_menu(&mut self, message: u64, position: Point<Pixels>) {
        let Some((server, _)) = self.selected_indices() else {
            return;
        };
        let Some(view) = self.message(message) else {
            return;
        };
        let text = self
            .presentation
            .message(message)
            .document
            .plain_text()
            .into();
        let selection = self
            .text
            .selection
            .filter(|selection| selection.carets(order(view)).is_some())
            .and_then(|_| self.selected_text())
            .map(SharedString::from);
        let link = self
            .text
            .hovered
            .as_ref()
            .filter(|hovered| hovered.message == message)
            .map(|hovered| hovered.url.clone());
        // A moderator's Delete depends on Write here (editing.rs); it
        // appears once the server has said.
        if let Some((_, channel)) = self.selected_indices()
            && let TextTarget::Channel(id) = self.servers[server].channels[channel].target
            && self.servers[server].status == Status::Connected
            && !self.servers[server].permissions.contains_key(&id)
        {
            let _ = self
                .backend
                .command(&self.servers[server].id, Command::RequestPermissions(id));
        }
        self.text.menu = Some(MessageMenu {
            serial: message,
            text,
            selection,
            link,
        });
        self.menu = Some(Menu {
            server,
            target: MenuTarget::Message,
            position,
        });
    }
}

/// The text between two carets: blocks joined by newlines (formulas within
/// a line stay in it), images left out.
pub fn selected_text(document: &Document, start: Caret, end: Caret) -> String {
    let parts = document
        .blocks
        .iter()
        .enumerate()
        .take(end.block.saturating_add(1))
        .skip(start.block)
        .filter_map(|(index, block)| {
            let text = block.text()?;
            let from = if index == start.block {
                start.offset
            } else {
                0
            };
            let to = if index == end.block {
                end.offset
            } else {
                text.len()
            };
            Some((index, std::borrow::Cow::from(text.get(from..to)?)))
        });
    document.join(parts)
}

fn floor_boundary(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// The word, run of spaces or single other character at `offset`.
fn word_at(text: &str, offset: usize) -> Range<usize> {
    let offset = floor_boundary(text, offset);
    let class = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            1
        } else if c.is_whitespace() {
            2
        } else {
            3
        }
    };
    // The character after the caret, or the one before it at a line's end.
    let (at, c) = match text[offset..].chars().next().filter(|&c| c != '\n') {
        Some(c) => (offset, c),
        None => match text[..offset].char_indices().next_back() {
            Some((at, c)) if c != '\n' => (at, c),
            _ => return offset..offset,
        },
    };
    let kind = class(c);
    if kind == 3 {
        return at..at + c.len_utf8();
    }
    let same = |c: char| c != '\n' && class(c) == kind;
    let start = text[..at]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| same(c))
        .last()
        .map_or(at, |(index, _)| index);
    let end = text[at..]
        .char_indices()
        .find(|&(_, c)| !same(c))
        .map_or(text.len(), |(index, _)| at + index);
    start..end
}

/// The line, between newlines, at `offset`.
fn line_at(text: &str, offset: usize) -> Range<usize> {
    let offset = floor_boundary(text, offset);
    let start = text[..offset].rfind('\n').map_or(0, |index| index + 1);
    let end = text[offset..]
        .find('\n')
        .map_or(text.len(), |index| offset + index);
    start..end
}

/// `highlights` (sorted, disjoint) over a text of `len` bytes, split where
/// `range` begins and ends, with `apply` changing the style inside it.
fn overlay(
    highlights: &[(Range<usize>, HighlightStyle)],
    range: Range<usize>,
    len: usize,
    apply: impl Fn(&mut HighlightStyle),
) -> Vec<(Range<usize>, HighlightStyle)> {
    let mut covered = Vec::with_capacity(2 * highlights.len() + 1);
    let mut end = 0;
    for (part, style) in highlights {
        if end < part.start {
            covered.push((end..part.start, HighlightStyle::default()));
        }
        covered.push((part.clone(), *style));
        end = part.end;
    }
    if end < len {
        covered.push((end..len, HighlightStyle::default()));
    }
    let mut split = Vec::with_capacity(covered.len() + 2);
    for (part, style) in covered {
        let inside = part.start.max(range.start)..part.end.min(range.end);
        if inside.is_empty() {
            split.push((part, style));
            continue;
        }
        if part.start < inside.start {
            split.push((part.start..inside.start, style));
        }
        let mut changed = style;
        apply(&mut changed);
        split.push((inside.clone(), changed));
        if inside.end < part.end {
            split.push((inside.end..part.end, style));
        }
    }
    split
}

fn select(style: &mut HighlightStyle) {
    style.background_color = Some(tint(ACCENT, 0x70).into());
}

fn underline(style: &mut HighlightStyle) {
    style.underline = Some(UnderlineStyle {
        thickness: px(1.),
        ..Default::default()
    });
}

/// A block's text with its highlights, the selected part and the hovered
/// link. With an id it is a label for screen readers, as `text!` makes, and
/// its links and formulas are links and formulas in it.
///
/// Inline formulas are placed when the text is laid out, once the font and
/// the scale are known (`crate::math::place`): the text laid out then has a
/// stand-in for each rendered formula, which is painted over it. Offsets
/// outside this element (carets, links, the selection) stay those of the
/// block's own text; [`BlockLayout`] maps between them.
pub struct BlockText {
    id: Option<ElementId>,
    text: SharedString,
    links: Option<Links>,
    styled: StyledText,
    layout: BlockLayout,
    math: Option<MathText>,
}

/// What laying out inline formulas needs: the formulas, the styles over the
/// block's own text, and where they were placed.
struct MathText {
    formulas: Arc<[InlineMath]>,
    styles: Vec<(Range<usize>, HighlightStyle)>,
    monospace: Vec<Range<usize>>,
    placed: Vec<crate::math::Placed>,
    line_height: Option<Pixels>,
    baseline: Pixels,
    em: Pixels,
}

impl BlockText {
    pub fn new(
        id: Option<ElementId>,
        text: SharedString,
        highlights: &[(Range<usize>, HighlightStyle)],
        monospace: &[Range<usize>],
        selected: Option<Range<usize>>,
        hovered: Option<Range<usize>>,
        links: Option<Links>,
    ) -> Self {
        let math = Arc::from([]);
        Self::with_math(
            id, text, highlights, monospace, selected, hovered, links, math,
        )
    }

    /// A text block with inline formulas.
    #[allow(clippy::too_many_arguments)]
    pub fn with_math(
        id: Option<ElementId>,
        text: SharedString,
        highlights: &[(Range<usize>, HighlightStyle)],
        monospace: &[Range<usize>],
        selected: Option<Range<usize>>,
        hovered: Option<Range<usize>>,
        links: Option<Links>,
        formulas: Arc<[InlineMath]>,
    ) -> Self {
        let mut styles: Option<Vec<_>> = None;
        if let Some(range) = hovered {
            styles = Some(overlay(highlights, range, text.len(), underline));
        }
        if let Some(range) = selected {
            let base = styles.as_deref().unwrap_or(highlights);
            styles = Some(overlay(base, range, text.len(), select));
        }
        let styled = styled_text(
            text.clone(),
            styles.as_deref().unwrap_or(highlights),
            monospace,
        );
        let layout = BlockLayout::default();
        layout.0.borrow_mut().layout = styled.layout().clone();
        // Formulas are placed at layout time, from the styles kept here.
        let math = (!formulas.is_empty()).then(|| MathText {
            formulas,
            styles: styles.unwrap_or_else(|| highlights.to_vec()),
            monospace: monospace.to_vec(),
            placed: Vec::new(),
            line_height: None,
            baseline: px(0.),
            em: px(0.),
        });
        Self {
            id,
            text,
            links,
            styled,
            layout,
            math,
        }
    }

    /// Maps positions to offsets once the text is laid out.
    pub fn layout(&self) -> BlockLayout {
        self.layout.clone()
    }

    /// With formulas, the text with their stand-ins, and the styles moved
    /// to it; a formula that failed shows as code with a wavy underline, one
    /// on its way in a faint colour.
    fn place_math(&mut self, window: &mut Window, cx: &mut App) {
        let Some(math) = self.math.as_mut().filter(|math| !math.formulas.is_empty()) else {
            self.math = None;
            return;
        };
        let placement = crate::math::place(&self.text, &math.formulas, window, cx);
        let mut styles = std::mem::take(&mut math.styles);
        let mut monospace = std::mem::take(&mut math.monospace);
        for range in &placement.failed {
            styles = overlay(&styles, range.clone(), self.text.len(), |style| {
                style.background_color = Some(crate::theme::color(crate::theme::CODE_SPAN).into());
                style.underline = Some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(crate::theme::color(crate::theme::WARNING).into()),
                    wavy: true,
                });
            });
            monospace.push(range.clone());
        }
        for range in &placement.pending {
            styles = overlay(&styles, range.clone(), self.text.len(), |style| {
                style.color = Some(crate::theme::color(crate::theme::TEXT_FAINT).into());
            });
        }
        monospace.sort_by_key(|range| range.start);
        let map: Vec<(Range<usize>, Range<usize>)> = placement
            .placed
            .iter()
            .map(|placed| (placed.source.clone(), placed.shown.clone()))
            .collect();
        if !map.is_empty() || !placement.failed.is_empty() || !placement.pending.is_empty() {
            let shown = |range: &Range<usize>| {
                to_shown(&map, range.start, false)..to_shown(&map, range.end, true)
            };
            let styles: Vec<_> = styles
                .iter()
                .map(|(range, style)| (shown(range), *style))
                .filter(|(range, _)| !range.is_empty())
                .collect();
            let monospace: Vec<_> = monospace.iter().map(shown).collect();
            self.styled = styled_text(placement.text.into(), &styles, &monospace);
            let mut layout = self.layout.0.borrow_mut();
            layout.layout = self.styled.layout().clone();
            layout.map = map;
        }
        self.math = Some(MathText {
            formulas: math.formulas.clone(),
            styles: Vec::new(),
            monospace: Vec::new(),
            placed: placement.placed,
            line_height: placement.line_height,
            baseline: placement.baseline,
            em: placement.em,
        });
    }
}

fn styled_text(
    text: SharedString,
    styles: &[(Range<usize>, HighlightStyle)],
    monospace: &[Range<usize>],
) -> StyledText {
    let styled = StyledText::new(text);
    let styled = if styles.is_empty() {
        styled
    } else {
        styled.with_highlights(styles.iter().cloned())
    };
    if monospace.is_empty() {
        styled
    } else {
        let family = crate::theme::mono_font();
        styled.with_font_family_overrides(
            monospace
                .iter()
                .map(|range| (range.clone(), family.clone())),
        )
    }
}

/// An offset in a block's text as an offset in its laid-out text: in a
/// formula, its stand-in's start or (`end`) end.
fn to_shown(map: &[(Range<usize>, Range<usize>)], offset: usize, end: bool) -> usize {
    let mut shift = 0_isize;
    for (source, shown) in map {
        if offset <= source.start {
            break;
        }
        if offset < source.end {
            return if end { shown.end } else { shown.start };
        }
        shift = shown.end as isize - source.end as isize;
    }
    offset.saturating_add_signed(shift)
}

/// An offset in the laid-out text as one in the block's text: in a
/// formula's stand-in, the nearer end of the formula.
fn to_source(map: &[(Range<usize>, Range<usize>)], offset: usize) -> usize {
    let mut shift = 0_isize;
    for (source, shown) in map {
        if offset <= shown.start {
            break;
        }
        if offset < shown.end {
            return if offset - shown.start < shown.end - offset {
                source.start
            } else {
                source.end
            };
        }
        shift = source.end as isize - shown.end as isize;
    }
    offset.saturating_add_signed(shift)
}

/// A block's text as laid out, with offsets in the block's own text.
#[derive(Clone, Default)]
pub struct BlockLayout(Rc<std::cell::RefCell<Laid>>);

#[derive(Default)]
struct Laid {
    layout: TextLayout,
    /// Each placed formula: its text as written, and its stand-in.
    map: Vec<(Range<usize>, Range<usize>)>,
}

impl BlockLayout {
    pub fn bounds(&self) -> Bounds<Pixels> {
        self.0.borrow().layout.bounds()
    }

    pub fn index_for_position(&self, position: Point<Pixels>) -> Result<usize, usize> {
        let laid = self.0.borrow();
        let map = |offset| to_source(&laid.map, offset);
        laid.layout
            .index_for_position(position)
            .map(map)
            .map_err(map)
    }
}

impl IntoElement for BlockText {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for BlockText {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        self.id.clone()
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn a11y_role(&self) -> Option<Role> {
        self.id.as_ref().map(|_| Role::Label)
    }

    fn write_a11y_info(&self, node: &mut accesskit::Node) {
        node.set_value(self.text.to_string());
    }

    fn a11y_synthetic_children(&mut self, _: &mut (), builder: &mut A11ySubtreeBuilder) {
        let mut index = 0;
        for (range, url) in self.links.iter().flat_map(|links| links.iter()) {
            let mut node = accesskit::Node::new(Role::Link);
            node.set_label(&self.text[range.clone()]);
            node.set_url(url.to_string());
            builder.push_child(builder.synthetic_node_id(index), node);
            index += 1;
        }
        // A formula's name is its source, as written.
        for formula in self.math.iter().flat_map(|math| math.formulas.iter()) {
            let Some(source) = self.text.get(formula.range.clone()) else {
                continue;
            };
            let mut node = accesskit::Node::new(Role::Math);
            node.set_label(source);
            builder.push_child(builder.synthetic_node_id(index), node);
            index += 1;
        }
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        self.place_math(window, cx);
        let line_height = self.math.as_ref().and_then(|math| math.line_height);
        let refinement = line_height.map(|height| TextStyleRefinement {
            line_height: Some(height.into()),
            ..Default::default()
        });
        window.with_text_style(refinement, |window| {
            self.styled.request_layout(None, inspector_id, window, cx)
        })
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.styled
            .prepaint(None, inspector_id, bounds, &mut (), window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.styled
            .paint(None, inspector_id, bounds, &mut (), &mut (), window, cx);
        let Some(math) = &self.math else {
            return;
        };
        let color = window.text_style().color;
        let layout = self.styled.layout();
        for placed in &math.placed {
            let Some(line) = layout.position_for_index(placed.shown.start) else {
                continue;
            };
            let (width, height, baseline) = placed.formula.metrics(math.em);
            let origin = point(
                line.x + (placed.width - width) / 2.,
                line.y + math.baseline - baseline,
            );
            if let Some(image) = placed.formula.image(color) {
                let bounds = Bounds::new(origin, size(width, height));
                let _ = window.paint_image(bounds, bounds, Corners::default(), image, 0, false);
            }
        }
    }
}

/// A text block of a message as laid out this frame.
pub struct TextBox {
    pub block: usize,
    pub text: SharedString,
    pub layout: BlockLayout,
    pub links: Option<Links>,
}

/// A message's text blocks as laid out this frame.
pub type Texts = Vec<TextBox>;

/// The offset in laid-out text for a position, the nearest one beside glyphs.
fn offset_at(layout: &BlockLayout, position: Point<Pixels>) -> usize {
    // Half a pixel down: exactly on a line's bottom edge GPUI answers with the
    // start of that line rather than a place in it.
    let probe = point(position.x, position.y + px(0.5));
    layout.index_for_position(probe).unwrap_or_else(|near| near)
}

/// The caret nearest to a position among a message's texts.
fn caret_near(texts: &[TextBox], position: Point<Pixels>) -> Option<Caret> {
    let laid_out = texts
        .iter()
        .map(|text| (text.block, text.text.len(), text.layout.bounds()));
    nearest_caret(laid_out, position.y, |index| {
        offset_at(&texts[index].layout, position)
    })
}

/// The caret for height `y` among texts laid out top to bottom, given as
/// (block, length, bounds): in the text at that height (`index` finds the
/// offset in it, the nearest one beside the glyphs), at the start of the next
/// text when `y` falls between texts or above them, or at the end of the last
/// text below them.
fn nearest_caret(
    texts: impl IntoIterator<Item = (usize, usize, Bounds<Pixels>)>,
    y: Pixels,
    index: impl Fn(usize) -> usize,
) -> Option<Caret> {
    let mut last = None;
    for (at, (block, len, bounds)) in texts.into_iter().enumerate() {
        if y < bounds.top() {
            return Some(Caret { block, offset: 0 });
        }
        if y <= bounds.bottom() {
            let offset = index(at).min(len);
            return Some(Caret { block, offset });
        }
        last = Some(Caret { block, offset: len });
    }
    last
}

/// How far `y` is above (positive) or below (negative) `bounds`; 0 inside.
fn overshoot(bounds: Bounds<Pixels>, y: Pixels) -> f32 {
    if y < bounds.top() {
        f32::from(bounds.top() - y)
    } else if y > bounds.bottom() {
        f32::from(bounds.bottom() - y)
    } else {
        0.
    }
}

/// The text (by index) and link under the pointer.
fn link_at(
    texts: &[TextBox],
    hitboxes: &[Hitbox],
    position: Point<Pixels>,
    window: &Window,
) -> Option<(usize, usize)> {
    let index = hitboxes
        .iter()
        .position(|hitbox| hitbox.is_hovered(window))?;
    let text = &texts[index];
    let offset = text.layout.index_for_position(position).ok()?;
    let link = text
        .links
        .as_ref()?
        .iter()
        .position(|(range, _)| range.contains(&offset))?;
    Some((index, link))
}

/// Whether a link's text names its address ("example.com/" for
/// "https://example.com" does); otherwise its address shows on hover.
fn names_target(text: &str, url: &str) -> bool {
    let bare = |value: &str| {
        let value = value.trim().to_lowercase();
        value
            .strip_prefix("https://")
            .or_else(|| value.strip_prefix("http://"))
            .unwrap_or(&value)
            .trim_end_matches('/')
            .to_owned()
    };
    bare(text) == bare(url)
}

/// While text is selected, any press clears the selection (one on text starts
/// another, see [`SelectionArea`]), and releasing the button ends a drag. With
/// the timeline, not its messages: those holding the selection's ends may be
/// scrolled out of what is laid out.
pub fn timeline_handlers(
    desktop: WeakEntity<Desktop>,
    selection: TextSelection,
) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |_, _, window, _| {
            let clear = desktop.clone();
            window.on_mouse_event(move |event: &MouseDownEvent, phase, _, cx| {
                if phase == DispatchPhase::Capture && event.button == MouseButton::Left {
                    let _ = clear.update(cx, |this, cx| {
                        if this.text.selection.take().is_some() {
                            cx.notify();
                        }
                    });
                }
            });
            if !selection.dragging {
                return;
            }
            // The drag ends; what is selected is also the primary selection,
            // for middle-click paste.
            window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                    return;
                }
                let _ = desktop.update(cx, |this, cx| {
                    let Some(selection) = this
                        .text
                        .selection
                        .as_mut()
                        .filter(|selection| selection.dragging)
                    else {
                        return;
                    };
                    selection.dragging = false;
                    this.text.scrolling = None;
                    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                    if let Some(text) = this.selected_text() {
                        cx.write_to_primary(ClipboardItem::new_string(text));
                    }
                    cx.notify();
                });
            });
        },
    )
    .absolute()
    .size_0()
}

/// Wraps a message's blocks: a press on its text starts a selection (a double
/// click takes a word, a triple click a line), dragging extends it, into
/// other messages too, and any other press clears it (see
/// [`timeline_handlers`]). A click on a link without dragging opens it.
pub struct SelectionArea {
    desktop: WeakEntity<Desktop>,
    message: Order,
    /// Whether the selection's button is down.
    dragging: bool,
    candidate: Rc<Cell<Option<Candidate>>>,
    /// The block and link under the pointer, if in this message.
    hovered: Option<(usize, usize)>,
    texts: Rc<[TextBox]>,
    child: AnyElement,
}

impl SelectionArea {
    pub fn new(
        desktop: WeakEntity<Desktop>,
        state: &TextState,
        message: Order,
        hovered: Option<(usize, usize)>,
        texts: Texts,
        child: impl IntoElement,
    ) -> Self {
        let selection = state.selection.as_ref();
        Self {
            desktop,
            message,
            dragging: selection.is_some_and(|selection| selection.dragging),
            candidate: state.candidate.clone(),
            hovered,
            texts: texts.into(),
            child: child.into_any_element(),
        }
    }

    /// Follow the link under the pointer: underline it, change the cursor and
    /// show where a link that does not say so leads.
    fn track_links(&self, boxes: &Rc<[Hitbox]>, window: &mut Window) {
        let (desktop, texts, hitboxes) = (self.desktop.clone(), self.texts.clone(), boxes.clone());
        let (message, shown) = (self.message.serial, self.hovered);
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let found = link_at(&texts, &hitboxes, event.position, window);
            let block = |(index, link): (usize, usize)| (texts[index].block, link);
            if found.map(block) == shown {
                return;
            }
            let hovered = found.and_then(|(index, link)| {
                let text = &texts[index];
                let (range, url) = text.links.as_ref()?.get(link)?;
                Some(HoveredLink {
                    message,
                    block: text.block,
                    link,
                    url: url.clone(),
                    position: event.position,
                    deceptive: !names_target(&text.text[range.clone()], url),
                })
            });
            let _ = desktop.update(cx, |this, cx| {
                let mine = |hovered: &HoveredLink| hovered.message == message;
                if hovered.is_some() || this.text.hovered.as_ref().is_some_and(mine) {
                    this.text.hovered = hovered;
                    cx.notify();
                }
            });
        });
        let (desktop, texts, hitboxes) = (self.desktop.clone(), self.texts.clone(), boxes.clone());
        let message = self.message;
        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                return;
            }
            let Some((index, link)) = link_at(&texts, &hitboxes, event.position, window) else {
                return;
            };
            let text = &texts[index];
            let Some((range, url)) = text.links.as_ref().and_then(|links| links.get(link)) else {
                return;
            };
            let _ = desktop.update(cx, |this, cx| {
                // Only a click: pressed on this link and released unmoved.
                let pressed = this.text.selection.is_some_and(|selection| {
                    let (start, end) = selection.range();
                    start == end
                        && start.message == message
                        && start.caret.block == text.block
                        && range.contains(&start.caret.offset)
                });
                if pressed {
                    // The link may take the conversation away from under the pointer.
                    this.text.hovered = None;
                    this.open_link(url, window, cx);
                    cx.notify();
                }
            });
        });
    }
}

impl IntoElement for SelectionArea {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for SelectionArea {
    type RequestLayoutState = ();
    type PrepaintState = Rc<[Hitbox]>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Rc<[Hitbox]> {
        self.child.prepaint(window, cx);
        // Above the blocks, inside their clip; they let presses through.
        self.texts
            .iter()
            .map(|text| window.insert_hitbox(text.layout.bounds(), HitboxBehavior::Normal))
            .collect()
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        hitboxes: &mut Rc<[Hitbox]>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
        for (text, hitbox) in self.texts.iter().zip(hitboxes.iter()) {
            let on_link = self.hovered.is_some_and(|(block, _)| block == text.block);
            let cursor = if on_link {
                CursorStyle::PointingHand
            } else {
                CursorStyle::IBeam
            };
            window.set_cursor_style(cursor, hitbox);
        }
        if self.texts.iter().any(|text| text.links.is_some()) {
            self.track_links(hitboxes, window);
        }
        let message = self.message;
        if self.dragging {
            // This message offers its caret nearest the pointer; the nearest
            // message's wins, and the head moves there once per move.
            let (texts, candidate) = (self.texts.clone(), self.candidate.clone());
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, _| {
                if phase != DispatchPhase::Capture
                    || event.pressed_button != Some(MouseButton::Left)
                {
                    return;
                }
                let distance = px(overshoot(bounds, event.position.y).abs());
                let better = candidate
                    .get()
                    .is_none_or(|best| best.position != event.position || distance < best.distance);
                if better && let Some(caret) = caret_near(&texts, event.position) {
                    candidate.set(Some(Candidate {
                        position: event.position,
                        distance,
                        place: Place { message, caret },
                    }));
                }
            });
            let (desktop, candidate) = (self.desktop.clone(), self.candidate.clone());
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                let Some(best) = candidate
                    .take()
                    .filter(|best| best.position == event.position)
                else {
                    return;
                };
                let _ = desktop.update(cx, |this, cx| {
                    this.drag_to(best.place, event.position, window, cx)
                });
            });
        }
        let (desktop, texts, hitboxes) =
            (self.desktop.clone(), self.texts.clone(), hitboxes.clone());
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                return;
            }
            let Some(text) = hitboxes
                .iter()
                .position(|hitbox| hitbox.is_hovered(window))
                .map(|index| &texts[index])
            else {
                return;
            };
            let offset = offset_at(&text.layout, event.position).min(text.text.len());
            let taken = match event.click_count {
                0 | 1 => offset..offset,
                2 => word_at(&text.text, offset),
                _ => line_at(&text.text, offset),
            };
            let caret = |offset| Caret {
                block: text.block,
                offset,
            };
            let place = |offset| Place {
                message,
                caret: caret(offset),
            };
            let origin = (place(taken.start), place(taken.end));
            let _ = desktop.update(cx, |this, cx| {
                this.text.selection = Some(TextSelection {
                    origin,
                    head: origin.1,
                    dragging: true,
                });
                // Like a page in a browser, the conversation takes the keyboard
                // (Ctrl+C) from the composer.
                window.focus(&this.text.focus, cx);
                cx.notify();
            });
            cx.stop_propagation();
        });
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: GPUI's glob would replace the standard `#[test]`.
    use super::{
        Caret, Order, Place, TextSelection, block_range, line_at, names_target, nearest_caret,
        overlay, overshoot, select, selected_text, word_at,
    };
    use crate::document::{Block, Document, Kind};
    use gpui_kit::{Bounds, point, px};
    use gpui_kit::{HighlightStyle, rgb};

    #[test]
    fn selections_extend_what_the_press_took_in() {
        let message = Order {
            observed_at_ms: 10,
            serial: 7,
        };
        let place = |block, offset| Place {
            message,
            caret: Caret { block, offset },
        };
        let word = (place(1, 4), place(1, 9));
        let selection = |head| TextSelection {
            origin: word,
            head,
            dragging: false,
        };
        assert_eq!(selection(place(1, 9)).range(), word);
        assert_eq!(selection(place(1, 6)).range(), word);
        assert_eq!(selection(place(0, 2)).range(), (place(0, 2), place(1, 9)));
        assert_eq!(selection(place(3, 1)).range(), (place(1, 4), place(3, 1)));
        let carets = selection(place(3, 1)).carets(message).unwrap();
        assert_eq!(block_range(carets, 0, 10), None);
        assert_eq!(block_range(carets, 1, 10), Some(4..10));
        assert_eq!(block_range(carets, 2, 5), Some(0..5));
        assert_eq!(block_range(carets, 3, 5), Some(0..1));
        let carets = selection(place(1, 4)).carets(message).unwrap();
        assert_eq!(block_range(carets, 1, 10), Some(4..9));
        let click = TextSelection {
            origin: (place(1, 4), place(1, 4)),
            head: place(1, 4),
            dragging: true,
        };
        assert_eq!(click.carets(message), None);
    }

    #[test]
    fn a_drag_selects_across_messages_in_timeline_order() {
        // Older history loaded later has newer serials but earlier times.
        let (older, first, second, third) = (
            Order {
                observed_at_ms: 1,
                serial: 90,
            },
            Order {
                observed_at_ms: 5,
                serial: 3,
            },
            Order {
                observed_at_ms: 5,
                serial: 4,
            },
            Order {
                observed_at_ms: 9,
                serial: 5,
            },
        );
        assert!(older < first && first < second && second < third);
        let place = |message, block, offset| Place {
            message,
            caret: Caret { block, offset },
        };
        // Pressed in the second line of `first`, dragged up into `older`.
        let pressed = place(first, 0, 6);
        let selection = TextSelection {
            origin: (pressed, pressed),
            head: place(older, 1, 2),
            dragging: true,
        };
        assert_eq!(
            selection.carets(older),
            Some((
                Caret {
                    block: 1,
                    offset: 2
                },
                Caret::END
            ))
        );
        assert_eq!(
            selection.carets(first),
            Some((
                Caret::default(),
                Caret {
                    block: 0,
                    offset: 6
                }
            ))
        );
        assert_eq!(selection.carets(second), None);
        // Down into `third`: everything of `second` in between.
        let selection = TextSelection {
            head: place(third, 0, 3),
            ..selection
        };
        assert_eq!(selection.carets(older), None);
        assert_eq!(
            selection.carets(first),
            Some((
                Caret {
                    block: 0,
                    offset: 6
                },
                Caret::END
            ))
        );
        assert_eq!(
            selection.carets(second),
            Some((Caret::default(), Caret::END))
        );
        let all = selection.carets(second).unwrap();
        assert_eq!(block_range(all, 0, 8), Some(0..8));
        assert_eq!(block_range(all, 4, 2), Some(0..2));
    }

    #[test]
    fn positions_map_to_the_nearest_caret() {
        // Two texts, block 0 at 100–148 (two lines), block 2 at 160–184.
        let bounds = |top: f32, bottom: f32| {
            Bounds::from_corners(point(px(0.), px(top)), point(px(300.), px(bottom)))
        };
        let texts = [(0, 30, bounds(100., 148.)), (2, 12, bounds(160., 184.))];
        // What GPUI finds in the text the height falls in: line 2 of block 0
        // starts at 15; beside the glyphs it answers the line's end.
        let index = |at: usize| if at == 0 { 20 } else { 99 };
        let caret = |y: f32| nearest_caret(texts, px(y), index);
        let at = |block, offset| Some(Caret { block, offset });
        assert_eq!(caret(40.), at(0, 0), "above: the start");
        assert_eq!(caret(130.), at(0, 20), "within, on the second line");
        assert_eq!(caret(148.), at(0, 20), "on the bottom edge, still in it");
        assert_eq!(caret(152.), at(2, 0), "between texts: the next one's start");
        assert_eq!(caret(170.), at(2, 12), "an offset past the end is clamped");
        assert_eq!(caret(400.), at(2, 12), "below: the end of the last");
        assert_eq!(nearest_caret([], px(1.), index), None);
        // How far the pointer is past a message (or the timeline), and which way.
        assert_eq!(overshoot(bounds(100., 148.), px(90.)), 10.);
        assert_eq!(overshoot(bounds(100., 148.), px(120.)), 0.);
        assert_eq!(overshoot(bounds(100., 148.), px(150.)), -2.);
    }

    #[test]
    fn copies_text_and_code_across_blocks_but_not_images() {
        let image = Block::Image {
            image: crate::document::render_image(1, 1, "AAAAAA==").unwrap(),
            width: 1,
            height: 1,
            source: None,
        };
        let document = Document {
            blocks: vec![
                Block::Text {
                    text: "Hello there".into(),
                    highlights: Vec::new().into(),
                    links: Vec::new().into(),
                    code: Vec::new().into(),
                    math: Vec::new().into(),
                    kind: Kind::Paragraph,
                },
                image,
                Block::Code {
                    language: "rust".into(),
                    text: "fn main() {}".into(),
                    highlights: Vec::new().into(),
                },
                Block::Text {
                    text: "Bye".into(),
                    highlights: Vec::new().into(),
                    links: Vec::new().into(),
                    code: Vec::new().into(),
                    math: Vec::new().into(),
                    kind: Kind::Paragraph,
                },
            ],
        };
        let caret = |block, offset| Caret { block, offset };
        assert_eq!(selected_text(&document, caret(0, 6), caret(0, 11)), "there");
        assert_eq!(
            selected_text(&document, caret(0, 6), caret(3, 2)),
            "there\nfn main() {}\nBy"
        );
        // Ending at the start of a block adds no empty line.
        assert_eq!(
            selected_text(&document, caret(2, 3), caret(3, 0)),
            "main() {}"
        );
    }

    #[test]
    fn double_and_triple_clicks_take_words_and_lines() {
        let text = "let größe = 42;\n  next_line()";
        assert_eq!(&text[word_at(text, 5)], "größe");
        assert_eq!(&text[word_at(text, 4)], "größe");
        assert_eq!(&text[word_at(text, 3)], " ");
        assert_eq!(&text[word_at(text, 12)], "=");
        // At a line's end, the word before the caret.
        assert_eq!(&text[word_at(text, 17)], ";");
        assert_eq!(&text[word_at(text, 20)], "next_line");
        assert_eq!(word_at("", 0), 0..0);
        assert_eq!(&text[line_at(text, 3)], "let größe = 42;");
        assert_eq!(&text[line_at(text, 19)], "  next_line()");
    }

    #[test]
    fn the_selection_splits_highlights_and_covers_plain_gaps() {
        let red = HighlightStyle {
            color: Some(rgb(0xff0000).into()),
            ..Default::default()
        };
        let split = overlay(&[(2..6, red)], 4..8, 10, select);
        let ranges: Vec<_> = split.iter().map(|(range, _)| range.clone()).collect();
        assert_eq!(ranges, [0..2, 2..4, 4..6, 6..8, 8..10]);
        let selected: Vec<_> = split
            .iter()
            .map(|(_, style)| style.background_color.is_some())
            .collect();
        assert_eq!(selected, [false, false, true, true, false]);
        // The selected part keeps its syntax colour.
        assert_eq!(split[2].1.color, red.color);
        assert_eq!(split[3].1.color, None);
    }

    #[test]
    fn links_whose_text_is_not_their_address_are_flagged() {
        assert!(names_target("https://example.com/", "https://example.com"));
        assert!(names_target("example.com", "https://Example.com/"));
        assert!(names_target("www.example.com", "http://www.example.com"));
        assert!(!names_target("example.com", "https://evil.example"));
        assert!(!names_target("the docs", "https://docs.example"));
    }

    #[test]
    fn offsets_map_between_the_text_and_its_formulas_stand_ins() {
        use super::{to_shown, to_source};
        // "a $x^2$ b $y$": formulas at 2..7 and 10..13 stand in as 4 and 2 bytes.
        let map = [(2..7, 2..6), (10..13, 9..11)];
        assert_eq!(to_shown(&map, 0, false), 0);
        assert_eq!(to_shown(&map, 2, false), 2);
        assert_eq!(to_shown(&map, 4, false), 2, "inside: the stand-in's start");
        assert_eq!(to_shown(&map, 4, true), 6, "inside: or its end");
        assert_eq!(to_shown(&map, 7, false), 6);
        assert_eq!(to_shown(&map, 9, false), 8);
        assert_eq!(to_shown(&map, 13, false), 11);
        assert_eq!(to_shown(&map, 14, false), 12);
        assert_eq!(to_source(&map, 2), 2);
        assert_eq!(to_source(&map, 3), 2, "nearer the formula's start");
        assert_eq!(to_source(&map, 5), 7, "nearer its end");
        assert_eq!(to_source(&map, 6), 7);
        assert_eq!(to_source(&map, 8), 9);
        assert_eq!(to_source(&map, 12), 14);
        // Every offset maps back to where it came from, outside formulas.
        for offset in [0, 1, 2, 7, 8, 9, 10, 13, 14] {
            assert_eq!(to_source(&map, to_shown(&map, offset, false)), offset);
        }
    }
}
