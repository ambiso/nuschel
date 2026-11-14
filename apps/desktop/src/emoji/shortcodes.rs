// SPDX-License-Identifier: EUPL-1.2
//! Short codes in the composer, as in chat apps and terminals: `:heart:`
//! types ❤️. A colon and two characters of a code list the emoji whose codes
//! start with them (or have a word that does) above the composer; Up and
//! Down choose, Enter or Tab inserts, Escape closes the list until another
//! code is typed. Typing the closing colon of a known code replaces it at
//! once. Codes are emojibase's (nix/emoji.py); the emoji takes the skin tone
//! chosen in the picker.
//!
//! A code starts only at the start of the text, after a space, a bracket, a
//! quote or another emoji: never inside a word, a time (`10:30`) or an
//! address (`https://`).
use super::{Emoji, all, image, image_of};
use crate::{state::Desktop, theme::*};
use gpui_kit::{prelude::*, *};
use std::{collections::HashMap, ops::Range, sync::OnceLock};

/// emojibase's longest code has 46 characters.
const MAX_CODE: usize = 64;
/// Characters typed after the colon before the list shows: `:o` or `:p` in
/// a sentence would otherwise open it.
const MIN_QUERY: usize = 2;
/// Rows the list shows.
const SHOWN: usize = 8;

fn code_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-')
}

/// Whether a code's colon may follow `before` (None: the start of the text).
fn may_follow(before: Option<char>) -> bool {
    before.is_none_or(|c| !code_char(c) && !matches!(c, ':' | '/' | '.'))
}

/// The code being typed before `cursor`: the range from its colon to the
/// cursor, and what follows the colon (possibly nothing).
pub(super) fn typed_at(text: &str, cursor: usize) -> Option<(Range<usize>, &str)> {
    let before = text.get(..cursor)?;
    let mut start = cursor;
    for (at, c) in before.char_indices().rev().take(MAX_CODE + 1) {
        if !code_char(c) {
            break;
        }
        start = at;
    }
    let code = &before[start..];
    if code.len() > MAX_CODE {
        return None;
    }
    let colon = start.checked_sub(1)?;
    if before.as_bytes()[colon] != b':' {
        return None;
    }
    may_follow(before[..colon].chars().next_back()).then_some((colon..cursor, code))
}

/// Every emoji by each of its codes.
fn by_code() -> &'static HashMap<&'static str, &'static Emoji> {
    static BY_CODE: OnceLock<HashMap<&'static str, &'static Emoji>> = OnceLock::new();
    BY_CODE.get_or_init(|| {
        all()
            .flat_map(|emoji| emoji.shortcodes.split(' ').map(move |code| (code, emoji)))
            .filter(|(code, _)| !code.is_empty())
            .collect()
    })
}

/// A known code whose closing colon is just before `cursor` (`:heart:|`),
/// from its opening colon, with its emoji.
pub(super) fn closed_at(text: &str, cursor: usize) -> Option<(Range<usize>, &'static Emoji)> {
    let inner = text.get(..cursor)?.strip_suffix(':')?;
    let (range, code) = typed_at(inner, inner.len())?;
    let emoji = by_code().get(code.to_ascii_lowercase().as_str())?;
    Some((range.start..cursor, emoji))
}

/// The emoji a code being typed may become, best first, each with the code
/// that matched: the code itself, then codes starting with it, then codes
/// with a later word starting with it (`:smile` finds :sweat_smile:);
/// shorter codes first, then in the table's order.
pub(super) fn complete(typed: &str) -> Vec<(&'static Emoji, &'static str)> {
    let typed = typed.to_ascii_lowercase();
    let mut found: Vec<((u8, usize, usize), &'static Emoji, &'static str)> = all()
        .enumerate()
        .filter_map(|(order, emoji)| {
            emoji
                .shortcodes
                .split(' ')
                .filter_map(|code| {
                    let rank = if code == typed {
                        0
                    } else if code.starts_with(&typed) {
                        1
                    } else if code
                        .split(['_', '-'])
                        .skip(1)
                        .any(|word| word.starts_with(&typed))
                    {
                        2
                    } else {
                        return None;
                    };
                    Some(((rank, code.len(), order), emoji, code))
                })
                .min_by_key(|(key, ..)| *key)
        })
        .collect();
    found.sort_by_key(|(key, ..)| *key);
    found.truncate(SHOWN);
    found
        .into_iter()
        .map(|(_, emoji, code)| (emoji, code))
        .collect()
}

/// The list while it shows: where its code is typed, what it found and
/// which row is chosen.
struct Completion {
    range: Range<usize>,
    found: Vec<(&'static Emoji, &'static str)>,
    chosen: usize,
}

/// The composer's short codes: the open list, the code whose list Escape
/// closed (by its colon), and the text as it was before the last change, to
/// tell a typed closing colon from one that came back with an undo.
#[derive(Default)]
pub struct Shortcodes {
    open: Option<Completion>,
    closed: Option<usize>,
    before: SharedString,
}

/// Whether `now` is `before` with one colon typed at `cursor - 1`.
fn typed_colon(before: &str, now: &str, cursor: usize) -> bool {
    cursor >= 1
        && now.len() == before.len() + 1
        && now.as_bytes().get(cursor - 1) == Some(&b':')
        && now.get(..cursor - 1) == before.get(..cursor - 1)
        && now.get(cursor..) == before.get(cursor - 1..)
}

impl Desktop {
    /// The composer changed: a typed closing colon of a known code types its
    /// emoji, and a code being typed lists what it may become.
    pub fn shortcodes_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (text, cursor, plain) = self.composer.update(cx, |input, cx| {
            let plain =
                input.selected_range().is_empty() && input.marked_text_range(window, cx).is_none();
            (input.value(), input.cursor(), plain)
        });
        let before = std::mem::replace(&mut self.shortcodes.before, text.clone());
        self.shortcodes.open = None;
        if !plain {
            return;
        }
        if typed_colon(&before, &text, cursor)
            && let Some((range, emoji)) = closed_at(&text, cursor)
        {
            self.shortcodes.closed = None;
            return self.type_shortcode(range, emoji, window, cx);
        }
        let Some((range, typed)) = typed_at(&text, cursor) else {
            self.shortcodes.closed = None;
            return;
        };
        if typed.len() < MIN_QUERY || self.shortcodes.closed == Some(range.start) {
            return;
        }
        self.shortcodes.closed = None;
        let found = complete(typed);
        if !found.is_empty() {
            self.shortcodes.open = Some(Completion {
                range,
                found,
                chosen: 0,
            });
        }
    }

    /// Replaces the code in `range` with the emoji, in the chosen skin tone.
    fn type_shortcode(
        &mut self,
        range: Range<usize>,
        emoji: &'static Emoji,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.shortcodes.open = None;
        self.remember_emoji(emoji);
        let text = emoji.in_tone(self.emoji_tone);
        self.composer.update(cx, |input, cx| {
            input.set_selected_range(range, cx);
            input.replace_text_in_range(None, text, window, cx);
            input.focus(window, cx);
        });
    }

    /// Types the chosen row's emoji.
    fn accept_shortcode(
        &mut self,
        row: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.shortcodes.open.as_ref() else {
            return;
        };
        let Some((emoji, _)) = open.found.get(row.unwrap_or(open.chosen)) else {
            return;
        };
        let (range, emoji) = (open.range.clone(), *emoji);
        self.type_shortcode(range, emoji, window, cx);
    }

    /// A composer key while the list shows: Up, Down, Enter, Tab, Escape.
    /// True when the list took it.
    pub fn shortcode_key(
        &mut self,
        key: ShortcodeKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(open) = self.shortcodes.open.as_mut() else {
            return false;
        };
        let rows = open.found.len();
        match key {
            ShortcodeKey::Up => open.chosen = (open.chosen + rows - 1) % rows,
            ShortcodeKey::Down => open.chosen = (open.chosen + 1) % rows,
            ShortcodeKey::Accept => self.accept_shortcode(None, window, cx),
            ShortcodeKey::Close => {
                self.shortcodes.closed = Some(open.range.start);
                self.shortcodes.open = None;
            }
        }
        cx.notify();
        true
    }
}

/// What a key does to the list.
#[derive(Clone, Copy)]
pub enum ShortcodeKey {
    Up,
    Down,
    Accept,
    Close,
}

/// The list above the composer, while a code is typed and matches emoji
/// (and the picker is closed).
pub fn list(this: &Desktop, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    let open = this
        .shortcodes
        .open
        .as_ref()
        .filter(|_| this.emoji.is_none())?;
    let tone = this.emoji_tone;
    Some(
        div()
            .id("emoji-shortcodes")
            .role(Role::ListBox)
            .aria_label("Emoji")
            .absolute()
            .left_0()
            .bottom_full()
            .mb_2()
            .w(px(340.))
            .p_1()
            .flex()
            .flex_col()
            .rounded(px(10.))
            .bg(color(ELEVATED))
            .border_1()
            .border_color(color(BORDER_STRONG))
            .shadow_lg()
            // The composer keeps the focus (and the keys) while one is clicked.
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .children(open.found.iter().enumerate().map(|(row, (emoji, code))| {
                let chosen = row == open.chosen;
                let shown = emoji.in_tone(tone);
                div()
                    .id(("emoji-shortcode", row))
                    .role(Role::ListBoxOption)
                    .aria_selected(chosen)
                    .aria_label(format!("{} :{code}:", emoji.name))
                    .h(px(32.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .when(chosen, |row| row.bg(color(SELECTED)))
                    .hover(|style| style.bg(color(HOVER)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.accept_shortcode(Some(row), window, cx);
                        cx.notify();
                    }))
                    .children(image_of(shown).map(|id| image(id, 20.)))
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_sm()
                            .text_color(color(TEXT))
                            .child(
                                text!(format!(":{code}:")).with_id(("emoji-shortcode-code", row)),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_xs()
                            .text_color(color(TEXT_FAINT))
                            .child(text!(emoji.name).with_id(("emoji-shortcode-name", row))),
                    )
            }))
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::{closed_at, complete, typed_at, typed_colon};

    fn typed(text: &str) -> Option<&str> {
        typed_at(text, text.len()).map(|(_, code)| code)
    }

    #[test]
    fn a_code_starts_after_a_space_a_bracket_or_an_emoji() {
        assert_eq!(typed(":hea"), Some("hea"));
        assert_eq!(typed("I :hea"), Some("hea"));
        assert_eq!(typed("(:+1"), Some("+1"));
        assert_eq!(typed("😀:fire"), Some("fire"));
        assert_eq!(typed_at("hi :wa", 6), Some((3..6, "wa")));
        // Not inside words, times, addresses, or after another code's colon.
        for text in [
            "word:hea",
            "10:30",
            "https://example",
            "a.:x",
            "::hea",
            "hi",
        ] {
            assert_eq!(typed(text), None, "{text}");
        }
        // Only up to the cursor; overly long codes are none.
        assert_eq!(typed_at(":heart and more", 4), Some((0..4, "hea")));
        assert_eq!(typed(&format!(":{}", "a".repeat(65))), None);
        // A cursor inside a character is no place.
        assert_eq!(typed_at("é", 1), None);
    }

    #[test]
    fn a_closing_colon_of_a_known_code_is_its_emoji() {
        let closed =
            |text: &str| closed_at(text, text.len()).map(|(range, emoji)| (range, emoji.name));
        assert_eq!(closed(":heart:"), Some((0..7, "red heart")));
        assert_eq!(closed("so :+1:"), Some((3..7, "thumbs up")));
        assert_eq!(closed("so :THUMBSUP:"), Some((3..13, "thumbs up")));
        assert_eq!(closed(":nothing_like_this:"), None);
        assert_eq!(closed("10:30:"), None);
        assert_eq!(closed(":heart"), None);
    }

    #[test]
    fn only_a_typed_colon_closes_a_code() {
        assert!(typed_colon(":heart", ":heart:", 7));
        assert!(typed_colon(":heart and", ":heart: and", 7));
        // Pasted, or back with an undo: more than one character changed.
        assert!(!typed_colon("", ":heart:", 7));
        assert!(!typed_colon("❤️", ":heart:", 7));
        assert!(!typed_colon(":heart", ":heart!", 7));
    }

    #[test]
    fn completions_rank_exact_then_starts_then_words() {
        let codes =
            |typed: &str| -> Vec<&str> { complete(typed).iter().map(|(_, code)| *code).collect() };
        assert_eq!(codes("heart")[0], "heart");
        assert_eq!(complete("heart")[0].0.name, "red heart");
        assert!(codes("hea").iter().all(|code| code.starts_with("hea")));
        assert_eq!(codes("+1")[0], "+1");
        let smile = codes("smile");
        assert_eq!(smile[0], "smile");
        assert!(
            smile.iter().any(|code| code.contains("_smile")),
            "{smile:?}"
        );
        assert!(complete("zzqqzz").is_empty());
        assert!(complete("e").len() <= super::SHOWN);
        // Each emoji once.
        let found = complete("thumbs");
        for (at, (emoji, _)) in found.iter().enumerate() {
            assert!(
                !found[..at]
                    .iter()
                    .any(|(seen, _)| std::ptr::eq(*seen, *emoji))
            );
        }
    }
}
