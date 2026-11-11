// SPDX-License-Identifier: EUPL-1.2
//! The keyboard in the focused timeline beyond moving between entries
//! (view.rs, `timeline_key`): the links inside the current message, and
//! typing that belongs in the composer.
//!
//! Tab steps through the current message's links (Shift+Tab back) and on
//! past the last one to the next control; Enter opens the link the keyboard
//! is on, or a message's only link, through the same `open_link` a click
//! uses (allowed schemes only; a `mumble://` link to a server that is not
//! saved opens the form to confirm it). A line under the timeline names the
//! link and where it really leads, also for screen readers. A printable key
//! moves to the composer and is typed there.
use crate::{
    document::{Block, Document},
    scrolling::Key,
    selection::HoveredLink,
    state::Desktop,
};
use gpui_kit::*;
use nuschl_client::timeline::Message;

/// A link in a message: its block, its index in that block, its text and
/// where it leads.
pub struct MessageLink {
    pub block: usize,
    pub link: usize,
    pub text: SharedString,
    pub url: SharedString,
}

/// The links of a message, in reading order.
pub fn message_links(message: &Message, document: &Document) -> Vec<MessageLink> {
    let mut found = Vec::new();
    if message.deleted.is_some() {
        return found;
    }
    for (block, content) in document.blocks.iter().enumerate() {
        if let Block::Text { text, links, .. } = content {
            for (link, (range, url)) in links.iter().enumerate() {
                found.push(MessageLink {
                    block,
                    link,
                    text: text
                        .get(range.clone())
                        .unwrap_or_default()
                        .to_string()
                        .into(),
                    url: url.clone(),
                });
            }
        }
    }
    found
}

/// What a key typed, if it is text for the composer: a printable character
/// without Ctrl, Cmd or Fn (Space activates the entry instead).
pub fn typed_text(keystroke: &Keystroke) -> Option<&str> {
    let modifiers = &keystroke.modifiers;
    if modifiers.control || modifiers.platform || modifiers.function {
        return None;
    }
    keystroke
        .key_char
        .as_deref()
        .filter(|text| !text.is_empty() && *text != " " && !text.chars().any(char::is_control))
}

impl Desktop {
    /// The links of a message of the open timeline, as it is drawn.
    fn links_of(&self, message: &Message) -> Vec<MessageLink> {
        message_links(message, &self.presentation.message(message.serial).document)
    }

    /// The message the keyboard is on in the open timeline, if it is one.
    fn keyboard_message(&self) -> Option<&Message> {
        let Some(Key::Message(serial)) = self.timeline_current.get() else {
            return None;
        };
        let (server, channel) = self.selected_indices()?;
        self.client.servers[server].channels[channel]
            .timeline
            .messages
            .iter()
            .find(|message| message.serial == serial)
    }

    /// The link the keyboard is on, if it is still in the current message.
    fn keyboard_link(&self) -> Option<(u64, usize)> {
        let (serial, index) = self.link_focus?;
        (self.keyboard_message()?.serial == serial).then_some((serial, index))
    }

    /// Tab (or Shift+Tab) in the timeline: through the current message's
    /// links. False lets it move focus on as usual.
    pub fn timeline_tab(&mut self, backwards: bool) -> bool {
        let Some(message) = self.keyboard_message() else {
            self.link_focus = None;
            return false;
        };
        let (serial, count) = (message.serial, self.links_of(message).len());
        let next = match (self.keyboard_link().map(|(_, index)| index), backwards) {
            (None, false) if count > 0 => Some(0),
            (Some(index), false) if index + 1 < count => Some(index + 1),
            (Some(index), true) if index > 0 => Some(index - 1),
            // Back from the first link to the message itself.
            (Some(_), true) => {
                self.focus_link(None);
                return true;
            }
            _ => None,
        };
        match next {
            Some(index) => {
                self.focus_link(Some((serial, index)));
                true
            }
            None => {
                self.focus_link(None);
                false
            }
        }
    }

    /// Mark the keyboard's link (underlined, as under the pointer).
    pub(crate) fn focus_link(&mut self, link: Option<(u64, usize)>) {
        let previous = self.link_focus.take();
        if previous.is_some()
            && self
                .text
                .hovered
                .as_ref()
                .is_some_and(|hovered| Some(hovered.message) == previous.map(|(serial, _)| serial))
        {
            self.text.hovered = None;
        }
        let Some((serial, index)) = link else {
            return;
        };
        let Some(found) = self
            .keyboard_message()
            .and_then(|message| self.links_of(message).into_iter().nth(index))
        else {
            return;
        };
        self.link_focus = Some((serial, index));
        self.text.hovered = Some(HoveredLink {
            message: serial,
            block: found.block,
            link: found.link,
            url: found.url,
            position: Point::default(),
            // Where it leads is shown under the timeline instead (link_status).
            deceptive: false,
        });
    }

    /// Enter in the timeline: open the link the keyboard is on, or the
    /// current message's only link. False when there is none.
    pub fn open_keyboard_link(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(message) = self.keyboard_message() else {
            return false;
        };
        let links = self.links_of(message);
        let index = match self.keyboard_link() {
            Some((_, index)) => index,
            None if links.len() == 1 => 0,
            None => return false,
        };
        let Some(link) = links.into_iter().nth(index) else {
            return false;
        };
        self.open_link(&link.url, window, cx);
        true
    }

    /// The line under the timeline while the keyboard is on a link: which
    /// one, its text and where it leads.
    pub fn link_status(&self) -> Option<String> {
        let (_, index) = self.keyboard_link()?;
        let links = self.links_of(self.keyboard_message()?);
        let link = links.get(index)?;
        let text = if link.text.trim().is_empty() || link.text == link.url {
            String::new()
        } else {
            format!("{} → ", link.text.trim())
        };
        Some(format!(
            "Link {} of {}: {text}{} · Enter opens it",
            index + 1,
            links.len(),
            link.url
        ))
    }

    /// A printable key in the timeline: write it in the composer instead.
    pub fn type_into_composer(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(text) = typed_text(keystroke) else {
            return false;
        };
        let text = text.to_owned();
        self.focus_link(None);
        self.composer.update(cx, |input, cx| {
            input.focus(window, cx);
            input.replace_text_in_range(None, &text, window, cx);
        });
        true
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that glob includes GPUI's `test` macro, which shadows `#[test]`.
    use super::typed_text;
    use gpui_kit::{Keystroke, Modifiers};

    fn key(key: &str, key_char: Option<&str>, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            modifiers,
            key: key.into(),
            key_char: key_char.map(Into::into),
        }
    }

    #[test]
    fn printable_keys_go_to_the_composer_and_shortcuts_do_not() {
        let plain = Modifiers::default();
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        let control = Modifiers {
            control: true,
            ..Default::default()
        };
        assert_eq!(typed_text(&key("a", Some("a"), plain)), Some("a"));
        assert_eq!(typed_text(&key("a", Some("A"), shift)), Some("A"));
        assert_eq!(typed_text(&key("ü", Some("ü"), plain)), Some("ü"));
        assert_eq!(typed_text(&key("c", None, control)), None);
        assert_eq!(typed_text(&key("space", Some(" "), plain)), None);
        assert_eq!(typed_text(&key("enter", Some("\n"), plain)), None);
        assert_eq!(typed_text(&key("up", None, plain)), None);
    }
}
