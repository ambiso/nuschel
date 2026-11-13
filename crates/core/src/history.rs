// SPDX-License-Identifier: EUPL-1.2
//! Atomic history replacements. Callers validate input bounds and enforce
//! tombstone dominance before comparing these snapshots; they carry no deletion
//! state and do not themselves authorize an edit.

use crate::preview::Preview;

/// One replacement of a message's literal body and carried preview.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEdit {
    pub body: String,
    pub preview: Option<Preview>,
    pub edited_ms: i64,
    /// The server's committed edit count, or unknown for a legacy source.
    pub edit_revision: Option<u32>,
}

impl HistoryEdit {
    pub fn snapshot(&self) -> HistorySnapshot<'_> {
        HistorySnapshot {
            body: &self.body,
            preview: self.preview.as_ref(),
            edited_ms: Some(self.edited_ms),
            edit_revision: self.edit_revision,
        }
    }
}

/// A borrowed view of an initial message, edit, or stored history row.
#[derive(Clone, Copy, Debug)]
pub struct HistorySnapshot<'a> {
    pub body: &'a str,
    pub preview: Option<&'a Preview>,
    pub edited_ms: Option<i64>,
    /// `Some(0)` is an authoritative unedited message; `None` is unknown.
    pub edit_revision: Option<u32>,
}

/// Whether incoming content may replace current content as one atomic value.
///
/// Known revisions take precedence over timestamps. Equal known revisions only
/// confirm identical body and preview data. An unknown revision never replaces a
/// known one. When adopting the first known revision, a conflicting payload must
/// have a strictly newer timestamp; an identical payload may keep its timestamp.
/// Two unknown revisions retain the legacy timestamp ordering, including its
/// inability to distinguish different edits in the same millisecond.
///
/// Callers must validate bounds and reject replacement of a tombstone first.
/// Comparison borrows both payloads and copies neither body nor image bytes.
pub fn history_replacement_allowed(
    current: HistorySnapshot<'_>,
    incoming: HistorySnapshot<'_>,
) -> bool {
    let identical = || current.body == incoming.body && current.preview == incoming.preview;
    match (current.edit_revision, incoming.edit_revision) {
        (Some(current), Some(incoming)) => {
            incoming > current || (incoming == current && identical())
        }
        (Some(_), None) => false,
        (None, Some(_)) => {
            incoming.edited_ms > current.edited_ms
                || (incoming.edited_ms == current.edited_ms && identical())
        }
        (None, None) => incoming.edited_ms >= current.edited_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview::{EmbeddedImage, ImageMime, ProviderId, VERSION};

    fn preview() -> Preview {
        let value = Preview {
            version: VERSION,
            provider: ProviderId::OpenGraphV1,
            url: "https://example.org/".into(),
            title: "A title".into(),
            description: "A description".into(),
            image: Some(EmbeddedImage {
                mime: ImageMime::Png,
                // Opaque carried bytes; no image decoding occurs here.
                data: vec![1, 2, 3],
                width: 1,
                height: 1,
                alt: "An image".into(),
            }),
        };
        value.validate().unwrap();
        value
    }

    #[test]
    fn edit_snapshot_preserves_literal_content_and_unknown_revision() {
        let edit = HistoryEdit {
            body: "literal\nhttps://example.org/".into(),
            preview: Some(preview()),
            edited_ms: 7,
            edit_revision: None,
        };
        let snapshot = edit.snapshot();
        assert_eq!(snapshot.body, "literal\nhttps://example.org/");
        assert_eq!(snapshot.preview, edit.preview.as_ref());
        assert_eq!(snapshot.edited_ms, Some(7));
        assert_eq!(snapshot.edit_revision, None);
    }

    #[test]
    fn known_revisions_order_equal_millisecond_edits_in_both_directions() {
        let old_card = preview();
        let mut new_card = preview();
        new_card.title = "New title".into();
        let old = HistorySnapshot {
            body: "old body",
            preview: Some(&old_card),
            edited_ms: Some(10),
            edit_revision: Some(1),
        };
        let new = HistorySnapshot {
            body: "new body",
            preview: Some(&new_card),
            edit_revision: Some(2),
            ..old
        };
        assert!(history_replacement_allowed(old, new));
        assert!(!history_replacement_allowed(new, old));
        // Authoritative counts, rather than clocks, determine known ordering.
        assert!(history_replacement_allowed(
            old,
            HistorySnapshot {
                edited_ms: Some(9),
                ..new
            }
        ));
    }

    #[test]
    fn equal_known_revision_requires_exact_body_and_carried_preview() {
        let card = preview();
        let mut different = preview();
        different.image.as_mut().unwrap().data[0] = 4;
        let current = HistorySnapshot {
            body: "body",
            preview: Some(&card),
            edited_ms: Some(10),
            edit_revision: Some(2),
        };
        assert!(history_replacement_allowed(current, current));
        for incoming in [
            HistorySnapshot {
                body: "other body",
                ..current
            },
            HistorySnapshot {
                preview: None,
                ..current
            },
            HistorySnapshot {
                preview: Some(&different),
                ..current
            },
        ] {
            assert!(!history_replacement_allowed(current, incoming));
            assert!(!history_replacement_allowed(
                current,
                HistorySnapshot {
                    edited_ms: Some(11),
                    ..incoming
                }
            ));
        }
    }

    #[test]
    fn known_zero_is_distinct_from_unknown_and_cannot_be_downgraded() {
        let card = preview();
        let known = HistorySnapshot {
            body: "body",
            preview: Some(&card),
            edited_ms: None,
            edit_revision: Some(0),
        };
        let unknown = HistorySnapshot {
            edit_revision: None,
            ..known
        };
        assert!(history_replacement_allowed(unknown, known));
        assert!(!history_replacement_allowed(known, unknown));
        assert!(!history_replacement_allowed(
            known,
            HistorySnapshot {
                edited_ms: Some(99),
                ..unknown
            }
        ));
    }

    #[test]
    fn adopting_a_known_revision_protects_legacy_timestamp_and_payload() {
        let card = preview();
        let old = HistorySnapshot {
            body: "body",
            preview: Some(&card),
            edited_ms: Some(10),
            edit_revision: None,
        };
        for (timestamp, identical, expected) in [
            (None, true, false),
            (Some(9), true, false),
            (Some(10), true, true),
            (Some(10), false, false),
            (Some(11), false, true),
        ] {
            let incoming = HistorySnapshot {
                preview: if identical { Some(&card) } else { None },
                edited_ms: timestamp,
                edit_revision: Some(2),
                ..old
            };
            assert_eq!(
                history_replacement_allowed(old, incoming),
                expected,
                "timestamp={timestamp:?}, identical={identical}"
            );
        }
        assert!(!history_replacement_allowed(
            HistorySnapshot {
                edited_ms: None,
                ..old
            },
            HistorySnapshot {
                preview: None,
                edited_ms: None,
                edit_revision: Some(0),
                ..old
            }
        ));
    }

    #[test]
    fn unknown_revisions_keep_legacy_timestamp_ordering() {
        let current = HistorySnapshot {
            body: "body",
            preview: None,
            edited_ms: Some(10),
            edit_revision: None,
        };
        for (timestamp, expected) in [
            (None, false),
            (Some(9), false),
            (Some(10), true),
            (Some(11), true),
        ] {
            assert_eq!(
                history_replacement_allowed(
                    current,
                    HistorySnapshot {
                        body: "different body",
                        edited_ms: timestamp,
                        ..current
                    }
                ),
                expected
            );
        }
    }
}
