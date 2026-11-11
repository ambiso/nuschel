// SPDX-License-Identifier: EUPL-1.2
//! A text field: the composer and the fields of the switcher, search and
//! prompts. Emacs-style keys as in a shell (Ctrl+A, Ctrl+E, Ctrl+W, Ctrl+U,
//! Ctrl+K), arrows, Home and End.
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Input {
    text: String,
    /// A byte offset at a character boundary.
    cursor: usize,
}

impl Input {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn set(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    /// The columns before the cursor on its line, and the line it is on.
    pub fn cursor_position(&self) -> (usize, usize) {
        let before = &self.text[..self.cursor];
        let line = before.matches('\n').count();
        let start = before.rfind('\n').map_or(0, |at| at + 1);
        (before[start..].width(), line)
    }

    pub fn insert(&mut self, text: &str) {
        // Pasted control characters other than line breaks are dropped.
        let text: String = text
            .replace("\r\n", "\n")
            .chars()
            .map(|c| if c == '\r' { '\n' } else { c })
            .filter(|c| *c == '\n' || !c.is_control())
            .collect();
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
    }

    fn previous(&self) -> usize {
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(at, _)| at)
    }

    fn next(&self) -> usize {
        self.text[self.cursor..]
            .chars()
            .next()
            .map_or(self.cursor, |c| self.cursor + c.len_utf8())
    }

    /// The start of the word before the cursor.
    fn word_start(&self) -> usize {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end();
        trimmed.rfind(char::is_whitespace).map_or(0, |at| {
            at + trimmed[at..].chars().next().map_or(1, char::len_utf8)
        })
    }

    /// Apply an editing key; returns whether it was one (and the text or
    /// cursor may have changed). Enter is the caller's.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char(c) if !control && !alt => {
                let mut buffer = [0; 4];
                self.insert(c.encode_utf8(&mut buffer));
            }
            KeyCode::Char('a') if control => self.cursor = 0,
            KeyCode::Char('e') if control => self.cursor = self.text.len(),
            KeyCode::Char('u') if control => {
                self.text.replace_range(..self.cursor, "");
                self.cursor = 0;
            }
            KeyCode::Char('k') if control => self.text.truncate(self.cursor),
            KeyCode::Char('w') if control => {
                let start = self.word_start();
                self.text.replace_range(start..self.cursor, "");
                self.cursor = start;
            }
            KeyCode::Char('b') if alt => self.cursor = self.word_start(),
            KeyCode::Backspace if alt || control => {
                let start = self.word_start();
                self.text.replace_range(start..self.cursor, "");
                self.cursor = start;
            }
            KeyCode::Backspace => {
                let start = self.previous();
                self.text.replace_range(start..self.cursor, "");
                self.cursor = start;
            }
            KeyCode::Delete => {
                let end = self.next();
                self.text.replace_range(self.cursor..end, "");
            }
            KeyCode::Left => self.cursor = self.previous(),
            KeyCode::Right => self.cursor = self.next(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.text.len(),
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(input: &mut Input, code: KeyCode, modifiers: KeyModifiers) {
        input.key(KeyEvent::new(code, modifiers));
    }

    #[test]
    fn editing_keys_work_on_characters_not_bytes() {
        let mut input = Input::default();
        for c in "Grüße Welt".chars() {
            press(&mut input, KeyCode::Char(c), KeyModifiers::NONE);
        }
        press(&mut input, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(input.text(), "Grüße ");
        press(&mut input, KeyCode::Backspace, KeyModifiers::NONE);
        press(&mut input, KeyCode::Left, KeyModifiers::NONE);
        press(&mut input, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(input.text(), "Grüe");
        assert_eq!(input.cursor_position(), (3, 0));
        press(&mut input, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(input.text(), "e");
        input.insert("a\u{1b}[2J\r\nb");
        assert_eq!(input.text(), "a[2J\nbe");
        assert_eq!(input.cursor_position(), (1, 1));
    }
}
