// SPDX-License-Identifier: EUPL-1.2
//! Selectable message text. GPUI has no selectable text element: [`BlockText`]
//! lays a block's text out like `StyledText`, with the selection as a
//! background highlight under the glyphs, and exposes it to screen readers
//! like `text!`. [`SelectionArea`] wraps a message's blocks and turns presses
//! and drags into carets across them. The selection lives in [`Desktop`], so
//! Ctrl+C and the message menu can copy it.
use crate::{
    document::Document,
    menus::{Menu, MenuTarget},
    state::{Desktop, MessageView},
    theme::{ACCENT, tint},
};
use gpui_kit::*;
use std::{ops::Range, rc::Rc, time::Duration};

/// A place in a message: a block and a byte offset into its text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Caret {
    pub block: usize,
    pub offset: usize,
}

/// Text selected in one message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextSelection {
    /// The message's [`MessageView::serial`].
    pub message: u64,
    /// What the press took in: a caret, or the word or line of a double or
    /// triple click. Dragging extends it and never drops it.
    origin: (Caret, Caret),
    head: Caret,
}

impl TextSelection {
    /// Start and end, in reading order.
    pub fn range(&self) -> (Caret, Caret) {
        (self.origin.0.min(self.head), self.origin.1.max(self.head))
    }

    /// The selected bytes of block `block`, whose text is `len` bytes long.
    pub fn block_range(&self, block: usize, len: usize) -> Option<Range<usize>> {
        let (start, end) = self.range();
        if !(start.block..=end.block).contains(&block) {
            return None;
        }
        let from = if block == start.block {
            start.offset
        } else {
            0
        };
        let to = if block == end.block { end.offset } else { len };
        (from < to).then_some(from..to)
    }
}

/// What a message's context menu copies, taken when it opens.
pub struct MessageMenu {
    pub text: SharedString,
    pub selection: Option<SharedString>,
}

/// Message text state: the selection, the message a context menu is for, the
/// code last copied with a code block's button (shown as a check for a
/// moment), and the focus target that takes Ctrl+C while text is selected.
pub struct TextState {
    pub selection: Option<TextSelection>,
    pub menu: Option<MessageMenu>,
    pub copied: Option<SharedString>,
    pub focus: FocusHandle,
}

impl TextState {
    pub fn new(cx: &mut App) -> Self {
        Self {
            selection: None,
            menu: None,
            copied: None,
            focus: cx.focus_handle(),
        }
    }
}

impl Desktop {
    /// A message of the open conversation.
    fn message(&self, serial: u64) -> Option<&MessageView> {
        let (server, channel) = self.selected_indices()?;
        self.servers[server].channels[channel]
            .messages
            .iter()
            .find(|message| message.serial == serial)
    }

    /// The selected text, when some is selected in the open conversation.
    pub fn selected_text(&self) -> Option<String> {
        let selection = self.text.selection?;
        let (start, end) = selection.range();
        let text = selected_text(&self.message(selection.message)?.document, start, end);
        (!text.is_empty()).then_some(text)
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

    /// Right click on a message: Copy (the selection in it, if any) and Copy message.
    pub fn open_message_menu(&mut self, message: u64, position: Point<Pixels>) {
        let Some((server, _)) = self.selected_indices() else {
            return;
        };
        let Some(view) = self.message(message) else {
            return;
        };
        let text = view.document.plain_text().into();
        let selection = self
            .text
            .selection
            .filter(|selection| selection.message == message)
            .and_then(|_| self.selected_text())
            .map(SharedString::from);
        self.text.menu = Some(MessageMenu { text, selection });
        self.menu = Some(Menu {
            server,
            target: MenuTarget::Message,
            position,
        });
    }
}

/// The text between two carets: blocks joined by newlines, images left out.
pub fn selected_text(document: &Document, start: Caret, end: Caret) -> String {
    let parts: Vec<&str> = document
        .blocks
        .iter()
        .enumerate()
        .take(end.block + 1)
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
            text.get(from..to)
        })
        .filter(|part| !part.is_empty())
        .collect();
    parts.join("\n")
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

/// `highlights` (sorted, disjoint) over a text of `len` bytes, split where the
/// selection begins and ends, with the selection's background inside it.
fn with_selection(
    highlights: &[(Range<usize>, HighlightStyle)],
    selected: Range<usize>,
    len: usize,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let mut covered = Vec::with_capacity(2 * highlights.len() + 1);
    let mut end = 0;
    for (range, style) in highlights {
        if end < range.start {
            covered.push((end..range.start, HighlightStyle::default()));
        }
        covered.push((range.clone(), *style));
        end = range.end;
    }
    if end < len {
        covered.push((end..len, HighlightStyle::default()));
    }
    let mut split = Vec::with_capacity(covered.len() + 2);
    for (range, style) in covered {
        let inside = range.start.max(selected.start)..range.end.min(selected.end);
        if inside.is_empty() {
            split.push((range, style));
            continue;
        }
        if range.start < inside.start {
            split.push((range.start..inside.start, style));
        }
        let background = Some(tint(ACCENT, 0x70).into());
        split.push((
            inside.clone(),
            HighlightStyle {
                background_color: background,
                ..style
            },
        ));
        if inside.end < range.end {
            split.push((inside.end..range.end, style));
        }
    }
    split
}

/// A block's text with its highlights and the selected part of it, if any.
/// With an id it is a label for screen readers, as `text!` makes.
pub struct BlockText {
    id: Option<ElementId>,
    text: SharedString,
    styled: StyledText,
}

impl BlockText {
    pub fn new(
        id: Option<ElementId>,
        text: SharedString,
        highlights: &[(Range<usize>, HighlightStyle)],
        selected: Option<Range<usize>>,
    ) -> Self {
        let styled = StyledText::new(text.clone());
        let styled = match selected {
            Some(selected) => {
                styled.with_highlights(with_selection(highlights, selected, text.len()))
            }
            None if highlights.is_empty() => styled,
            None => styled.with_highlights(highlights.iter().cloned()),
        };
        Self { id, text, styled }
    }

    /// Maps positions to offsets once the text is laid out.
    pub fn layout(&self) -> TextLayout {
        self.styled.layout().clone()
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

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        self.styled.request_layout(None, inspector_id, window, cx)
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
    }
}

/// A message's text blocks as laid out this frame: block index, text, layout.
pub type Texts = Vec<(usize, SharedString, TextLayout)>;

/// The caret nearest to a position: in the text beside or under it, or at the
/// start of the next text below.
fn caret_near(
    texts: &[(usize, SharedString, TextLayout)],
    position: Point<Pixels>,
) -> Option<Caret> {
    for (block, _, layout) in texts {
        let bounds = layout.bounds();
        if position.y < bounds.top() {
            return Some(Caret {
                block: *block,
                offset: 0,
            });
        }
        if position.y <= bounds.bottom() {
            return Some(Caret {
                block: *block,
                offset: layout
                    .index_for_position(position)
                    .unwrap_or_else(|near| near),
            });
        }
    }
    let (block, text, _) = texts.last()?;
    Some(Caret {
        block: *block,
        offset: text.len(),
    })
}

/// Wraps a message's blocks: a press on its text starts a selection (a double
/// click takes a word, a triple click a line), dragging extends it, and any
/// other press clears it.
pub struct SelectionArea {
    desktop: WeakEntity<Desktop>,
    message: u64,
    /// Whether the selection is in this message.
    selected: bool,
    texts: Rc<[(usize, SharedString, TextLayout)]>,
    child: AnyElement,
}

impl SelectionArea {
    pub fn new(
        desktop: WeakEntity<Desktop>,
        message: u64,
        selected: bool,
        texts: Texts,
        child: impl IntoElement,
    ) -> Self {
        Self {
            desktop,
            message,
            selected,
            texts: texts.into(),
            child: child.into_any_element(),
        }
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
    type PrepaintState = Vec<Hitbox>;

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
    ) -> Vec<Hitbox> {
        self.child.prepaint(window, cx);
        // Above the blocks, inside their clip; they let presses through.
        self.texts
            .iter()
            .map(|(_, _, layout)| window.insert_hitbox(layout.bounds(), HitboxBehavior::Normal))
            .collect()
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        hitboxes: &mut Vec<Hitbox>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
        for hitbox in hitboxes.iter() {
            window.set_cursor_style(CursorStyle::IBeam, hitbox);
        }
        let message = self.message;
        if self.selected {
            // Any press clears the selection; one on text starts another below.
            let desktop = self.desktop.clone();
            window.on_mouse_event(move |event: &MouseDownEvent, phase, _, cx| {
                if phase == DispatchPhase::Capture && event.button == MouseButton::Left {
                    let _ = desktop.update(cx, |this, cx| {
                        if this.text.selection.take().is_some() {
                            cx.notify();
                        }
                    });
                }
            });
            let (desktop, texts) = (self.desktop.clone(), self.texts.clone());
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                if phase != DispatchPhase::Bubble || event.pressed_button != Some(MouseButton::Left)
                {
                    return;
                }
                let Some(head) = caret_near(&texts, event.position) else {
                    return;
                };
                let _ = desktop.update(cx, |this, cx| {
                    if let Some(selection) = &mut this.text.selection
                        && selection.message == message
                        && selection.head != head
                    {
                        selection.head = head;
                        cx.notify();
                    }
                });
            });
            // What is selected is also the primary selection, for middle-click paste.
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            {
                let desktop = self.desktop.clone();
                window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble && event.button == MouseButton::Left {
                        let text = desktop.read_with(cx, |this, _| this.selected_text());
                        if let Ok(Some(text)) = text {
                            cx.write_to_primary(ClipboardItem::new_string(text));
                        }
                    }
                });
            }
        }
        let (desktop, texts, hitboxes) = (
            self.desktop.clone(),
            self.texts.clone(),
            std::mem::take(hitboxes),
        );
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                return;
            }
            let Some((block, text, layout)) = hitboxes
                .iter()
                .position(|hitbox| hitbox.is_hovered(window))
                .map(|index| &texts[index])
            else {
                return;
            };
            let offset = layout
                .index_for_position(event.position)
                .unwrap_or_else(|near| near);
            let taken = match event.click_count {
                0 | 1 => offset..offset,
                2 => word_at(text, offset),
                _ => line_at(text, offset),
            };
            let caret = |offset| Caret {
                block: *block,
                offset,
            };
            let origin = (caret(taken.start), caret(taken.end));
            let _ = desktop.update(cx, |this, cx| {
                this.text.selection = Some(TextSelection {
                    message,
                    origin,
                    head: origin.1,
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
    use super::{Caret, TextSelection, line_at, selected_text, with_selection, word_at};
    use crate::document::{Block, Document};
    use gpui_kit::{HighlightStyle, rgb};

    #[test]
    fn selections_extend_what_the_press_took_in() {
        let caret = |block, offset| Caret { block, offset };
        let word = (caret(1, 4), caret(1, 9));
        let selection = |head| TextSelection {
            message: 7,
            origin: word,
            head,
        };
        assert_eq!(selection(caret(1, 9)).range(), word);
        assert_eq!(selection(caret(1, 6)).range(), word);
        assert_eq!(selection(caret(0, 2)).range(), (caret(0, 2), caret(1, 9)));
        assert_eq!(selection(caret(3, 1)).range(), (caret(1, 4), caret(3, 1)));
        let across = selection(caret(3, 1));
        assert_eq!(across.block_range(0, 10), None);
        assert_eq!(across.block_range(1, 10), Some(4..10));
        assert_eq!(across.block_range(2, 5), Some(0..5));
        assert_eq!(across.block_range(3, 5), Some(0..1));
        assert_eq!(selection(caret(1, 4)).block_range(1, 10), Some(4..9));
        let empty = TextSelection {
            message: 7,
            origin: (caret(1, 4), caret(1, 4)),
            head: caret(1, 4),
        };
        assert_eq!(empty.block_range(1, 10), None);
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
                Block::Text("Hello there".into()),
                image,
                Block::Code {
                    language: "rust".into(),
                    text: "fn main() {}".into(),
                    highlights: Vec::new().into(),
                },
                Block::Text("Bye".into()),
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
        let split = with_selection(&[(2..6, red)], 4..8, 10);
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
}
