// SPDX-License-Identifier: EUPL-1.2
//! Emoji: the picker beside the composer (also on Ctrl+. or Ctrl+; as in
//! GTK), and Twemoji images for emoji in all text the window draws.
//!
//! The images and the picker's names, groups and keywords come from the
//! pinned `twemoji` and `emojibase-data` flake inputs (nix/emoji.nix) and are
//! compiled in by build.rs; nothing is loaded at run time. They are trusted
//! build inputs, checked by nix/emoji.py to be static drawings, and drawn by
//! GPUI's SVG renderer like the interface icons.
//!
//! [`find_images`] decides which text is drawn as an image, from the text
//! alone: characters Unicode shows as text by default (™, ©, ❤) only with
//! U+FE0F after them, nothing with U+FE0E after it. GPUI's text painting asks
//! it for every line (vendor/gpui-pre, `EmojiImages`), so messages and the
//! composer's text show the same images as the picker.
use crate::{state::Desktop, theme::*};
use gpui_kit::{prelude::*, *};
use std::{fmt::Write as _, future::Future, ops::Range, sync::Arc, sync::OnceLock};

mod assets {
    include!(concat!(env!("OUT_DIR"), "/emoji_assets.rs"));
}

const COLUMNS: usize = 9;
const CELL: f32 = 36.;
const IMAGE: f32 = 26.;
const RECENT: usize = 18;
/// The longest image key has nine code points (without U+FE0F).
const MAX_PARTS: usize = 10;
const TEXT_STYLE: char = '\u{FE0E}';
const EMOJI_STYLE: char = '\u{FE0F}';
/// Skin tones by number, 1 (light) to 5 (dark); 0 is the default yellow.
pub const TONES: usize = 5;
const TONE_NAMES: [&str; TONES + 1] = [
    "Default skin tone",
    "Light skin tone",
    "Medium-light skin tone",
    "Medium skin tone",
    "Medium-dark skin tone",
    "Dark skin tone",
];
const TONE_SAMPLE: [&str; TONES + 1] = ["✋", "✋🏻", "✋🏼", "✋🏽", "✋🏾", "✋🏿"];

/// The system's colour emoji font. GPUI shapes every emoji range with it
/// (`EmojiImages::font_family`), so emoji get their usual width even where
/// the UI font has a glyph of its own (DejaVu Sans has the classic smileys,
/// and most fonts have ™); Twemoji is then drawn over that advance. Without
/// it installed, GPUI's fallback shapes them. (On Linux this needs the patch
/// in vendor/gpui-pre-wgpu.)
pub const FONT: &str = if cfg!(target_os = "macos") {
    "Apple Color Emoji"
} else if cfg!(windows) {
    "Segoe UI Emoji"
} else {
    "Noto Color Emoji"
};

/// The image of an image key, and whether its character is text by default.
fn lookup(key: &str) -> Option<(u32, bool)> {
    let at = assets::IMAGES
        .binary_search_by(|(candidate, _, _)| (*candidate).cmp(key))
        .ok()?;
    Some((at as u32, assets::IMAGES[at].1))
}

/// Whether some image key starts with `prefix` (keys are sorted).
fn continues(prefix: &str) -> bool {
    let at = assets::IMAGES.partition_point(|(candidate, _, _)| *candidate < prefix);
    assets::IMAGES
        .get(at)
        .is_some_and(|(candidate, _, _)| candidate.starts_with(prefix))
}

/// The byte ranges of `text` drawn as Twemoji images, with their images:
/// at each position the longest sequence with an image, U+FE0F ignored for
/// the match but kept in the range. A single character that is text by
/// default needs U+FE0F after it; anything followed by U+FE0E stays text.
pub fn find_images(text: &str, found: &mut Vec<(Range<usize>, u32)>) {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let byte_at = |index: usize| chars.get(index).map_or(text.len(), |(at, _)| *at);
    let mut key = String::new();
    let mut i = 0;
    while i < chars.len() {
        let first = chars[i].1;
        // Keycaps start with # * or a digit; every other key outside ASCII.
        if first.is_ascii() && !matches!(first, '#' | '*' | '0'..='9') {
            i += 1;
            continue;
        }
        key.clear();
        // (end as a char index, image, text by default)
        let mut best: Option<(usize, u32, bool)> = None;
        let (mut j, mut parts) = (i, 0);
        while j < chars.len() && parts < MAX_PARTS {
            let c = chars[j].1;
            if c == EMOJI_STYLE {
                j += 1;
                if let Some(best) = best.as_mut().filter(|best| best.0 == j - 1) {
                    best.0 = j;
                }
                continue;
            }
            if parts > 0 {
                key.push('-');
            }
            let _ = write!(key, "{:x}", c as u32);
            parts += 1;
            j += 1;
            if let Some((image, text_default)) = lookup(&key) {
                best = Some((j, image, text_default));
            } else if !continues(&key) {
                break;
            }
        }
        match best {
            Some((end, image, text_default)) => {
                let single = end == i + 1 || (end == i + 2 && chars[i + 1].1 == EMOJI_STYLE);
                let styled = chars[end - 1].1 == EMOJI_STYLE;
                let as_text = chars.get(end).is_some_and(|(_, c)| *c == TEXT_STYLE)
                    || (text_default && single && !styled);
                if !as_text {
                    found.push((byte_at(i)..byte_at(end), image));
                }
                i = end;
            }
            None => i += 1,
        }
    }
}

/// The image of `emoji` if all of it is one image.
fn image_of(emoji: &str) -> Option<u32> {
    let mut found = Vec::new();
    find_images(emoji, &mut found);
    match found.as_slice() {
        [(range, image)] if range.start == 0 && range.end == emoji.len() => Some(*image),
        _ => None,
    }
}

/// GPUI's text painting draws emoji with these images.
pub struct Twemoji;

impl EmojiImages for Twemoji {
    fn find(&self, text: &str, found: &mut Vec<(Range<usize>, u32)>) {
        find_images(text, found);
    }

    fn svg(&self, id: u32) -> Option<&'static [u8]> {
        assets::IMAGES.get(id as usize).map(|(_, _, svg)| *svg)
    }

    fn font_family(&self) -> Option<&'static str> {
        Some(FONT)
    }
}

/// Draws emoji in text as Twemoji from now on; call once at startup.
pub fn install() {
    set_emoji_images(&Twemoji);
}

/// An emoji as an image `edge` wide, rasterised for the window's scale.
fn image(id: u32, edge: f32) -> Img {
    img(move |window: &mut Window, cx: &mut App| {
        let pixels = (edge * window.scale_factor()).round() as u32;
        window.use_asset::<EmojiImage>(&(id, pixels), cx)
    })
    .size(px(edge))
    .flex_shrink_0()
}

/// A Twemoji SVG at an edge length in device pixels; GPUI caches each.
enum EmojiImage {}

impl Asset for EmojiImage {
    type Source = (u32, u32);
    type Output = Result<Arc<RenderImage>, ImageCacheError>;

    fn load(
        (id, edge): (u32, u32),
        cx: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        let renderer = cx.svg_renderer();
        async move {
            // An unknown id parses nothing, which is an error.
            let svg = Twemoji.svg(id).unwrap_or_default();
            let parsed = renderer.parse_svg(svg)?;
            let edge = DevicePixels::from(edge.clamp(1, 512));
            Ok(renderer.render_parsed(&parsed, SvgSize::ExactSize(size(edge, edge)))?)
        }
    }
}

/// One emoji of the picker.
pub struct Emoji {
    pub emoji: &'static str,
    pub name: &'static str,
    /// Lower-case words of its name, keywords and short codes.
    search: &'static str,
    /// Its variants by skin tone, light to dark; empty where there is none.
    tones: Option<[&'static str; TONES]>,
}

impl Emoji {
    /// The emoji in skin tone `tone` (0 for the default), if it has that tone.
    pub fn in_tone(&self, tone: usize) -> &'static str {
        match (&self.tones, tone) {
            (Some(tones), 1..=TONES) if !tones[tone - 1].is_empty() => tones[tone - 1],
            _ => self.emoji,
        }
    }

    fn matches(&self, words: &[String]) -> bool {
        words.iter().all(|word| {
            self.search
                .split(' ')
                .any(|part| part.starts_with(word.as_str()))
        })
    }
}

struct Group {
    name: &'static str,
    emoji: Vec<Emoji>,
}

fn groups() -> &'static [Group] {
    static GROUPS: OnceLock<Vec<Group>> = OnceLock::new();
    GROUPS.get_or_init(|| parse(assets::TABLE))
}

/// The table nix/emoji.py writes: `@Group`, then emoji, name, search words
/// and optionally five skin tones, separated by tabs.
fn parse(table: &'static str) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for line in table.lines() {
        if let Some(name) = line.strip_prefix('@') {
            groups.push(Group {
                name,
                emoji: Vec::new(),
            });
            continue;
        }
        let fields: Vec<&'static str> = line.split('\t').collect();
        let (Some(group), [emoji, name, search, tones @ ..]) = (groups.last_mut(), &fields[..])
        else {
            continue;
        };
        group.emoji.push(Emoji {
            emoji,
            name,
            search,
            tones: <[&str; TONES]>::try_from(tones).ok(),
        });
    }
    groups
}

fn all() -> impl Iterator<Item = &'static Emoji> {
    groups().iter().flat_map(|group| group.emoji.iter())
}

/// Emoji with a word of their name, keywords or short codes starting with
/// each word of `query`.
fn matches(query: &str) -> Vec<&'static Emoji> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    all().filter(|emoji| emoji.matches(&words)).collect()
}

/// Whether a keystroke opens the picker.
pub fn shortcut(keystroke: &Keystroke) -> bool {
    let modifiers = &keystroke.modifiers;
    modifiers.control
        && !modifiers.alt
        && !modifiers.platform
        && matches!(keystroke.key.as_str(), "." | ";")
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Emoji,
    Stickers,
    Gifs,
}

/// One line of the emoji tab: a group heading, up to `COLUMNS` emoji, or
/// the "nothing found" note. All lines are one cell high, so only the
/// visible ones are built (GPUI's uniform list).
enum Row {
    Heading(&'static str),
    Emoji(Vec<&'static Emoji>),
    Nothing,
}

/// The open picker: its tab, what is typed, and the lines it shows.
pub struct Picker {
    tab: Tab,
    query: String,
    rows: Vec<Row>,
    /// The skin tone choices are shown in place of the search.
    choosing_tone: bool,
}

impl Picker {
    fn new(tab: Tab, query: String, recent: &[&'static Emoji]) -> Self {
        let mut rows = Vec::new();
        let add = |rows: &mut Vec<Row>, emoji: Vec<&'static Emoji>| {
            rows.extend(
                emoji
                    .chunks(COLUMNS)
                    .map(|chunk| Row::Emoji(chunk.to_vec())),
            );
        };
        if query.trim().is_empty() {
            if !recent.is_empty() {
                rows.push(Row::Heading("Recently used"));
                add(&mut rows, recent.to_vec());
            }
            for group in groups() {
                rows.push(Row::Heading(group.name));
                add(&mut rows, group.emoji.iter().collect());
            }
        } else {
            let found = matches(&query);
            if found.is_empty() {
                rows.push(Row::Nothing);
            }
            add(&mut rows, found);
        }
        Self {
            tab,
            query,
            rows,
            choosing_tone: false,
        }
    }

    /// What Enter inserts: the first emoji shown.
    fn first(&self) -> Option<&'static Emoji> {
        self.rows.iter().find_map(|row| match row {
            Row::Emoji(emoji) => emoji.first().copied(),
            _ => None,
        })
    }
}

impl Desktop {
    pub fn open_emoji(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.emoji = Some(Picker::new(Tab::Emoji, String::new(), &self.emoji_recent));
        window.focus(&self.emoji_focus, cx);
    }

    pub fn toggle_emoji(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.emoji.is_some() {
            self.close_emoji(window, cx);
        } else {
            self.open_emoji(window, cx);
        }
    }

    pub fn close_emoji(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.emoji = None;
        self.composer
            .update(cx, |input, cx| input.focus(window, cx));
    }

    /// Inserts an emoji, in the chosen skin tone, at the composer's cursor.
    /// The picker closes unless `keep_open` (Shift held), so several emoji
    /// can be added in a row.
    fn pick_emoji(
        &mut self,
        emoji: &'static Emoji,
        keep_open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.emoji_recent
            .retain(|recent| !std::ptr::eq(*recent, emoji));
        self.emoji_recent.insert(0, emoji);
        self.emoji_recent.truncate(RECENT);
        let text = emoji.in_tone(self.emoji_tone);
        if keep_open {
            self.composer
                .update(cx, |input, cx| input.insert(text, window, cx));
            window.focus(&self.emoji_focus, cx);
        } else {
            self.close_emoji(window, cx);
            self.composer
                .update(cx, |input, cx| input.insert(text, window, cx));
        }
    }

    fn set_emoji_tab(&mut self, tab: Tab) {
        if let Some(picker) = &mut self.emoji {
            picker.tab = tab;
            picker.choosing_tone = false;
        }
    }

    fn set_emoji_tone(&mut self, tone: usize) {
        self.emoji_tone = tone.min(TONES);
        if let Some(picker) = &mut self.emoji {
            picker.choosing_tone = false;
        }
    }

    fn emoji_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(picker) = &mut self.emoji else {
            return;
        };
        let keystroke = &event.keystroke;
        let modifiers = &keystroke.modifiers;
        match keystroke.key.as_str() {
            "escape" if picker.choosing_tone => {
                picker.choosing_tone = false;
                cx.stop_propagation();
                cx.notify();
                return;
            }
            "escape" => return self.close_emoji(window, cx),
            _ if shortcut(keystroke) => return self.close_emoji(window, cx),
            _ => {}
        }
        if picker.tab != Tab::Emoji {
            return;
        }
        let mut query = picker.query.clone();
        match keystroke.key.as_str() {
            "enter" => {
                return match picker.first() {
                    Some(emoji) => self.pick_emoji(emoji, modifiers.shift, window, cx),
                    None => self.close_emoji(window, cx),
                };
            }
            "backspace" => {
                query.pop();
            }
            _ => match &keystroke.key_char {
                Some(text) if !modifiers.control && !modifiers.alt && !modifiers.platform => {
                    query.push_str(text);
                }
                _ => return,
            },
        }
        let choosing_tone = picker.choosing_tone;
        let mut picker = Picker::new(Tab::Emoji, query, &self.emoji_recent);
        picker.choosing_tone = choosing_tone;
        self.emoji = Some(picker);
        cx.stop_propagation();
        cx.notify();
    }
}

fn cell(emoji: &'static Emoji, tone: usize, cx: &mut Context<Desktop>) -> AnyElement {
    let shown = emoji.in_tone(tone);
    let name = emoji.name;
    div()
        .id(SharedString::from(shown))
        .role(Role::Button)
        .aria_label(name)
        .size(px(CELL))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .cursor_pointer()
        .hover(|style| style.bg(color(HOVER)))
        .tooltip(move |_, cx| cx.new(|_| crate::dialogs::Tooltip(name.into())).into())
        .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
            this.pick_emoji(emoji, event.modifiers().shift, window, cx);
            cx.notify();
        }))
        .child(match image_of(shown) {
            Some(id) => image(id, IMAGE).into_any_element(),
            // Every emoji of the table has an image (tested).
            None => div()
                .font_family(FONT)
                .text_size(px(22.))
                .child(shown)
                .into_any_element(),
        })
        .into_any_element()
}

fn row(row: &Row, tone: usize, cx: &mut Context<Desktop>) -> AnyElement {
    match row {
        Row::Heading(name) => div()
            .h(px(CELL))
            .px_1()
            .pb_1()
            .flex()
            .items_end()
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(color(TEXT_FAINT))
            .child(text!(id = *name, *name))
            .into_any_element(),
        Row::Emoji(emoji) => div()
            .flex()
            .children(emoji.iter().map(|emoji| cell(emoji, tone, cx)))
            .into_any_element(),
        Row::Nothing => div()
            .h(px(CELL))
            .px_1()
            .flex()
            .items_center()
            .text_sm()
            .text_color(color(TEXT_MUTED))
            .child(text!("No emoji found").with_id("emoji-none"))
            .into_any_element(),
    }
}

fn tab(tab: Tab, current: Tab, label: &'static str, cx: &mut Context<Desktop>) -> AnyElement {
    let selected = tab == current;
    div()
        .id(label)
        .role(Role::Tab)
        .aria_label(label)
        .aria_selected(selected)
        .tab_index(0)
        .px_3()
        .py(px(4.))
        .rounded(px(6.))
        .cursor_pointer()
        .text_sm()
        .text_color(color(if selected { TEXT } else { TEXT_MUTED }))
        .when(selected, |tab| tab.bg(color(SELECTED)))
        .hover(|style| style.bg(color(HOVER)))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.set_emoji_tab(tab);
            cx.notify();
        }))
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                this.set_emoji_tab(tab);
                cx.stop_propagation();
                cx.notify();
            }
        }))
        .child(text!(label).with_id("emoji-tab-label"))
        .into_any_element()
}

/// The button showing the chosen skin tone, or one choice of tone.
fn tone_button(
    id: &'static str,
    tone: usize,
    selected: bool,
    action: impl Fn(&mut Desktop) + 'static,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let name = TONE_NAMES[tone];
    // The button is replaced by what it opens or chooses, so the picker
    // takes the focus back (typing goes on searching).
    let action = std::rc::Rc::new(
        move |this: &mut Desktop, window: &mut Window, cx: &mut Context<Desktop>| {
            action(this);
            window.focus(&this.emoji_focus, cx);
        },
    );
    let key_action = action.clone();
    div()
        .id(SharedString::from(format!("{id}-{tone}")))
        .role(Role::Button)
        .aria_label(name)
        .tab_index(0)
        .size(px(30.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .cursor_pointer()
        .when(selected, |button| button.bg(color(SELECTED)))
        .hover(|style| style.bg(color(HOVER)))
        .tooltip(move |_, cx| cx.new(|_| crate::dialogs::Tooltip(name.into())).into())
        .on_click(cx.listener(move |this, _, window, cx| {
            action(this, window, cx);
            cx.notify();
        }))
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                key_action(this, window, cx);
                cx.stop_propagation();
                cx.notify();
            }
        }))
        .children(image_of(TONE_SAMPLE[tone]).map(|id| image(id, 20.)))
        .into_any_element()
}

fn search_bar(picker: &Picker, tone: usize, cx: &mut Context<Desktop>) -> AnyElement {
    if picker.choosing_tone {
        return div()
            .id("emoji-tones")
            .role(Role::Group)
            .aria_label("Skin tone")
            .mx_2()
            .mb_2()
            .p_1()
            .rounded(px(6.))
            .bg(color(INPUT))
            .border_1()
            .border_color(color(BORDER_STRONG))
            .flex()
            .justify_between()
            .children((0..=TONES).map(|choice| {
                tone_button(
                    "emoji-tone",
                    choice,
                    choice == tone,
                    move |this| this.set_emoji_tone(choice),
                    cx,
                )
            }))
            .into_any_element();
    }
    let search = if picker.query.is_empty() {
        div()
            .flex_1()
            .text_color(color(TEXT_FAINT))
            .child(text!("Search emoji").with_id("emoji-search"))
    } else {
        div()
            .flex_1()
            .child(text!(picker.query.clone()).with_id("emoji-search"))
    };
    div()
        .mx_2()
        .mb_2()
        .flex()
        .items_center()
        .gap_1()
        .child(
            div()
                .flex_1()
                .px_3()
                .py_2()
                .rounded(px(6.))
                .bg(color(INPUT))
                .border_1()
                .border_color(color(ACCENT))
                .flex()
                .items_center()
                .gap_2()
                .text_sm()
                .child(crate::assets::icon("search", 14., color(TEXT_MUTED)))
                .child(search),
        )
        .child(tone_button(
            "emoji-tone-current",
            tone,
            false,
            |this| {
                if let Some(picker) = &mut this.emoji {
                    picker.choosing_tone = true;
                }
            },
            cx,
        ))
        .into_any_element()
}

fn coming_soon(what: &'static str) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .text_sm()
        .text_color(color(TEXT_MUTED))
        .child(text!(what).with_id("emoji-coming-soon"))
        .into_any_element()
}

/// The open picker above the composer; a click outside closes it.
pub fn overlay(this: &Desktop, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    let picker = this.emoji.as_ref()?;
    let tone = this.emoji_tone;
    let body = match picker.tab {
        Tab::Emoji => div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(search_bar(picker, tone, cx))
            .child(
                uniform_list(
                    "emoji-grid",
                    picker.rows.len(),
                    cx.processor(|this, range: Range<usize>, _, cx| {
                        let Some(picker) = &this.emoji else {
                            return Vec::new();
                        };
                        let tone = this.emoji_tone;
                        let rows = &picker.rows;
                        rows[range.start.min(rows.len())..range.end.min(rows.len())]
                            .iter()
                            .map(|line| row(line, tone, cx))
                            .collect()
                    }),
                )
                .flex_1()
                .min_h_0()
                .px_2()
                .pb_2(),
            )
            .child(
                div()
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(color(BORDER))
                    .text_xs()
                    .text_color(color(TEXT_FAINT))
                    .child(
                        text!("Shift+click adds several · Twemoji, CC-BY 4.0")
                            .with_id("emoji-hint"),
                    ),
            )
            .into_any_element(),
        Tab::Stickers => coming_soon("Stickers are coming soon."),
        Tab::Gifs => coming_soon("GIFs are coming soon."),
    };
    Some(
        div()
            .id("emoji-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.close_emoji(window, cx);
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("emoji-picker")
                    .role(Role::Dialog)
                    .aria_label("Emoji, stickers and GIFs")
                    .track_focus(&this.emoji_focus)
                    .on_key_down(
                        cx.listener(|this, event, window, cx| this.emoji_key(event, window, cx)),
                    )
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .absolute()
                    .right(px(24.))
                    .bottom(px(78.))
                    .w(px(CELL * COLUMNS as f32 + 26.))
                    .h(px(400.))
                    .flex()
                    .flex_col()
                    .rounded(px(10.))
                    .bg(color(ELEVATED))
                    .border_1()
                    .border_color(color(BORDER_STRONG))
                    .shadow_lg()
                    .child(
                        div()
                            .id("emoji-tabs")
                            .role(Role::TabList)
                            .aria_label("Picker")
                            .m_2()
                            .flex()
                            .gap_1()
                            .child(tab(Tab::Emoji, picker.tab, "Emoji", cx))
                            .child(tab(Tab::Stickers, picker.tab, "Stickers", cx))
                            .child(tab(Tab::Gifs, picker.tab, "GIFs", cx)),
                    )
                    .child(body),
            )
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    // Not `super::*`: gpui_kit's glob import brings GPUI's own `test` attribute.
    use super::{TONES, Twemoji, all, assets, find_images, groups, image_of, matches};
    use gpui_kit::{DevicePixels, EmojiImages, SvgRenderer, SvgSize, size};
    use std::sync::Arc;

    fn images(text: &str) -> Vec<&str> {
        let mut found = Vec::new();
        find_images(text, &mut found);
        found.into_iter().map(|(range, _)| &text[range]).collect()
    }

    #[test]
    fn the_table_parses_into_groups() {
        assert_eq!(groups().len(), 9);
        assert!(groups().iter().all(|group| !group.emoji.is_empty()));
        assert_eq!(groups()[0].name, "Smileys & emotion");
        let grinning = all().find(|emoji| emoji.emoji == "😀").unwrap();
        assert_eq!(grinning.name, "grinning face");
        // Every emoji the picker offers, in every tone, is one image.
        for emoji in all() {
            for tone in 0..=TONES {
                let shown = emoji.in_tone(tone);
                assert!(image_of(shown).is_some(), "{shown} ({})", emoji.name);
            }
        }
    }

    #[test]
    fn search_matches_names_keywords_and_short_codes() {
        let found = |query: &str, name: &str| matches(query).iter().any(|e| e.name == name);
        assert!(found("grin", "grinning face"));
        assert!(found("thumbs up", "thumbs up"));
        assert!(found("THUMBSUP", "thumbs up"));
        // A keyword that is not in the name.
        assert!(found("cheerful", "grinning face"));
        assert!(found("germany", "flag: Germany"));
        assert!(!found("rinning", "grinning face"));
        assert!(matches("zzzqqq").is_empty());
    }

    #[test]
    fn skin_tones() {
        let wave = all().find(|emoji| emoji.name == "waving hand").unwrap();
        assert_eq!(wave.in_tone(0), "👋");
        assert_eq!(wave.in_tone(1), "👋🏻");
        assert_eq!(wave.in_tone(5), "👋🏿");
        // Two people: each tone for both.
        let pair = all()
            .find(|emoji| emoji.name == "people holding hands")
            .unwrap();
        assert_eq!(pair.in_tone(3), "🧑🏽‍🤝‍🧑🏽");
        // Without tones, the emoji itself.
        let grinning = all().find(|emoji| emoji.emoji == "😀").unwrap();
        assert_eq!(grinning.in_tone(4), "😀");
    }

    /// ™ is text by default: only ™ with U+FE0F is the emoji.
    #[test]
    fn text_presentation_characters_need_the_emoji_selector() {
        let text_tm = "\u{2122}";
        let emoji_tm = "\u{2122}\u{FE0F}";
        assert_eq!(images(text_tm), Vec::<&str>::new());
        assert_eq!(images(emoji_tm), vec![emoji_tm]);
        assert_eq!(
            images(&format!("Nuschl{text_tm} and {emoji_tm}!")),
            vec![emoji_tm]
        );
        // The text selector keeps it text.
        assert!(images("\u{2122}\u{FE0E}").is_empty());
        for (text, emoji) in [("©", "©️"), ("®", "®️"), ("❤", "❤️"), ("☝", "☝️")]
        {
            assert!(images(text).is_empty(), "{text}");
            assert_eq!(images(emoji), vec![emoji]);
        }
        // The picker inserts the emoji form, which is drawn as an image.
        let tm = all().find(|emoji| emoji.name == "trade mark").unwrap();
        assert_eq!(tm.emoji, emoji_tm);
        assert!(image_of(tm.emoji).is_some());
    }

    #[test]
    fn emoji_presentation_characters_and_sequences_are_images() {
        assert_eq!(images("a 😀 b"), vec!["😀"]);
        // U+FE0E asks for text even where emoji is the default.
        assert!(images("😀\u{FE0E}").is_empty());
        assert_eq!(images("👍🏽👍"), vec!["👍🏽", "👍"]);
        assert_eq!(images("🇩🇪🇦🇹"), vec!["🇩🇪", "🇦🇹"]);
        assert_eq!(images("1️⃣ #️⃣"), vec!["1️⃣", "#️⃣"]);
        // ZWJ sequences are one image, even with a text-default part.
        assert_eq!(images("❤️‍🔥"), vec!["❤️‍🔥"]);
        assert_eq!(images("👩‍👩‍👧‍👦"), vec!["👩‍👩‍👧‍👦"]);
        assert_eq!(images("🧑🏻‍🤝‍🧑🏿"), vec!["🧑🏻‍🤝‍🧑🏿"]);
        // Plain text, digits and other symbols stay text.
        assert!(images("Version 2024 # * → ✓ ü ß 中文").is_empty());
    }

    #[test]
    fn every_image_is_a_small_static_svg() {
        for (key, _, svg) in assets::IMAGES {
            let text = std::str::from_utf8(svg).unwrap();
            assert!(text.starts_with("<svg") && svg.len() <= 64 * 1024, "{key}");
        }
        assert!(Twemoji.svg(assets::IMAGES.len() as u32).is_none());
    }

    #[test]
    fn images_render_at_the_requested_size() {
        let renderer = SvgRenderer::new(Arc::new(crate::assets::Assets));
        let id = image_of("😀").unwrap();
        let parsed = renderer.parse_svg(Twemoji.svg(id).unwrap()).unwrap();
        let edge = DevicePixels::from(32u32);
        let image = renderer
            .render_parsed(&parsed, SvgSize::ExactSize(size(edge, edge)))
            .unwrap();
        assert_eq!(image.size(0), size(edge, edge));
        // Twemoji's grinning face is yellow below its eyes (BGRA).
        let bytes = image.as_bytes(0).unwrap();
        let at = 4 * (12 * 32 + 6);
        let [blue, green, red, alpha] = <[u8; 4]>::try_from(&bytes[at..at + 4]).unwrap();
        assert!(
            alpha == 255 && red > 200 && green > 150 && blue < 120,
            "{red} {green} {blue} {alpha}"
        );
    }
}
