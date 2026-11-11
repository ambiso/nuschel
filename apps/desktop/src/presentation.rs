// SPDX-License-Identifier: EUPL-1.2
//! What the desktop derives from the SDK's messages to draw them: the
//! renderer-ready document (styled text, images for the GPU atlas), the local
//! time, the height the row had when last laid out, and whether a long
//! message is expanded. The SDK keeps the messages themselves
//! (`nuschl_client::timeline`); this keeps what only drawing needs, by
//! message serial, built when a conversation is shown and rebuilt when a
//! message's revision changes (an edit, a deletion).
use crate::document::Document;
use nuschl_client::{conversation::ActivityLine, timeline::Message};
use std::{cell::Cell, collections::HashMap, rc::Rc};

/// Messages taller than this are collapsed until expanded.
pub const COLLAPSED_HEIGHT: f32 = 300.;

#[derive(Default)]
pub struct MessageUi {
    revision: u32,
    pub document: Document,
    /// When it was seen, labelled relative to the day it was first shown.
    pub time: String,
    /// Laid-out content height from the last frame, so "Show more" follows
    /// the real wrapped height (window width, images, code) rather than a
    /// character count.
    pub measured: Rc<Cell<f32>>,
    /// The row's height when last laid out, for the room it keeps while
    /// scrolled out of view.
    pub height: crate::scrolling::Height,
    pub expanded: bool,
}

impl MessageUi {
    fn new(message: &Message) -> Self {
        Self {
            revision: message.revision,
            document: message.document.clone().into(),
            time: crate::state::local_time(message.observed_at_ms),
            ..Default::default()
        }
    }

    pub fn has_more(&self) -> bool {
        self.measured.get() > COLLAPSED_HEIGHT + 1.
    }
}

/// Heights of an activity line's row, and of the run (collapsed line or
/// header) starting at it, when last laid out.
#[derive(Default)]
pub struct ActivityUi {
    pub height: crate::scrolling::Height,
    pub run_height: crate::scrolling::Height,
}

/// Presentations of the messages in the timelines that were shown.
#[derive(Default)]
pub struct Presentation {
    messages: HashMap<u64, MessageUi>,
    activity: HashMap<u64, ActivityUi>,
    /// What a message or line not presented yet looks like (empty).
    missing: MessageUi,
    missing_line: ActivityUi,
    /// Size at which presentations of messages no timeline holds any more
    /// are dropped.
    prune_at: usize,
}

impl Presentation {
    /// The presentation of `message`; empty until its timeline was shown.
    pub fn message(&self, serial: u64) -> &MessageUi {
        self.messages.get(&serial).unwrap_or(&self.missing)
    }

    /// The heights of activity line `serial`; empty until its timeline was shown.
    pub fn activity(&self, serial: u64) -> &ActivityUi {
        self.activity.get(&serial).unwrap_or(&self.missing_line)
    }

    pub fn message_mut(&mut self, serial: u64) -> Option<&mut MessageUi> {
        self.messages.get_mut(&serial)
    }

    /// Present every message of a timeline about to be shown; a message
    /// whose content changed gets a new document (a deleted one collapses).
    pub fn present<'a>(
        &mut self,
        messages: &[Message],
        activity: impl IntoIterator<Item = &'a ActivityLine>,
    ) {
        for line in activity {
            self.activity.entry(line.serial).or_default();
        }
        for message in messages {
            let ui = self
                .messages
                .entry(message.serial)
                .or_insert_with(|| MessageUi::new(message));
            if ui.revision != message.revision {
                ui.revision = message.revision;
                ui.document = message.document.clone().into();
                if message.deleted.is_some() {
                    ui.expanded = false;
                }
            }
        }
    }

    /// Whether presentations of gone messages should be looked for now.
    pub fn wants_pruning(&self) -> bool {
        self.messages.len() + self.activity.len() > self.prune_at.max(4096)
    }

    /// Keep only the presentations of messages and lines (by serial) still
    /// in a timeline.
    pub fn prune(&mut self, live: impl IntoIterator<Item = u64>) {
        let live: std::collections::HashSet<u64> = live.into_iter().collect();
        self.messages.retain(|serial, _| live.contains(serial));
        self.activity.retain(|serial, _| live.contains(serial));
        self.prune_at = (self.messages.len() + self.activity.len()) * 2;
    }
}
