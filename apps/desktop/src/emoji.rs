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
use gpui_kit::{
    base::{
        Scrollbar, ScrollbarMode,
        input::{Input, InputEvent, InputState},
    },
    prelude::*,
    *,
};
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

/// How much of a search is looked at: a long paste searches by its start.
const MAX_QUERY_CHARS: usize = 64;

/// Without U+FE0F, which a typed or pasted emoji may or may not carry.
fn bare(text: &str) -> String {
    text.chars().filter(|&c| c != EMOJI_STYLE).collect()
}

/// Every emoji of the table, and each of its skin tones, by its bare text;
/// and the longest of those texts, in characters.
fn by_text() -> &'static (std::collections::HashMap<String, &'static Emoji>, usize) {
    static BY_TEXT: OnceLock<(std::collections::HashMap<String, &'static Emoji>, usize)> =
        OnceLock::new();
    BY_TEXT.get_or_init(|| {
        let mut map = std::collections::HashMap::new();
        for emoji in all() {
            let tones = emoji.tones.iter().flatten().filter(|tone| !tone.is_empty());
            for text in std::iter::once(&emoji.emoji).chain(tones) {
                map.entry(bare(text)).or_insert(emoji);
            }
        }
        let longest = map.keys().map(|key| key.chars().count()).max().unwrap_or(0);
        (map, longest)
    })
}

/// A search split into the emoji it contains (in order, once each) and its
/// other words, lower-cased.
fn parse_query(query: &str) -> (Vec<&'static Emoji>, Vec<String>) {
    let (map, longest) = by_text();
    let chars: Vec<char> = bare(query).chars().take(MAX_QUERY_CHARS).collect();
    let (mut found, mut text) = (Vec::<&'static Emoji>::new(), String::new());
    let mut at = 0;
    while at < chars.len() {
        // The longest emoji starting here: ❤️‍🔥 before ❤️.
        let emoji = (1..=(*longest).min(chars.len() - at))
            .rev()
            .find_map(|length| {
                map.get(&chars[at..at + length].iter().collect::<String>())
                    .map(|emoji| (*emoji, length))
            });
        match emoji {
            Some((emoji, length)) => {
                if !found.iter().any(|seen| std::ptr::eq(*seen, emoji)) {
                    found.push(emoji);
                }
                text.push(' ');
                at += length;
            }
            None => {
                text.push(chars[at]);
                at += 1;
            }
        }
    }
    let words = text.split_whitespace().map(str::to_lowercase).collect();
    (found, words)
}

/// What a search finds. Words: emoji with a word of their name, keywords or
/// short codes starting with each of them. An emoji in the search: itself,
/// then every emoji sharing any of its words (most shared first), still
/// narrowed by the typed words.
fn matches(query: &str) -> Vec<&'static Emoji> {
    let (found, words) = parse_query(query);
    if found.is_empty() {
        return all().filter(|emoji| emoji.matches(&words)).collect();
    }
    let wanted: std::collections::HashSet<&str> = found
        .iter()
        .flat_map(|emoji| emoji.search.split(' '))
        .filter(|word| !word.is_empty())
        .collect();
    let mut related: Vec<(usize, &'static Emoji)> = all()
        .filter(|emoji| !found.iter().any(|seen| std::ptr::eq(*seen, *emoji)))
        .filter(|emoji| emoji.matches(&words))
        .filter_map(|emoji| {
            let mut shared: Vec<&str> = emoji
                .search
                .split(' ')
                .filter(|word| wanted.contains(word))
                .collect();
            shared.sort_unstable();
            shared.dedup();
            (!shared.is_empty()).then_some((shared.len(), emoji))
        })
        .collect();
    // Stable: equally related emoji keep the table's order.
    related.sort_by(|a, b| b.0.cmp(&a.0));
    found
        .into_iter()
        .chain(related.into_iter().map(|(_, emoji)| emoji))
        .collect()
}

/// The category tabs, from the table: each group's name and its first emoji.
fn categories() -> impl Iterator<Item = (&'static str, &'static str)> {
    groups()
        .iter()
        .filter_map(|group| Some((group.name, group.emoji.first()?.emoji)))
}

/// Where a group's heading is among `rows` (none while searching).
fn heading_row(rows: &[Row], group: &str) -> Option<usize> {
    rows.iter()
        .position(|row| matches!(row, Row::Heading(name) if *name == group))
}

/// The group whose lines are at the top when line `top` is: its tab is the
/// current one. None above the first group (recently used) and while
/// searching.
fn current_category(rows: &[Row], top: usize) -> Option<&'static str> {
    rows[..(top + 1).min(rows.len())]
        .iter()
        .rev()
        .find_map(|row| match row {
            Row::Heading(name) => Some(*name),
            _ => None,
        })
        .filter(|name| groups().iter().any(|group| group.name == *name))
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

/// The open picker: its tab, its search field, and the lines it shows.
pub struct Picker {
    tab: Tab,
    /// A real text field: cursor, selection, clipboard and word editing.
    search: Entity<InputState>,
    rows: Vec<Row>,
    scroll: UniformListScrollHandle,
    /// The skin tone choices are shown in place of the search.
    choosing_tone: bool,
    _changes: Subscription,
}

/// The lines of the emoji tab for what is typed: everything by group
/// (recently used first), or what matches.
fn rows(query: &str, recent: &[&'static Emoji]) -> Vec<Row> {
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
        let found = matches(query);
        if found.is_empty() {
            rows.push(Row::Nothing);
        }
        add(&mut rows, found);
    }
    rows
}

impl Picker {
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
        let search = cx.new(|cx| {
            crate::state::accessible(
                InputState::new(window, cx).placeholder("Search emoji"),
                "Search emoji",
                false,
                false,
            )
        });
        search.update(cx, |input, cx| input.focus(window, cx));
        let changes = cx.subscribe_in(&search, window, |this, _, event, window, cx| {
            match event {
                InputEvent::Change => this.emoji_search_changed(cx),
                // Enter inserts the first emoji shown; Shift keeps the picker open.
                InputEvent::PressEnter { shift, .. } => {
                    match this.emoji.as_ref().and_then(Picker::first) {
                        Some(emoji) => this.pick_emoji(emoji, *shift, window, cx),
                        None => this.close_emoji(window, cx),
                    }
                }
                _ => return,
            }
            cx.notify();
        });
        self.emoji = Some(Picker {
            tab: Tab::Emoji,
            search,
            rows: rows("", &self.emoji_recent),
            scroll: UniformListScrollHandle::new(),
            choosing_tone: false,
            _changes: changes,
        });
    }

    /// The search changed: list what it finds, from the top.
    fn emoji_search_changed(&mut self, cx: &mut Context<Self>) {
        let Some(picker) = &mut self.emoji else {
            return;
        };
        let query = picker.search.read(cx).value();
        picker.rows = rows(&query, &self.emoji_recent);
        picker.scroll.scroll_to_item(0, ScrollStrategy::Top);
    }

    /// Show a category from its heading. A search hides the headings, so it
    /// is cleared first.
    fn show_emoji_category(
        &mut self,
        name: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(picker) = &mut self.emoji else {
            return;
        };
        if !picker.search.read(cx).value().is_empty() {
            let search = picker.search.clone();
            search.update(cx, |input, cx| input.set_value("", window, cx));
            self.emoji_search_changed(cx);
        }
        if let Some(picker) = &self.emoji
            && let Some(at) = heading_row(&picker.rows, name)
        {
            picker.scroll.scroll_to_item_strict(at, ScrollStrategy::Top);
        }
    }

    /// Keys go back to the search field, or, while the skin tones are shown,
    /// to the picker (Escape closes them).
    fn focus_emoji_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.emoji {
            Some(picker) if !picker.choosing_tone => {
                let search = picker.search.clone();
                search.update(cx, |input, cx| input.focus(window, cx));
            }
            Some(_) => window.focus(&self.emoji_focus, cx),
            None => {}
        }
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
            self.focus_emoji_search(window, cx);
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
                self.focus_emoji_search(window, cx);
                cx.stop_propagation();
                cx.notify();
                return;
            }
            "escape" => return self.close_emoji(window, cx),
            _ if shortcut(keystroke) => return self.close_emoji(window, cx),
            _ => {}
        }
        // The search field types, pastes and edits itself; a key that reaches
        // the picker (a tab or tone button had the focus) goes back to it.
        let typed = match &keystroke.key_char {
            Some(text) if !modifiers.control && !modifiers.alt && !modifiers.platform => text,
            _ => return,
        };
        if picker.tab != Tab::Emoji || picker.choosing_tone || keystroke.key == "enter" {
            return;
        }
        let search = picker.search.clone();
        let typed = typed.clone();
        search.update(cx, |input, cx| {
            input.focus(window, cx);
            input.insert(typed, window, cx);
        });
        self.emoji_search_changed(cx);
        cx.stop_propagation();
        cx.notify();
    }
}

/// One emoji of the grid. Its id is its place (line and column), not the
/// emoji: an emoji can be shown twice (recently used and in its group), and
/// two elements with one id share hover, tooltip and click state, so neither
/// would show its tooltip or take a click.
fn cell(emoji: &'static Emoji, tone: usize, place: usize, cx: &mut Context<Desktop>) -> AnyElement {
    let shown = emoji.in_tone(tone);
    let name = emoji.name;
    div()
        .id(("emoji-cell", place))
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

fn row(line: usize, row: &Row, tone: usize, cx: &mut Context<Desktop>) -> AnyElement {
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
            .children(
                emoji
                    .iter()
                    .enumerate()
                    .map(|(column, emoji)| cell(emoji, tone, line * COLUMNS + column, cx)),
            )
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
    // The button is replaced by what it opens or chooses, so the focus
    // moves on: to the search (typing goes on searching), or to the picker
    // while the tones are shown.
    let action = std::rc::Rc::new(
        move |this: &mut Desktop, window: &mut Window, cx: &mut Context<Desktop>| {
            action(this);
            this.focus_emoji_search(window, cx);
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
    let search = div()
        .id("emoji-search")
        .flex_1()
        .min_w_0()
        .child(Input::new(&picker.search));
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
                .py(px(6.))
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

/// The emoji, a line at a time: only the visible lines are built.
fn grid(picker: &Picker, cx: &mut Context<Desktop>) -> AnyElement {
    uniform_list(
        "emoji-grid",
        picker.rows.len(),
        cx.processor(|this, range: Range<usize>, _, cx| {
            let Some(picker) = &this.emoji else {
                return Vec::new();
            };
            let tone = this.emoji_tone;
            let rows = &picker.rows;
            let start = range.start.min(rows.len());
            rows[start..range.end.min(rows.len())]
                .iter()
                .enumerate()
                .map(|(offset, line)| row(start + offset, line, tone, cx))
                .collect()
        }),
    )
    .track_scroll(&picker.scroll)
    .size_full()
    .px_2()
    .pt(px(3.))
    .pb_2()
    .into_any_element()
}

/// The line at the top of the grid (all lines are one cell high), or the
/// one a jump is about to bring there.
fn top_row(scroll: &UniformListScrollHandle) -> usize {
    let state = scroll.0.borrow();
    match &state.deferred_scroll_to_item {
        Some(deferred) => deferred.item_index,
        None => (-f32::from(state.base_handle.offset().y) / CELL + 0.5).max(0.) as usize,
    }
}

/// A tab per category, drawn with its first emoji; a click shows the whole
/// category from its heading (and leaves the search).
fn category_tabs(picker: &Picker, cx: &mut Context<Desktop>) -> AnyElement {
    let current = current_category(&picker.rows, top_row(&picker.scroll));
    div()
        .id("emoji-categories")
        .role(Role::TabList)
        .aria_label("Categories")
        .mx_2()
        .flex()
        // The tabs are narrower than the picker: the space is on both sides.
        .justify_center()
        .children(categories().map(|(name, icon)| {
            let selected = current == Some(name);
            div()
                .id(SharedString::from(format!("emoji-category-{name}")))
                .role(Role::Tab)
                .aria_label(name)
                .aria_selected(selected)
                .tab_index(0)
                .size(px(CELL))
                .flex()
                .items_center()
                .justify_center()
                .rounded_t(px(6.))
                // A tab: the selected one is underlined, on the line that
                // separates the tabs from the emoji.
                .border_b_2()
                .border_color(if selected {
                    Hsla::from(color(ACCENT))
                } else {
                    transparent_black()
                })
                .cursor_pointer()
                .hover(|style| style.bg(color(HOVER)))
                .tooltip(move |_, cx| cx.new(|_| crate::dialogs::Tooltip(name.into())).into())
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.show_emoji_category(name, window, cx);
                    cx.notify();
                }))
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        this.show_emoji_category(name, window, cx);
                        cx.stop_propagation();
                        cx.notify();
                    }
                }))
                .children(image_of(icon).map(|id| image(id, 22.)))
        }))
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
            .child(category_tabs(picker, cx))
            .child(
                // The emoji to click, on the message area's colour (white in
                // the light theme), apart from the tabs above them.
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .bg(color(MAIN))
                    .border_t_1()
                    .border_color(color(BORDER_STRONG))
                    // The current category follows the scrolling.
                    .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.notify()))
                    .child(grid(picker, cx))
                    .child(
                        div().absolute().inset_0().child(
                            Scrollbar::vertical(&picker.scroll)
                                .id("emoji-scrollbar")
                                .mode(ScrollbarMode::Always)
                                .styles(|styles| {
                                    styles
                                        .track(|track| track.width(px(8.)).bg(color(MAIN).into()))
                                        .thumb(|thumb| {
                                            thumb
                                                .width(px(4.))
                                                .bg(color(BORDER_STRONG))
                                                .radius(px(2.))
                                        })
                                        .thumb_hover(|thumb| thumb.bg(color(TEXT_FAINT)))
                                        .thumb_active(|thumb| thumb.bg(color(TEXT_MUTED)))
                                }),
                        ),
                    ),
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
    use super::{
        Row, TONES, Twemoji, all, assets, bare, categories, current_category, find_images, groups,
        heading_row, image_of, matches, rows,
    };
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
    fn an_emoji_in_the_search_finds_itself_then_those_sharing_its_words() {
        // Compared without U+FE0F: the table has the fully qualified form.
        let shown = |query: &str| -> Vec<String> {
            matches(query)
                .iter()
                .map(|emoji| bare(emoji.emoji))
                .collect()
        };
        let has = |found: &[String], emoji: &str| found.contains(&bare(emoji));
        for query in ["❤️", "❤", " ❤️ ", "❤️\u{FE0F}"] {
            let found = shown(query);
            assert_eq!(found.first(), Some(&bare("❤️")), "{query:?}: {found:?}");
            for heart in ["🧡", "💛", "💚", "💙", "💜", "🖤", "🤍", "💔", "😍"] {
                assert!(has(&found, heart), "{query:?} misses {heart}: {found:?}");
            }
            assert_eq!(
                found.iter().filter(|emoji| **emoji == bare("❤️")).count(),
                1
            );
        }
        // A skin tone finds the emoji it is a tone of.
        assert_eq!(shown("👍🏽").first(), Some(&bare("👍")));
        // Two emoji: both, then what shares a word with either.
        let both = shown("❤️ 👍");
        assert_eq!(both[..2], [bare("❤️"), bare("👍")]);
        assert!(has(&both, "🧡") && has(&both, "👎"), "{both:?}");
        // Words narrow what an emoji finds.
        let orange = shown("❤️ orange");
        assert!(has(&orange, "🧡") && !has(&orange, "💚"), "{orange:?}");
        // Text that is not an emoji searches as before; a long paste is bounded.
        assert!(shown("→xyzzy").is_empty() && shown("xyzzy").is_empty());
        // An emoji typed is always shown, even with words that match nothing.
        assert_eq!(shown("☃xyzzy"), [bare("☃️")]);
        assert_eq!(shown(&"❤️".repeat(10_000)).first(), Some(&bare("❤️")));
    }

    #[test]
    fn category_tabs_come_from_the_table_and_jump_to_their_heading() {
        let icons: Vec<String> = categories().map(|(_, icon)| bare(icon)).collect();
        let expected = ["😀", "👋", "🐵", "🍇", "🌍", "🎃", "👓", "🏧", "🏁"];
        assert_eq!(icons, expected.map(bare));
        assert_eq!(categories().count(), groups().len());
        let recent = [&groups()[3].emoji[0], &groups()[0].emoji[5]];
        for listed in [rows("", &[]), rows("", &recent)] {
            for (name, icon) in categories() {
                let at = heading_row(&listed, name).expect(name);
                match (&listed[at], &listed[at + 1]) {
                    (Row::Heading(heading), Row::Emoji(first)) => {
                        assert_eq!(*heading, name);
                        assert_eq!(first[0].emoji, icon);
                    }
                    _ => panic!("{name} is not a heading followed by emoji"),
                }
                // Anywhere in a group, its tab is the current one.
                assert_eq!(current_category(&listed, at), Some(name));
                assert_eq!(current_category(&listed, at + 2), Some(name));
            }
        }
        // Above the first group (recently used), no tab is current.
        assert_eq!(current_category(&rows("", &recent), 0), None);
        // While searching there are no headings to jump to.
        assert_eq!(heading_row(&rows("heart", &[]), groups()[0].name), None);
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
