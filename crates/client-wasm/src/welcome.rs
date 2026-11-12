// SPDX-License-Identifier: EUPL-1.2
//! SDK-worker-owned welcome documents, never HTML for the DOM to interpret.

use nuschl_content::ContentRenderer;
use nuschl_core::{ContentBlock, MessageDocument};
use serde::Serialize;
use std::collections::VecDeque;

const MAX_ENTRIES: usize = 16;
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_DOCUMENT_BYTES: usize = 256 * 1024;

#[derive(Clone, Serialize)]
pub struct View {
    pub token: String,
    pub document: MessageDocument,
}

struct Entry {
    id: String,
    generation: u64,
    source: String,
    view: View,
    dismissed: bool,
    published: bool,
    bytes: usize,
}

#[derive(Default)]
pub struct Welcomes {
    entries: VecDeque<Entry>,
    serial: u64,
    bytes: usize,
}

impl Welcomes {
    pub fn render(
        &mut self,
        id: &str,
        generation: u64,
        source: &str,
        renderer: &ContentRenderer,
    ) -> MessageDocument {
        if let Some(entry) = self
            .entries
            .iter()
            .find(|e| e.id == id && e.generation == generation && e.source == source)
        {
            return entry.view.document.clone();
        }
        self.remove(id);
        if source.len() > nuschl_core::MAX_CONTENT_BYTES {
            return fallback("[Server welcome exceeds the display limit]");
        }
        if source.is_empty() {
            return MessageDocument { blocks: Vec::new() };
        }
        let mut document = renderer
            .legacy_plain_text(source)
            .and_then(|text| renderer.render(&text))
            .unwrap_or_else(|_| fallback("[Server welcome could not be rendered]"));
        let mut size = serde_json::to_vec(&document).map_or(usize::MAX, |v| v.len());
        if size > MAX_DOCUMENT_BYTES {
            document = fallback("[Server welcome exceeds the display limit]");
            size = serde_json::to_vec(&document).map_or(0, |v| v.len());
        }
        let bytes = source.len().saturating_add(size);
        // Session bounds sources. Keep this boundary independently safe as well.
        if source.len() > nuschl_core::MAX_CONTENT_BYTES || bytes > MAX_BYTES {
            return document;
        }
        while self.entries.len() >= MAX_ENTRIES || self.bytes + bytes > MAX_BYTES {
            if let Some(old) = self.entries.pop_front() {
                self.bytes -= old.bytes;
            } else {
                break;
            }
        }
        self.serial = self.serial.checked_add(1).expect("welcome token exhausted");
        self.entries.push_back(Entry {
            id: id.into(),
            generation,
            source: source.into(),
            view: View {
                token: self.serial.to_string(),
                document: document.clone(),
            },
            dismissed: false,
            published: false,
            bytes,
        });
        self.bytes += bytes;
        document
    }

    pub fn view(
        &self,
        id: &str,
        generation: u64,
        source: Option<&str>,
        connected: bool,
    ) -> Option<&View> {
        self.entries
            .iter()
            .find(|e| {
                connected
                    && !e.dismissed
                    && e.id == id
                    && e.generation == generation
                    && Some(e.source.as_str()) == source
            })
            .map(|e| &e.view)
    }

    /// Documents cross the Worker boundary once, not on every server snapshot.
    pub fn take_view(
        &mut self,
        id: &str,
        generation: u64,
        source: Option<&str>,
        connected: bool,
    ) -> Option<View> {
        let entry = self.entries.iter_mut().find(|e| {
            connected
                && !e.dismissed
                && !e.published
                && e.id == id
                && e.generation == generation
                && Some(e.source.as_str()) == source
        })?;
        entry.published = true;
        Some(entry.view.clone())
    }

    pub fn dismiss(
        &mut self,
        id: &str,
        generation: u64,
        source: Option<&str>,
        token: &str,
    ) -> bool {
        let Some(entry) = self.entries.iter_mut().find(|e| {
            e.id == id
                && e.generation == generation
                && Some(e.source.as_str()) == source
                && e.view.token == token
        }) else {
            return false;
        };
        entry.dismissed = true;
        true
    }

    pub fn remove(&mut self, id: &str) {
        self.entries.retain(|e| {
            if e.id == id {
                self.bytes -= e.bytes;
                false
            } else {
                true
            }
        });
    }
}

fn fallback(text: &str) -> MessageDocument {
    MessageDocument {
        blocks: vec![ContentBlock::text(text)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_lease_dismissal_reconnect_and_replacement() {
        let mut cache = Welcomes::default();
        let renderer = ContentRenderer::default();
        let source = "<b>something.de</b>\n&lt;em&gt;literal&lt;/em&gt;";
        cache.render("one", 1, source, &renderer);
        let first = cache.view("one", 1, Some(source), true).unwrap().clone();
        assert!(
            serde_json::to_string(&first)
                .unwrap()
                .contains("something.de")
        );
        assert!(cache.view("one", 2, Some(source), true).is_none());
        assert!(cache.view("one", 1, Some("other"), true).is_none());
        assert!(cache.view("one", 1, Some(source), false).is_none());
        assert!(!cache.dismiss("one", 1, Some(source), "wrong"));
        assert!(cache.dismiss("one", 1, Some(source), &first.token));
        cache.render("one", 1, source, &renderer);
        assert!(cache.view("one", 1, Some(source), true).is_none());
        cache.render("one", 2, source, &renderer);
        let second = cache.view("one", 2, Some(source), true).unwrap().clone();
        assert_ne!(first.token, second.token);
        assert!(cache.take_view("one", 2, Some(source), true).is_some());
        assert!(cache.take_view("one", 2, Some(source), true).is_none());
        assert!(!cache.dismiss("one", 2, Some(source), &first.token));
        cache.render("one", 2, "changed", &renderer);
        assert!(cache.view("one", 2, Some(source), true).is_none());
        cache.remove("one");
        assert_eq!(cache.bytes, 0);
    }

    #[test]
    fn aggregate_and_rendered_expansion_stay_bounded() {
        let mut cache = Welcomes::default();
        let renderer = ContentRenderer::default();
        for i in 0..40 {
            cache.render(&i.to_string(), i, &"a".repeat(180_000), &renderer);
        }
        assert!(cache.entries.len() <= MAX_ENTRIES);
        assert!(cache.bytes <= MAX_BYTES);
        cache.render("expanded", 1, &"<b>a</b>".repeat(20_000), &renderer);
        let entry = cache.entries.back().unwrap();
        assert!(serde_json::to_vec(&entry.view.document).unwrap().len() <= MAX_DOCUMENT_BYTES);
        cache.render("expanded", 1, "", &renderer);
        assert!(cache.entries.iter().all(|e| e.id != "expanded"));
    }
}
