// SPDX-License-Identifier: EUPL-1.2
//! Emoji in messages: the pointer over one shows its name, as a tooltip
//! like the picker's, and a click shows a card with it larger, its name and
//! the short codes that type it, one per row. The emoji are found in the message's text
//! with the rule that draws them ([`find_images`]), so what is named is what
//! is shown. Links keep their own hover and click.
use super::{Emoji, describe, find_images, image, image_of, named};
use crate::{state::Desktop, theme::*};
use gpui_kit::{prelude::*, *};
use std::ops::Range;

/// The emoji of the table drawn at byte `offset` of `text`: its range, the
/// emoji and its skin tone. Only the line holding `offset` is searched: no
/// emoji sequence spans a line break.
pub fn emoji_at(text: &str, offset: usize) -> Option<(Range<usize>, &'static Emoji, usize)> {
    let head = text.get(..offset)?;
    let start = head.rfind('\n').map_or(0, |at| at + 1);
    let end = text[offset..]
        .find('\n')
        .map_or(text.len(), |at| offset + at);
    let mut found = Vec::new();
    find_images(&text[start..end], &mut found);
    let (range, _) = found
        .into_iter()
        .map(|(range, image)| (start + range.start..start + range.end, image))
        .find(|(range, _)| range.contains(&offset))?;
    let (emoji, tone) = named(&text[range.clone()])?;
    Some((range, emoji, tone))
}

/// The emoji under the pointer in a message.
#[derive(Clone)]
pub struct HoveredEmoji {
    pub message: u64,
    pub block: usize,
    pub range: Range<usize>,
    pub emoji: &'static Emoji,
    pub tone: usize,
    /// Where the pointer came onto it: the tooltip shows below.
    pub position: Point<Pixels>,
}

/// The card a click on an emoji in a message opens.
pub struct EmojiCard {
    emoji: &'static Emoji,
    tone: usize,
    position: Point<Pixels>,
}

impl Desktop {
    /// A click on an emoji in a message: show its card there.
    pub fn open_emoji_card(
        &mut self,
        hovered: &HoveredEmoji,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.emoji_card = Some(EmojiCard {
            emoji: hovered.emoji,
            tone: hovered.tone,
            position: hovered.position,
        });
        window.focus(&self.emoji_card_focus, cx);
    }

    fn close_emoji_card(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.emoji_card.take().is_some() {
            // Back to the timeline, where the click was.
            window.focus(&self.text.focus, cx);
        }
    }
}

/// The name of the emoji under the pointer, below it, as tooltips look.
pub fn tooltip(hovered: &HoveredEmoji) -> impl IntoElement {
    deferred(
        anchored()
            .position(hovered.position + point(px(0.), px(20.)))
            .snap_to_window_with_margin(px(8.))
            .child(
                // As the picker's tooltips look (dialogs::Tooltip).
                div()
                    .px_2()
                    .py_1()
                    .max_w(px(420.))
                    .rounded(px(6.))
                    .bg(color(APP))
                    .border_1()
                    .border_color(color(BORDER_STRONG))
                    .text_sm()
                    .text_color(color(TEXT))
                    .shadow_md()
                    .child(describe(hovered.emoji, hovered.tone)),
            ),
    )
    .with_priority(1)
}

/// The open card above everything; a click outside or Escape closes it. It
/// is centred under the emoji clicked, or over it in the window's lower half,
/// so it never covers the emoji.
pub fn card(this: &Desktop, window: &Window, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    let card = this.emoji_card.as_ref()?;
    let (anchor, at) = if card.position.y < window.viewport_size().height / 2. {
        (
            gpui_kit::Anchor::TopCenter,
            card.position + point(px(0.), px(16.)),
        )
    } else {
        (
            gpui_kit::Anchor::BottomCenter,
            card.position - point(px(0.), px(16.)),
        )
    };
    let shown = card.emoji.in_tone(card.tone);
    let name = describe(card.emoji, card.tone);
    // Every code that types it, the main one first, each on its own row.
    let codes: Vec<String> = card
        .emoji
        .shortcodes
        .split(' ')
        .filter(|code| !code.is_empty())
        .map(|code| format!(":{code}:"))
        .collect();
    let big = match image_of(shown) {
        Some(id) => image(id, 64.).into_any_element(),
        None => div()
            .font_family(super::FONT)
            .text_size(px(56.))
            .child(shown)
            .into_any_element(),
    };
    Some(
        div()
            .id("emoji-card-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.close_emoji_card(window, cx);
                    cx.notify();
                }),
            )
            .child(deferred(
                anchored()
                    .anchor(anchor)
                    .position(at)
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        div()
                            .id("emoji-card")
                            .role(Role::Dialog)
                            .aria_label(format!("{name}, typed {}", codes.join(" or ")))
                            .track_focus(&this.emoji_card_focus)
                            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                                if event.keystroke.key == "escape" {
                                    this.close_emoji_card(window, cx);
                                    cx.stop_propagation();
                                    cx.notify();
                                }
                            }))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .min_w(px(240.))
                            .max_w(px(360.))
                            .p_4()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap_2()
                            .rounded(px(10.))
                            .bg(color(ELEVATED))
                            .border_1()
                            .border_color(color(BORDER_STRONG))
                            .shadow_lg()
                            .child(big)
                            .child(
                                div()
                                    .w_full()
                                    .text_center()
                                    .whitespace_normal()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(color(TEXT))
                                    .child(text!(name.clone()).with_id("emoji-card-name")),
                            )
                            .child(
                                // "Type" on the left, the codes in rows on the right.
                                div()
                                    .max_w_full()
                                    .flex()
                                    .items_start()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .py(px(3.))
                                            .text_xs()
                                            .text_color(color(TEXT_MUTED))
                                            .child(text!("Type").with_id("emoji-card-type")),
                                    )
                                    .child(
                                        div()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .items_start()
                                            .gap_1()
                                            .children(codes.iter().enumerate().map(
                                                |(row, code)| {
                                                    div()
                                                        .max_w_full()
                                                        .px(px(6.))
                                                        .py(px(2.))
                                                        .rounded(px(4.))
                                                        .bg(color(INPUT))
                                                        .border_1()
                                                        .border_color(color(BORDER))
                                                        .overflow_hidden()
                                                        .whitespace_nowrap()
                                                        .text_ellipsis()
                                                        .font_family(crate::theme::mono_font())
                                                        .text_sm()
                                                        .text_color(color(TEXT))
                                                        .child(
                                                            text!(code.clone())
                                                                .with_id(("emoji-card-code", row)),
                                                        )
                                                },
                                            )),
                                    ),
                            ),
                    ),
            ))
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::emoji_at;

    #[test]
    fn the_emoji_under_an_offset_is_named_with_its_tone() {
        let text = "hi 👋🏽 and ❤️\nnext 😀";
        let at = |needle: &str| text.find(needle).unwrap();
        let (range, wave, tone) = emoji_at(text, at("👋")).unwrap();
        assert_eq!((&text[range], wave.name, tone), ("👋🏽", "waving hand", 3));
        // Anywhere inside it (the skin tone's bytes too).
        assert_eq!(emoji_at(text, at("🏽")).unwrap().1.name, "waving hand");
        let (range, heart, tone) = emoji_at(text, at("❤")).unwrap();
        assert_eq!((&text[range], heart.name, tone), ("❤️", "red heart", 0));
        assert_eq!(emoji_at(text, at("😀")).unwrap().1.name, "grinning face");
        // Text, ™ without U+FE0F (drawn as text), and offsets out of range.
        assert!(emoji_at(text, 0).is_none());
        assert!(emoji_at("Nuschl™", 6).is_none());
        assert!(emoji_at(text, text.len()).is_none());
        assert!(emoji_at(text, text.len() + 5).is_none());
    }
}
