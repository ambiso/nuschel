// SPDX-License-Identifier: EUPL-1.2
//! Bounded SDK-owned source leases. Posted work retains credit until its reply
//! acknowledges completion, even after cancellation or expiry. A cancelled job
//! whose ACK misses its bound retires the lane visibly; its credit stays held.

use crate::content_broker::{
    MAX_PENDING_BYTES, MAX_PENDING_JOBS, MAX_POSTED_JOBS, SourceMode, ValidatedRender,
    validate_render,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const QUEUE_DEADLINE_MS: u64 = 60_000;
/// Progress wall limit for the oldest unacknowledged posted job. The UI runs
/// posted jobs one at a time in request order, so only that job can hold the
/// codec. This is not the content worker's own execution deadline (in the UI).
const EXECUTION_DEADLINE_MS: u64 = 12_000;
/// Bound for the ACK of a cancelled posted job. Like every deadline here it is
/// measured with the SDK worker's `performance.now()`, which keeps advancing
/// while a tab or worker is frozen or suspended: after resume it can already
/// have elapsed before an imminent ACK is processed, and the lane then retires.
const CANCEL_ACK_DEADLINE_MS: u64 = 15_000;
/// A posted job that has not reached the head of the serial lane.
const UNARMED: u64 = u64::MAX;
const MAX_FAILED_HANDLES: usize = 128;
#[cfg(test)]
const DEADLINE_MS: u64 = QUEUE_DEADLINE_MS;
const MAX_HANDLES: usize = 4096;
const MAX_PLAIN_BYTES: usize = 16 * 1024 * 1024;
const TOO_LARGE: &str =
    "[This content exceeds the local 256 KiB preview limit. It cannot be previewed here.]";
const EXHAUSTED: &str =
    "[Preview tickets are exhausted. Reload this page to prepare new previews.]";
const RETIRED: &str = "[Message previews stopped because a preview job was not acknowledged. Reload this page to prepare previews again.]";
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestError {
    Overloaded,
    TooLarge,
    Exhausted,
    Retired,
}
impl RequestError {
    pub fn message(self) -> &'static str {
        match self {
            Self::Overloaded => "Content processing is busy. Retry when pending work finishes.",
            Self::TooLarge => TOO_LARGE,
            Self::Exhausted => EXHAUSTED,
            Self::Retired => RETIRED,
        }
    }
    pub fn terminal(self) -> Option<nuschl_core::MessageDocument> {
        (self != Self::Overloaded).then(|| nuschl_core::MessageDocument {
            blocks: vec![nuschl_core::ContentBlock::text(self.message())],
        })
    }
}
/// Why the UI retired its preview lane. Both variants are equally terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetireReason {
    /// A content ACK could not be delivered to the SDK.
    Undelivered,
    /// The UI received a content work batch that violates the protocol.
    Protocol,
}
impl std::str::FromStr for RetireReason {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "undelivered" => Ok(Self::Undelivered),
            "protocol" => Ok(Self::Protocol),
            _ => Err("Invalid content lane retirement."),
        }
    }
}
/// This controls presentation recovery only: it never supplies a source, mode,
/// authority, origin or ticket. The original SDK row remains authoritative.
pub fn is_terminal_document(document: &nuschl_core::MessageDocument) -> bool {
    matches!(document.blocks.as_slice(), [nuschl_core::ContentBlock::Text { text, spans, role: nuschl_core::TextRole::Paragraph }] if spans.is_empty() && (text == TOO_LARGE || text == EXHAUSTED || text == RETIRED))
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Origin {
    Message {
        server: String,
        generation: u64,
        channel: u32,
        instance: u64,
        serial: u64,
        revision: u32,
    },
    Welcome {
        server: String,
        generation: u64,
        revision: String,
    },
}

impl Origin {
    pub fn resident(&self, servers: &[nuschl_client::server::ServerState]) -> bool {
        let Self::Message {
            server,
            channel,
            serial,
            revision,
            ..
        } = self
        else {
            return false;
        };
        servers
            .iter()
            .find(|state| &state.id == server)
            .and_then(|state| state.channels.iter().find(|state| state.id == *channel))
            .is_some_and(|state| {
                state
                    .timeline
                    .messages
                    .iter()
                    .any(|message| message.serial == *serial && message.revision == *revision)
            })
    }

    fn same_row(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Message {
                    server: a,
                    channel: b,
                    serial: c,
                    revision: d,
                    ..
                },
                Self::Message {
                    server: e,
                    channel: f,
                    serial: g,
                    revision: h,
                    ..
                },
            ) => a == e && b == f && c == g && d == h,
            _ => self == other,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Operation {
    Render {
        request: String,
        source: String,
        mode: SourceMode,
    },
    Cancel {
        request: String,
    },
}

struct Job {
    origin: Origin,
    source: String,
    mode: SourceMode,
    charge: usize,
    /// Queue deadline until posted; then `UNARMED` until this job is the head.
    deadline: u64,
    ack_deadline: u64,
    posted: bool,
    cancelled: bool,
    cancel_sent: bool,
}

struct Handle {
    token: String,
    plain_bytes: usize,
    mode: SourceMode,
    source_digest: [u8; 32],
    failed: bool,
    used: u64,
}

pub struct Failure {
    pub terminal: Option<RequestError>,
    pub origin: Origin,
    pub evicted: Vec<Origin>,
}
pub struct Batch {
    pub operations: Vec<Operation>,
    pub failures: Vec<Failure>,
}

pub struct Completion {
    pub origin: Origin,
    pub source: String,
    pub token: String,
    pub rendered: ValidatedRender,
    pub failed: bool,
    pub evicted: Vec<Origin>,
}

#[derive(Default)]
pub struct Jobs {
    next: u64,
    pending: BTreeMap<u64, Job>,
    pending_bytes: usize,
    handles: BTreeMap<Origin, Handle>,
    plain_bytes: usize,
    selected: Option<(String, u32)>,
    retired: bool,
}

impl Jobs {
    pub fn request(
        &mut self,
        origin: Origin,
        source: &str,
        mode: SourceMode,
        now: u64,
    ) -> Result<(), RequestError> {
        if source.len() > nuschl_core::MAX_CONTENT_BYTES {
            return Err(RequestError::TooLarge);
        }
        if self.retired {
            return Err(RequestError::Retired);
        }
        if self.pending_source(&origin, source, mode) {
            return Ok(());
        }
        let next = self.next.checked_add(1).ok_or(RequestError::Exhausted)?;
        // Account for the retained SDK source AND its outstanding transport copy.
        // Metadata is bounded independently, including very short/empty sources.
        let charge = source.len().saturating_mul(2).saturating_add(4096);
        if self.pending.len() >= MAX_PENDING_JOBS
            || self.pending_bytes.saturating_add(charge) > MAX_PENDING_BYTES
        {
            return Err(RequestError::Overloaded);
        }
        self.next = next;
        self.pending.insert(
            self.next,
            Job {
                origin,
                source: source.into(),
                mode,
                charge,
                deadline: now.saturating_add(QUEUE_DEADLINE_MS),
                ack_deadline: UNARMED,
                posted: false,
                cancelled: false,
                cancel_sent: false,
            },
        );
        self.pending_bytes += charge;
        Ok(())
    }

    #[cfg(test)]
    pub fn take(&mut self, now: u64, current: impl Fn(&Origin, &str) -> bool) -> Vec<Operation> {
        self.take_with_failures(now, current).operations
    }

    pub fn take_with_failures(
        &mut self,
        now: u64,
        current: impl Fn(&Origin, &str) -> bool,
    ) -> Batch {
        let mut operations = Vec::new();
        let mut expired = Vec::new();
        self.pending.retain(|request, job| {
            if !job.cancelled {
                if !current(&job.origin, &job.source) {
                    job.cancelled = true;
                    expired.push((*request, job.origin.clone(), job.mode, None));
                } else if now >= job.deadline {
                    job.cancelled = true;
                    expired.push((
                        *request,
                        job.origin.clone(),
                        job.mode,
                        Some(<[u8; 32]>::from(Sha256::digest(job.source.as_bytes()))),
                    ));
                }
            }
            if job.cancelled {
                if !job.posted {
                    self.pending_bytes -= job.charge;
                    return false;
                }
                if !job.cancel_sent {
                    job.cancel_sent = true;
                    job.ack_deadline = now.saturating_add(CANCEL_ACK_DEADLINE_MS);
                    operations.push(Operation::Cancel {
                        request: request.to_string(),
                    });
                }
            }
            true
        });
        // A lost ACK never releases credit. Retire the lane instead: terminal,
        // visible to every waiting row, and without replacement capacity. The
        // UI can also retire it explicitly (`retire_lane`); both stop the same
        // outstanding work here, once.
        if self
            .pending
            .values()
            .any(|job| job.cancel_sent && now >= job.ack_deadline)
        {
            self.retired = true;
        }
        let mut stopped = Vec::new();
        if self.retired {
            self.pending.retain(|request, job| {
                if job.cancelled {
                    return true;
                }
                stopped.push(job.origin.clone());
                if !job.posted {
                    self.pending_bytes -= job.charge;
                    return false;
                }
                job.cancelled = true;
                job.cancel_sent = true;
                job.ack_deadline = now.saturating_add(CANCEL_ACK_DEADLINE_MS);
                operations.push(Operation::Cancel {
                    request: request.to_string(),
                });
                true
            });
        }
        let mut failures = Vec::new();
        for (request, origin, mode, digest) in expired {
            if self.successful_row(&origin) {
                continue;
            }
            if let Some(digest) = digest {
                // This identity was already issued for the original request.
                // Reusing it as failure metadata cannot mint new worker work,
                // including when the checked counter has reached its maximum.
                let evicted =
                    self.install_digest(origin.clone(), digest, mode, request.to_string(), 0, true);
                failures.push(Failure {
                    terminal: if self.retired {
                        Some(RequestError::Retired)
                    } else {
                        (self.next == u64::MAX).then_some(RequestError::Exhausted)
                    },
                    origin,
                    evicted,
                });
            } else {
                // The source lease is gone; only retire the still-resident
                // placeholder. This grants no authority to the new lifetime.
                failures.push(Failure {
                    terminal: None,
                    evicted: vec![origin.clone()],
                    origin,
                });
            }
        }
        for origin in stopped {
            if !self.successful_row(&origin) {
                failures.push(Failure {
                    terminal: Some(RequestError::Retired),
                    origin,
                    evicted: Vec::new(),
                });
            }
        }
        if self.retired {
            return Batch {
                operations,
                failures,
            };
        }
        // Cancelled posted jobs remain charged and occupy a slot until ACK.
        let mut posted = self.pending.values().filter(|job| job.posted).count();
        for (request, job) in &mut self.pending {
            if posted >= MAX_POSTED_JOBS {
                break;
            }
            if job.posted || job.cancelled {
                continue;
            }
            job.posted = true;
            job.deadline = UNARMED;
            posted += 1;
            operations.push(Operation::Render {
                request: request.to_string(),
                source: job.source.clone(),
                mode: job.mode,
            });
        }
        // The oldest unacknowledged posted job holds the serial codec, even
        // when cancelled: its worker can still be busy until the real ACK. The
        // next job's budget starts only once that ACK removed it here.
        if let Some(head) = self.pending.values_mut().find(|job| job.posted)
            && !head.cancelled
            && head.deadline == UNARMED
        {
            head.deadline = now.saturating_add(EXECUTION_DEADLINE_MS);
        }
        Batch {
            operations,
            failures,
        }
    }

    /// Whether a queue, progress or cancelled-ACK deadline needs a refresh.
    pub fn due(&self, now: u64) -> bool {
        if self.retired {
            // An explicit retirement still has outstanding work to stop.
            return self.pending.values().any(|job| !job.cancelled);
        }
        self.pending.values().any(|job| {
            if job.cancel_sent {
                now >= job.ack_deadline
            } else {
                !job.cancelled && now >= job.deadline
            }
        })
    }

    pub fn retired(&self) -> bool {
        self.retired
    }

    /// The UI's explicit, terminal retirement of its preview lane. Outstanding
    /// work is stopped at the next `take_with_failures`; posted credit stays
    /// held until each real ACK. Returns whether this call retired the lane.
    pub fn retire_lane(&mut self) -> bool {
        !std::mem::replace(&mut self.retired, true)
    }

    /// The reply itself releases transport credit. Invalid/stale results never
    /// install a handle or become renderable under another origin.
    pub fn complete(
        &mut self,
        request: &str,
        reply: Option<&str>,
        now: u64,
        current: impl FnOnce(&Origin, &str) -> bool,
    ) -> Result<Option<Completion>, &'static str> {
        // A forged reply cannot consume work that has not actually been posted.
        let Ok(request_id) = request.parse::<u64>() else {
            return Ok(None);
        };
        let Some(job) = self.pending.get(&request_id).filter(|job| job.posted) else {
            return Ok(None);
        };
        if job.cancelled || !current(&job.origin, &job.source) {
            let job = self.pending.remove(&request_id).expect("observed job");
            self.pending_bytes -= job.charge;
            return Ok(None);
        }
        let job = self.pending.remove(&request_id).expect("observed job");
        self.pending_bytes -= job.charge;
        let result = if now >= job.deadline {
            Ok(None)
        } else {
            reply.map(validate_render).transpose()
        };
        let failed = !matches!(&result, Ok(Some(_)));
        if failed && self.successful_row(&job.origin) {
            return Ok(None);
        }
        let fallback = if self.next == u64::MAX {
            EXHAUSTED
        } else {
            "[Message could not be prepared. Retry or reload to try again.]"
        };
        let rendered = result.ok().flatten().unwrap_or_else(|| ValidatedRender {
            document: nuschl_core::MessageDocument {
                blocks: vec![nuschl_core::ContentBlock::text(fallback)],
            },
            plain_text: fallback.into(),
            retained_bytes: 1024,
        });
        let token = request.to_owned();
        let evicted = self.install(
            job.origin.clone(),
            &job.source,
            job.mode,
            token.clone(),
            rendered.plain_text.len(),
            failed,
        );
        Ok(Some(Completion {
            origin: job.origin,
            source: job.source,
            token,
            rendered,
            failed,
            evicted,
        }))
    }

    fn install(
        &mut self,
        origin: Origin,
        source: &str,
        mode: SourceMode,
        token: String,
        plain_bytes: usize,
        failed: bool,
    ) -> Vec<Origin> {
        self.install_digest(
            origin,
            Sha256::digest(source.as_bytes()).into(),
            mode,
            token,
            plain_bytes,
            failed,
        )
    }

    fn install_digest(
        &mut self,
        origin: Origin,
        source_digest: [u8; 32],
        mode: SourceMode,
        token: String,
        plain_bytes: usize,
        failed: bool,
    ) -> Vec<Origin> {
        if failed && self.successful_row(&origin) {
            return Vec::new();
        }
        // A new origin for the SAME resident row replaces its accounting
        // atomically with the adapter's synchronous derivative replacement.
        self.handles.retain(|old, handle| {
            if old.same_row(&origin) {
                self.plain_bytes -= handle.plain_bytes;
                false
            } else {
                true
            }
        });
        let mut evicted = Vec::new();
        while if failed {
            self.handles.values().filter(|handle| handle.failed).count() >= MAX_FAILED_HANDLES
        } else {
            self.handles
                .values()
                .filter(|handle| !handle.failed)
                .count()
                >= MAX_HANDLES
                || self.plain_bytes.saturating_add(plain_bytes) > MAX_PLAIN_BYTES
        } {
            let Some(old_origin) = self
                .handles
                .iter()
                .filter(|(_, handle)| handle.failed == failed)
                .min_by_key(|(origin, handle)| (self.selected.as_ref().is_some_and(|(server, channel)| matches!(origin, Origin::Message { server: id, channel: target, .. } if id == server && target == channel)), handle.used))
                .map(|(origin, _)| origin.clone())
            else {
                break;
            };
            let old = self.handles.remove(&old_origin).expect("observed handle");
            self.plain_bytes -= old.plain_bytes;
            evicted.push(old_origin);
        }
        self.plain_bytes += plain_bytes;
        self.handles.insert(
            origin,
            Handle {
                token,
                plain_bytes,
                mode,
                source_digest,
                failed,
                used: self.next,
            },
        );
        evicted
    }

    /// A refused queue admission stores only bounded metadata and the original
    /// source digest. Explicit retry can rehydrate from the SDK's retained row;
    /// no additional source copy, background retry or worker credit is created.
    #[cfg(test)]
    pub fn failed(
        &mut self,
        origin: Origin,
        source: &str,
        mode: SourceMode,
    ) -> Result<Vec<Origin>, &'static str> {
        if source.len() > nuschl_core::MAX_CONTENT_BYTES {
            return Err("Content source exceeds the display limit.");
        }
        self.next = self
            .next
            .checked_add(1)
            .ok_or("Content ticket counter exhausted.")?;
        Ok(self.install(origin, source, mode, self.next.to_string(), 0, true))
    }

    /// Releasing a completed derivative's charge must release its exact
    /// resident model derivative too, even if its connection has since changed.
    /// A newer same-row owner wins; clearing old accounting cannot erase it.
    pub fn clear_evicted(
        &self,
        servers: &mut [nuschl_client::server::ServerState],
        origins: &[Origin],
    ) {
        for origin in origins {
            if self.owns_row(origin) {
                continue;
            }
            let Origin::Message {
                server,
                channel,
                serial,
                revision,
                ..
            } = origin
            else {
                continue;
            };
            if let Some(message) =
                servers
                    .iter_mut()
                    .find(|state| &state.id == server)
                    .and_then(|state| state.channels.iter_mut().find(|state| state.id == *channel))
                    .and_then(|state| {
                        state.timeline.messages.iter_mut().find(|message| {
                            message.serial == *serial && message.revision == *revision
                        })
                    })
            {
                message.document = nuschl_core::MessageDocument {
                    blocks: vec![nuschl_core::ContentBlock::text(
                        "[Preview evicted; reopen this conversation to prepare it again.]",
                    )],
                };
            }
        }
    }

    pub fn recovery_credit(&self) -> bool {
        self.pending.len() < MAX_POSTED_JOBS
    }

    pub fn select(&mut self, selected: Option<(String, u32)>) {
        self.selected = selected;
    }

    pub fn successful_row(&self, origin: &Origin) -> bool {
        self.handles
            .iter()
            .any(|(stored, handle)| !handle.failed && stored.same_row(origin))
    }

    pub fn permanent_refusal(&self, source: &str) -> Option<RequestError> {
        if source.len() > nuschl_core::MAX_CONTENT_BYTES {
            Some(RequestError::TooLarge)
        } else if self.retired {
            Some(RequestError::Retired)
        } else if self.next == u64::MAX {
            Some(RequestError::Exhausted)
        } else {
            None
        }
    }
    pub fn pending_source(&self, origin: &Origin, source: &str, mode: SourceMode) -> bool {
        self.pending.values().any(|job| {
            !job.cancelled && &job.origin == origin && job.source == source && job.mode == mode
        })
    }

    pub fn pending(&self, origin: &Origin) -> bool {
        self.pending
            .values()
            .any(|job| !job.cancelled && &job.origin == origin)
    }
    pub fn owns_row(&self, origin: &Origin) -> bool {
        self.handles.keys().any(|stored| stored.same_row(origin))
    }
    /// Explicit navigation touches existing selected rows before restoring
    /// missing handles, so old inactive rows are preferred eviction victims.
    pub fn touch(&mut self, origin: &Origin) {
        if let Some(handle) = self
            .handles
            .iter_mut()
            .find_map(|(stored, handle)| stored.same_row(origin).then_some(handle))
        {
            if let Some(next) = self.next.checked_add(1) {
                self.next = next;
                handle.used = next;
            }
        }
    }

    /// Explicit navigation, or a reconnect of the selected conversation: bounded
    /// metadata recovery preserves source representation even when a
    /// document/handle was evicted. No source copy or processing occurs in
    /// snapshots or this planner; only verified handles are re-keyed (`rebind`).
    pub fn recover_rows(
        &mut self,
        server: &str,
        generation: u64,
        channel: &nuschl_client::conversation::ConversationState,
    ) -> Vec<(Origin, SourceMode)> {
        let rows: Vec<_> = channel
            .timeline
            .messages
            .iter()
            .rev()
            .take(nuschl_client::timeline::MAX_TIMELINE)
            .filter_map(|message| {
                if is_terminal_document(&message.document) {
                    return None;
                }
                let mode = message.source_format?;
                let origin = Origin::Message {
                    server: server.into(),
                    generation,
                    channel: channel.id,
                    instance: channel.instance,
                    serial: message.serial,
                    revision: message.revision,
                };
                self.rebind(&origin, &message.source, mode);
                Some((origin, mode))
            })
            .collect();
        for (origin, _) in rows.iter().rev() {
            self.touch(origin);
        }
        rows.into_iter()
            .filter(|(origin, _)| {
                !self.successful_row(origin)
                    && self.token(origin).is_none()
                    && !self.pending(origin)
            })
            .collect()
    }

    /// After a connection lifetime change, a row's handle is keyed by its old
    /// origin. Re-key it ONLY for a successful derivative whose current row has
    /// the same serial, revision, source digest and mode; revoke anything else
    /// so the row is requested again from its current source. In-flight jobs
    /// are never re-keyed: `complete` still requires their exact origin.
    fn rebind(&mut self, origin: &Origin, source: &str, mode: SourceMode) {
        if self.handles.contains_key(origin) {
            return;
        }
        let Some(stale) = self
            .handles
            .keys()
            .find(|stored| stored.same_row(origin))
            .cloned()
        else {
            return;
        };
        let handle = self.handles.remove(&stale).expect("observed handle");
        if !handle.failed
            && handle.mode == mode
            && handle.source_digest == <[u8; 32]>::from(Sha256::digest(source.as_bytes()))
        {
            self.handles.insert(origin.clone(), handle);
        } else {
            self.plain_bytes -= handle.plain_bytes;
        }
    }

    pub fn is_failed(&self, origin: &Origin) -> bool {
        self.handles.get(origin).is_some_and(|handle| handle.failed)
    }

    pub fn origin(&self, token: &str) -> Option<(Origin, SourceMode, [u8; 32])> {
        self.handles
            .iter()
            .find(|(_, handle)| handle.token == token)
            .map(|(origin, handle)| (origin.clone(), handle.mode, handle.source_digest))
    }

    pub fn token(&self, origin: &Origin) -> Option<&str> {
        self.handles.get(origin).map(|handle| handle.token.as_str())
    }

    pub fn retain(&mut self, current: impl Fn(&Origin) -> bool) {
        self.handles.retain(|origin, handle| {
            if current(origin) {
                true
            } else {
                self.plain_bytes -= handle.plain_bytes;
                false
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn origin(serial: u64) -> Origin {
        Origin::Message {
            server: "owned".into(),
            generation: 1,
            channel: 7,
            instance: 9,
            serial,
            revision: 0,
        }
    }
    const REPLY: &str = r#"{"marked_text":"ok","plain_text":"ok","document":{"blocks":[{"kind":"text","text":"ok"}]}}"#;

    #[test]
    fn cancellation_retains_actual_transport_credit_and_stale_reply_cannot_install() {
        let mut jobs = Jobs::default();
        for serial in 0..6 {
            jobs.request(origin(serial), "source", SourceMode::Legacy, 0)
                .unwrap();
        }
        let work = jobs.take(0, |_, _| true);
        assert_eq!(work.len(), MAX_POSTED_JOBS);
        let held = jobs.pending_bytes;
        let cancels = jobs.take(1, |_, _| false);
        assert_eq!(cancels.len(), MAX_POSTED_JOBS);
        assert_eq!(jobs.pending.len(), MAX_POSTED_JOBS);
        assert!(jobs.pending_bytes < held);
        assert!(jobs.take(2, |_, _| true).is_empty());
        for operation in work {
            let Operation::Render { request, .. } = operation else {
                panic!()
            };
            assert!(
                jobs.complete(&request, Some(REPLY), 2, |_, _| true)
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(jobs.pending_bytes, 0);
        assert!(jobs.handles.is_empty());
    }

    #[test]
    fn exact_receipt_source_and_revision_guard_background_work_without_selection() {
        let mut jobs = Jobs::default();
        jobs.request(origin(1), "old", SourceMode::Plain, 0)
            .unwrap();
        jobs.request(origin(1), "old", SourceMode::Plain, 0)
            .unwrap();
        assert_eq!(jobs.take(0, |_, _| true).len(), 1);
        assert!(
            jobs.complete("1", Some(REPLY), 1, |_, source| source == "replacement")
                .unwrap()
                .is_none()
        );
        jobs.request(origin(2), "background", SourceMode::Plain, 2)
            .unwrap();
        assert!(
            jobs.complete("2", Some(REPLY), 3, |_, _| true)
                .unwrap()
                .is_none()
        );
        assert_eq!(jobs.take(3, |_, _| true).len(), 1);
        let completed = jobs
            .complete("2", Some(REPLY), 4, |lease, source| {
                lease == &origin(2) && source == "background"
            })
            .unwrap()
            .unwrap();
        assert_eq!(completed.rendered.plain_text, "ok");
        assert_eq!(jobs.token(&origin(2)), Some("2"));
        jobs.retain(|_| false);
        assert_eq!(jobs.plain_bytes, 0);
    }

    #[test]
    fn stalled_transport_and_bad_reply_never_enable_unbounded_replacement() {
        let mut jobs = Jobs::default();
        for serial in 0..MAX_PENDING_JOBS {
            jobs.request(origin(serial as u64), "", SourceMode::Plain, 0)
                .unwrap();
        }
        assert!(jobs.request(origin(999), "", SourceMode::Plain, 0).is_err());
        assert_eq!(jobs.take(0, |_, _| true).len(), 4);
        // Every queued job and the head of the serial lane expire; the other
        // posted jobs wait for the head and keep their unacknowledged credit.
        let expired = jobs.take_with_failures(DEADLINE_MS, |_, _| true);
        assert_eq!(expired.operations.len(), 1);
        assert_eq!(expired.failures.len(), MAX_PENDING_JOBS - 3);
        assert_eq!(jobs.pending.len(), 4);
        assert!(
            jobs.complete("1", Some("[]"), DEADLINE_MS, |_, _| true)
                .unwrap()
                .is_none()
        );
        jobs.request(origin(999), "new", SourceMode::Plain, DEADLINE_MS)
            .unwrap();
        let work = jobs.take(DEADLINE_MS, |_, _| true);
        assert_eq!(work.len(), 1);
        let Operation::Render { request, .. } = &work[0] else {
            panic!()
        };
        assert!(
            jobs.complete(request, Some("[]"), DEADLINE_MS + 1, |_, _| true)
                .unwrap()
                .unwrap()
                .failed
        );
        assert_eq!(jobs.token(&origin(999)), Some(request.as_str()));
    }

    #[test]
    fn queue_refusal_retains_only_bounded_retry_metadata_and_original_digest() {
        let mut jobs = Jobs::default();
        for serial in 0..MAX_PENDING_JOBS {
            jobs.request(origin(serial as u64), "", SourceMode::Plain, 0)
                .unwrap();
        }
        let bytes = jobs.pending_bytes;
        assert!(
            jobs.request(origin(999), "original", SourceMode::Legacy, 0)
                .is_err()
        );
        assert!(
            jobs.failed(origin(999), "original", SourceMode::Legacy)
                .unwrap()
                .is_empty()
        );
        assert_eq!(jobs.pending.len(), MAX_PENDING_JOBS);
        assert_eq!(jobs.pending_bytes, bytes);
        assert!(jobs.is_failed(&origin(999)));
        let token = jobs.token(&origin(999)).unwrap();
        let (observed, mode, digest) = jobs.origin(token).unwrap();
        assert_eq!(observed, origin(999));
        assert_eq!(mode, SourceMode::Legacy);
        assert_eq!(digest, <[u8; 32]>::from(Sha256::digest(b"original")));
        assert_ne!(digest, <[u8; 32]>::from(Sha256::digest(b"replacement")));
    }

    #[test]
    fn queued_and_posted_expiration_create_explicit_retry_without_releasing_unacked_slots() {
        let mut jobs = Jobs::default();
        for serial in 0..6 {
            jobs.request(origin(serial), "original", SourceMode::Legacy, 0)
                .unwrap();
        }
        let posted = jobs.take(0, |_, _| true);
        let expired = jobs.take_with_failures(DEADLINE_MS, |_, _| true);
        // Queued jobs and the serial head expire; posted followers still wait.
        assert_eq!(expired.failures.len(), 3);
        assert_eq!(expired.operations.len(), 1);
        assert_eq!(jobs.pending.len(), MAX_POSTED_JOBS);
        let held = jobs.pending_bytes;
        for serial in [0, 4, 5] {
            assert!(jobs.is_failed(&origin(serial)));
            assert!(jobs.token(&origin(serial)).is_some());
        }
        for serial in 1..4 {
            assert!(jobs.pending(&origin(serial)));
        }
        assert!(
            jobs.take_with_failures(DEADLINE_MS + 1, |_, _| true)
                .failures
                .is_empty()
        );
        jobs.request(origin(5), "original", SourceMode::Legacy, DEADLINE_MS + 1)
            .unwrap();
        assert!(
            jobs.take(DEADLINE_MS + 1, |_, _| true).is_empty(),
            "retry waits behind actual outstanding ACK credit"
        );
        let Operation::Render { request, .. } = &posted[0] else {
            panic!()
        };
        assert!(
            jobs.complete(request, Some(REPLY), DEADLINE_MS + 1, |_, _| true)
                .unwrap()
                .is_none(),
            "expired derivative cannot replace the failure handle"
        );
        assert!(jobs.pending_bytes < held + 4096 + "original".len() * 2);
        assert_eq!(jobs.take(DEADLINE_MS + 2, |_, _| true).len(), 1);
    }

    #[test]
    fn late_acknowledgment_is_failure_not_late_content_and_revoked_context_gets_nothing() {
        let mut jobs = Jobs::default();
        jobs.request(origin(1), "original", SourceMode::Plain, 0)
            .unwrap();
        jobs.take(0, |_, _| true);
        let completion = jobs
            .complete("1", Some(REPLY), DEADLINE_MS, |_, _| true)
            .unwrap()
            .unwrap();
        assert!(completion.failed);
        assert_ne!(completion.rendered.plain_text, "ok");
        assert_eq!(jobs.pending_bytes, 0);
        jobs.request(origin(2), "revoked", SourceMode::Plain, 0)
            .unwrap();
        let expired = jobs.take_with_failures(DEADLINE_MS, |_, _| false);
        assert_eq!(expired.failures.len(), 1);
        assert_eq!(expired.failures[0].evicted, vec![origin(2)]);
        assert!(jobs.token(&origin(2)).is_none());
        assert!(
            !jobs.owns_row(&origin(2)),
            "retirement cannot mint current authority"
        );
    }

    fn resident_model(channels: usize, count: usize) -> Vec<nuschl_client::server::ServerState> {
        use nuschl_client::{
            conversation::ConversationState,
            timeline::{IncomingMessage, Message},
        };
        use nuschl_core::{ChatMessage, ContentBlock, MessageDocument};
        let bookmark =
            crate::snapshot::bookmark("owned", "127.0.0.1", 64738, "fixture", None).unwrap();
        let mut server = nuschl_client::server::ServerState::new(bookmark);
        server.id = "owned".into();
        server.generation = 1;
        server.channels.clear();
        for index in 0..channels {
            let mut channel = ConversationState {
                id: index as u32 + 7,
                instance: 9,
                ..ConversationState::default()
            };
            channel.timeline.loaded = true;
            for row in 0..count {
                let mut message: Message = IncomingMessage::plain(
                    None,
                    ChatMessage {
                        server: "owned".into(),
                        channel: channel.id.to_string(),
                        author: "peer".into(),
                        observed_at_ms: row as i64,
                        body: format!("<b>original {index}/{row}</b>"),
                    },
                    MessageDocument {
                        blocks: vec![ContentBlock::text("pending")],
                    },
                    false,
                )
                .into();
                message.source_format = Some(SourceMode::Legacy);
                channel.timeline.messages.push(message);
            }
            server.channels.push(channel);
        }
        vec![server]
    }

    fn model_origin(
        server: &nuschl_client::server::ServerState,
        channel: usize,
        row: usize,
    ) -> Origin {
        let channel = &server.channels[channel];
        let message = &channel.timeline.messages[row];
        Origin::Message {
            server: server.id.clone(),
            generation: server.generation,
            channel: channel.id,
            instance: channel.instance,
            serial: message.serial,
            revision: message.revision,
        }
    }
    fn model_message_mut<'a>(
        servers: &'a mut [nuschl_client::server::ServerState],
        origin: &Origin,
    ) -> &'a mut nuschl_client::timeline::Message {
        let Origin::Message {
            server,
            channel,
            serial,
            revision,
            ..
        } = origin
        else {
            panic!()
        };
        servers
            .iter_mut()
            .find(|state| &state.id == server)
            .unwrap()
            .channels
            .iter_mut()
            .find(|state| state.id == *channel)
            .unwrap()
            .timeline
            .messages
            .iter_mut()
            .find(|message| message.serial == *serial && message.revision == *revision)
            .unwrap()
    }
    fn apply_model(
        jobs: &Jobs,
        servers: &mut [nuschl_client::server::ServerState],
        completion: Completion,
    ) {
        jobs.clear_evicted(servers, &completion.evicted);
        model_message_mut(servers, &completion.origin).document = completion.rendered.document;
    }
    fn resident_plain_bytes(servers: &[nuschl_client::server::ServerState]) -> usize {
        servers.iter().flat_map(|server|&server.channels).flat_map(|channel|&channel.timeline.messages).filter(|message|message.document.blocks.iter().any(|block|matches!(block,nuschl_core::ContentBlock::Text { text,.. } if text=="ok"))).count()*2
    }
    fn render_model(
        jobs: &mut Jobs,
        servers: &mut [nuschl_client::server::ServerState],
        origin: Origin,
    ) {
        let source = model_message_mut(servers, &origin).source.clone();
        jobs.request(origin.clone(), &source, SourceMode::Legacy, 0)
            .unwrap();
        let work = jobs.take(0, |candidate, _| candidate == &origin);
        let Operation::Render { request, .. } = &work[0] else {
            panic!()
        };
        let completion = jobs
            .complete(request, Some(REPLY), 1, |candidate, original| {
                candidate == &origin && original == source
            })
            .unwrap()
            .unwrap();
        apply_model(jobs, servers, completion);
    }

    #[test]
    fn resident_plain_charge_survives_reconnect_inactive_rows_and_replacement_is_atomic() {
        let mut servers = resident_model(2, 1);
        let mut jobs = Jobs::default();
        let first = model_origin(&servers[0], 0, 0);
        let inactive = model_origin(&servers[0], 1, 0);
        render_model(&mut jobs, &mut servers, first.clone());
        render_model(&mut jobs, &mut servers, inactive.clone());
        for generation in 2..102 {
            servers[0].generation = generation;
            servers[0].channels[0].instance += 1;
            jobs.retain(|origin| origin.resident(&servers));
            assert_eq!(jobs.plain_bytes, resident_plain_bytes(&servers));
            assert_eq!(jobs.plain_bytes, 4);
            let fresh = model_origin(&servers[0], 0, 0);
            assert_ne!(fresh, first);
            render_model(&mut jobs, &mut servers, fresh);
            assert_eq!(
                jobs.handles.len(),
                2,
                "new origin replaces SAME row charge instead of accumulating generations"
            );
            assert_eq!(jobs.plain_bytes, resident_plain_bytes(&servers));
            assert_eq!(jobs.plain_bytes, 4);
            assert!(
                jobs.token(&inactive).is_some(),
                "inactive resident derivative remains charged despite old generation"
            );
        }
        servers[0].channels[1].timeline.messages.clear();
        jobs.retain(|origin| origin.resident(&servers));
        assert_eq!(jobs.plain_bytes, 2);
        assert_eq!(jobs.plain_bytes, resident_plain_bytes(&servers));
        servers[0].channels[0].timeline.messages[0].revision += 1;
        servers[0].channels[0].timeline.messages[0].document =
            nuschl_core::MessageDocument { blocks: vec![] };
        jobs.retain(|origin| origin.resident(&servers));
        assert_eq!(jobs.plain_bytes, 0);
    }

    #[test]
    fn unsaved_raw_source_modes_recover_after_global_handle_eviction_without_history_io() {
        let mut servers = resident_model(5, 900);
        let mut jobs = Jobs::default();
        let originals: Vec<_> = servers[0]
            .channels
            .iter()
            .flat_map(|channel| &channel.timeline.messages)
            .map(|message| {
                (
                    message.serial,
                    message.source.clone(),
                    message.source_format,
                )
            })
            .collect();
        for channel in 0..5 {
            for row in 0..900 {
                let origin = model_origin(&servers[0], channel, row);
                render_model(&mut jobs, &mut servers, origin);
            }
        }
        assert_eq!(jobs.handles.len(), MAX_HANDLES);
        assert_eq!(jobs.plain_bytes, resident_plain_bytes(&servers));
        let first = model_origin(&servers[0], 0, 0);
        assert!(jobs.token(&first).is_none());
        let recovery = jobs.recover_rows("owned", 1, &servers[0].channels[0]);
        assert_eq!(recovery.len(), 4500 - MAX_HANDLES);
        // A bounded adapter cursor holds identities/modes, not duplicate raw
        // source. Queue refusal leaves the existing resident documents intact.
        let mut recovery: std::collections::VecDeque<_> = recovery.into();
        jobs.select(Some(("owned".into(), 0)));
        while !recovery.is_empty() || !jobs.pending.is_empty() {
            while let Some((origin, mode)) = recovery.front().cloned() {
                let source = model_message_mut(&mut servers, &origin).source.clone();
                if jobs.request(origin, &source, mode, 0).is_err() {
                    break;
                }
                recovery.pop_front();
            }
            assert!(jobs.pending.len() <= MAX_PENDING_JOBS);
            assert!(jobs.pending_bytes <= MAX_PENDING_BYTES);
            let work = jobs.take(1, |_, _| true);
            assert!(!work.is_empty());
            for operation in work {
                let Operation::Render { request, .. } = operation else {
                    panic!()
                };
                let completed = jobs
                    .complete(&request, Some(REPLY), 1, |_, _| true)
                    .unwrap()
                    .unwrap();
                assert!(
                    !completed.failed,
                    "a failure token is NOT recovered content"
                );
                apply_model(&jobs, &mut servers, completed);
            }
        }
        for row in 0..900 {
            let origin = model_origin(&servers[0], 0, row);
            assert!(jobs.successful_row(&origin));
            assert!(!jobs.is_failed(&origin));
        }
        assert_eq!(jobs.plain_bytes, resident_plain_bytes(&servers));
        let after: Vec<_> = servers[0]
            .channels
            .iter()
            .flat_map(|channel| &channel.timeline.messages)
            .map(|message| {
                assert!(message.id.is_none());
                (
                    message.serial,
                    message.source.clone(),
                    message.source_format,
                )
            })
            .collect();
        assert_eq!(
            after, originals,
            "no IDB reload, mode guess, body/time correlation or unsaved-row loss"
        );
    }
    #[test]
    fn numeric_fifo_and_execution_deadline_start_at_dispatch() {
        let mut jobs = Jobs::default();
        for serial in 0..20 {
            jobs.request(origin(serial), "source", SourceMode::Plain, 0)
                .unwrap();
        }
        let batch = jobs.take_with_failures(QUEUE_DEADLINE_MS - 1, |_, _| true);
        let ids: Vec<_> = batch
            .operations
            .into_iter()
            .map(|op| match op {
                Operation::Render { request, .. } => request,
                _ => panic!(),
            })
            .collect();
        assert_eq!(ids, ["1", "2", "3", "4"]);
        assert!(batch.failures.is_empty());
        let done = jobs
            .complete("1", Some(REPLY), QUEUE_DEADLINE_MS + 1, |_, _| true)
            .unwrap()
            .unwrap();
        assert!(
            !done.failed,
            "queue time cannot consume the execution deadline"
        );
    }

    #[test]
    fn serially_executed_posted_jobs_start_their_budget_at_the_head() {
        let mut jobs = Jobs::default();
        for serial in 0..MAX_POSTED_JOBS as u64 {
            jobs.request(origin(serial), "source", SourceMode::Plain, 0)
                .unwrap();
        }
        assert_eq!(jobs.take(0, |_, _| true).len(), MAX_POSTED_JOBS);
        // The UI codec runs one job at a time; each finishes inside its own
        // execution budget even though all were posted together.
        for (index, request) in ["1", "2", "3", "4"].into_iter().enumerate() {
            let now = (index as u64 + 1) * (EXECUTION_DEADLINE_MS - 1);
            let batch = jobs.take_with_failures(now - 1, |_, _| true);
            assert!(
                batch.failures.is_empty(),
                "queue time is not execution time"
            );
            let done = jobs
                .complete(request, Some(REPLY), now, |_, _| true)
                .unwrap()
                .unwrap();
            assert!(!done.failed);
            assert!(jobs.take(now, |_, _| true).is_empty());
        }
        assert_eq!(jobs.pending_bytes, 0);
    }

    #[test]
    fn failed_metadata_never_evicts_successful_derivatives_or_replaces_valid_rerender() {
        let mut jobs = Jobs::default();
        jobs.request(origin(0), "source", SourceMode::Plain, 0)
            .unwrap();
        jobs.take(0, |_, _| true);
        jobs.complete("1", Some(REPLY), 1, |_, _| true).unwrap();
        let token = jobs.token(&origin(0)).unwrap().to_owned();
        for serial in 1..1000 {
            assert!(
                jobs.failed(origin(serial), "source", SourceMode::Plain)
                    .unwrap()
                    .iter()
                    .all(|candidate| candidate != &origin(0))
            );
        }
        assert_eq!(jobs.token(&origin(0)), Some(token.as_str()));
        assert_eq!(
            jobs.handles.values().filter(|handle| handle.failed).count(),
            MAX_FAILED_HANDLES
        );
        jobs.request(origin(0), "source", SourceMode::Plain, 2)
            .unwrap();
        let batch = jobs.take(2, |_, _| true);
        let Operation::Render { request, .. } = &batch[0] else {
            panic!()
        };
        let expired = jobs.take_with_failures(2 + EXECUTION_DEADLINE_MS, |_, _| true);
        assert!(expired.failures.is_empty());
        assert!(
            jobs.complete(request, None, 3 + EXECUTION_DEADLINE_MS, |_, _| true)
                .unwrap()
                .is_none()
        );
        assert_eq!(jobs.token(&origin(0)), Some(token.as_str()));
    }

    #[test]
    fn reconnect_rebinds_verified_rows_to_the_current_origin_with_lru_priority() {
        let mut servers = resident_model(1, 5);
        let mut jobs = Jobs::default();
        for row in 0..5 {
            let origin = model_origin(&servers[0], 0, row);
            render_model(&mut jobs, &mut servers, origin);
        }
        let tokens: Vec<String> = (0..5)
            .map(|row| {
                jobs.token(&model_origin(&servers[0], 0, row))
                    .unwrap()
                    .to_owned()
            })
            .collect();
        // An obsolete posted job from the old lifetime is still outstanding.
        let old = model_origin(&servers[0], 0, 4);
        let source = servers[0].channels[0].timeline.messages[4].source.clone();
        jobs.request(old, &source, SourceMode::Legacy, 2).unwrap();
        let work = jobs.take(2, |_, _| true);
        let [
            Operation::Render {
                request: obsolete, ..
            },
        ] = work.as_slice()
        else {
            panic!()
        };
        servers[0].generation += 1;
        servers[0].channels[0].instance += 1;
        assert!(
            jobs.recover_rows("owned", servers[0].generation, &servers[0].channels[0])
                .is_empty(),
            "digest-verified rows need no new codec work"
        );
        for (row, token) in tokens.iter().enumerate() {
            let fresh = model_origin(&servers[0], 0, row);
            assert_eq!(
                jobs.token(&fresh),
                Some(token.as_str()),
                "the snapshot's exact current origin finds the derivative"
            );
            assert_eq!(jobs.origin(token).unwrap().0, fresh);
        }
        let used: Vec<_> = servers[0].channels[0].timeline.messages.iter().map(|message| jobs.handles.iter().find(|(origin, _)| matches!(origin, Origin::Message { serial, .. } if *serial == message.serial)).unwrap().1.used).collect();
        assert!(used.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(jobs.plain_bytes, resident_plain_bytes(&servers));
        // The old lifetime's late result is not current and cannot replace it.
        let fresh = model_origin(&servers[0], 0, 4);
        assert!(
            jobs.complete(obsolete, Some(REPLY), 3, |lease, _| lease == &fresh)
                .unwrap()
                .is_none()
        );
        assert_eq!(jobs.token(&fresh), Some(tokens[4].as_str()));
        assert_eq!(jobs.pending_bytes, 0);
    }

    #[test]
    fn reconnect_never_rebinds_a_changed_source_or_mode() {
        let mut servers = resident_model(1, 3);
        let mut jobs = Jobs::default();
        for row in 0..2 {
            let origin = model_origin(&servers[0], 0, row);
            render_model(&mut jobs, &mut servers, origin);
        }
        // A failure handle is not a successful receipt, even for unchanged input.
        let failed = model_origin(&servers[0], 0, 2);
        let source = servers[0].channels[0].timeline.messages[2].source.clone();
        jobs.failed(failed, &source, SourceMode::Legacy).unwrap();
        let stale: Vec<String> = (0..3)
            .map(|row| {
                jobs.token(&model_origin(&servers[0], 0, row))
                    .unwrap()
                    .to_owned()
            })
            .collect();
        servers[0].generation += 1;
        servers[0].channels[0].instance += 1;
        // Same serial and revision, but not the derivative's original input.
        servers[0].channels[0].timeline.messages[0].source = "<b>replaced</b>".into();
        servers[0].channels[0].timeline.messages[1].source_format = Some(SourceMode::Plain);
        let recovery = jobs.recover_rows("owned", servers[0].generation, &servers[0].channels[0]);
        assert_eq!(
            recovery.len(),
            3,
            "unverified rows are requested again from their current source"
        );
        for (row, token) in stale.iter().enumerate() {
            assert!(jobs.token(&model_origin(&servers[0], 0, row)).is_none());
            assert!(jobs.origin(token).is_none(), "the stale handle is revoked");
        }
        assert_eq!(jobs.plain_bytes, 0);
    }

    #[test]
    fn lost_cancel_ack_retires_the_lane_visibly_and_keeps_unacknowledged_credit() {
        let mut jobs = Jobs::default();
        for serial in 0..6 {
            jobs.request(origin(serial), "source", SourceMode::Plain, 0)
                .unwrap();
        }
        let posted = jobs.take(0, |_, _| true);
        assert_eq!(posted.len(), MAX_POSTED_JOBS);
        // The serial head expires; its Cancel is sent but no ACK ever arrives.
        let expired = jobs.take_with_failures(EXECUTION_DEADLINE_MS, |_, _| true);
        assert_eq!(expired.operations.len(), 1);
        let held = jobs.pending_bytes;
        let deadline = EXECUTION_DEADLINE_MS + CANCEL_ACK_DEADLINE_MS;
        assert!(!jobs.due(deadline - 1));
        assert!(
            jobs.take_with_failures(deadline - 1, |_, _| true)
                .failures
                .is_empty()
        );
        assert!(!jobs.retired());
        // The followers never started their budget behind the unacknowledged
        // head. The lost ACK becomes a visible terminal lane state rather than
        // a permanently silent busy, before any queued job's own deadline.
        assert!(jobs.due(deadline));
        let retired = jobs.take_with_failures(deadline, |_, _| true);
        assert!(jobs.retired());
        assert!(!jobs.due(deadline + 1));
        assert_eq!(
            retired.operations.len(),
            MAX_POSTED_JOBS - 1,
            "posted followers are cancelled but keep their credit"
        );
        assert_eq!(retired.failures.len(), 5, "every waiting row is told");
        assert!(
            retired
                .failures
                .iter()
                .all(|failure| failure.terminal == Some(RequestError::Retired))
        );
        assert_eq!(
            jobs.request(origin(9), "new", SourceMode::Plain, deadline),
            Err(RequestError::Retired)
        );
        assert!(
            RequestError::Retired
                .terminal()
                .is_some_and(|document| is_terminal_document(&document))
        );
        assert_eq!(jobs.permanent_refusal("new"), Some(RequestError::Retired));
        assert_eq!(
            jobs.pending.len(),
            MAX_POSTED_JOBS,
            "unacknowledged credit is never released"
        );
        assert!(jobs.pending_bytes > 0 && jobs.pending_bytes < held);
        // A late real ACK releases exactly its own credit; the lane stays terminal.
        let Operation::Render { request, .. } = &posted[0] else {
            panic!()
        };
        assert!(
            jobs.complete(request, Some(REPLY), deadline + 1, |_, _| true)
                .unwrap()
                .is_none()
        );
        assert_eq!(jobs.pending.len(), MAX_POSTED_JOBS - 1);
        assert!(jobs.take(deadline + 2, |_, _| true).is_empty());
        assert_eq!(
            jobs.request(origin(10), "new", SourceMode::Plain, deadline + 2),
            Err(RequestError::Retired)
        );
    }

    #[test]
    fn explicit_retirement_is_idempotent_terminal_and_keeps_posted_credit() {
        let mut jobs = Jobs::default();
        for serial in 0..6 {
            jobs.request(origin(serial), "source", SourceMode::Plain, 0)
                .unwrap();
        }
        let posted = jobs.take(0, |_, _| true);
        assert_eq!(posted.len(), MAX_POSTED_JOBS);
        let held = jobs.pending_bytes;
        assert!(!jobs.due(1));
        assert!(jobs.retire_lane());
        assert!(!jobs.retire_lane(), "a second retirement is a no-op");
        assert!(jobs.due(1), "outstanding work still has to be stopped");
        let stopped = jobs.take_with_failures(1, |_, _| true);
        assert_eq!(
            stopped.operations.len(),
            MAX_POSTED_JOBS,
            "posted jobs are cancelled, never released"
        );
        assert_eq!(stopped.failures.len(), 6, "every waiting row is told");
        assert!(
            stopped
                .failures
                .iter()
                .all(|failure| failure.terminal == Some(RequestError::Retired))
        );
        assert_eq!(jobs.pending.len(), MAX_POSTED_JOBS);
        assert!(jobs.pending_bytes > 0 && jobs.pending_bytes < held);
        assert!(!jobs.due(2));
        let again = jobs.take_with_failures(2, |_, _| true);
        assert!(again.operations.is_empty() && again.failures.is_empty());
        assert!(!jobs.retire_lane());
        assert_eq!(
            jobs.request(origin(9), "new", SourceMode::Plain, 2),
            Err(RequestError::Retired)
        );
        // Only each real ACK releases its posted credit; nothing is installed.
        for operation in posted {
            let Operation::Render { request, .. } = operation else {
                panic!()
            };
            assert!(
                jobs.complete(&request, Some(REPLY), 3, |_, _| true)
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(jobs.pending_bytes, 0);
        assert!(jobs.handles.is_empty());
        assert_eq!("undelivered".parse(), Ok(RetireReason::Undelivered));
        assert_eq!("protocol".parse(), Ok(RetireReason::Protocol));
        assert!("busy".parse::<RetireReason>().is_err());
    }

    #[test]
    fn cancelled_head_holds_the_following_job_until_its_real_ack() {
        let mut jobs = Jobs::default();
        for serial in 0..2 {
            jobs.request(origin(serial), "source", SourceMode::Plain, 0)
                .unwrap();
        }
        assert_eq!(jobs.take(0, |_, _| true).len(), 2);
        // The head's row changes; its worker may still be busy with it.
        let cancelled = jobs.take_with_failures(1, |lease, _| lease != &origin(0));
        assert!(
            matches!(cancelled.operations.as_slice(), [Operation::Cancel { request }] if request == "1")
        );
        // Past the follower's budget had it been armed at posting or at the
        // cancellation; it is still waiting behind the unacknowledged head.
        let waiting = jobs.take_with_failures(EXECUTION_DEADLINE_MS + 2, |_, _| true);
        assert!(waiting.failures.is_empty() && waiting.operations.is_empty());
        assert!(!jobs.retired());
        // Only the real ACK releases the head and starts the follower's budget.
        assert!(
            jobs.complete("1", None, EXECUTION_DEADLINE_MS + 3, |_, _| true)
                .unwrap()
                .is_none()
        );
        let start = EXECUTION_DEADLINE_MS + 3;
        assert!(jobs.take(start, |_, _| true).is_empty());
        assert!(!jobs.due(start + EXECUTION_DEADLINE_MS - 1));
        assert!(jobs.due(start + EXECUTION_DEADLINE_MS));
        let done = jobs
            .complete(
                "2",
                Some(REPLY),
                start + EXECUTION_DEADLINE_MS - 1,
                |_, _| true,
            )
            .unwrap()
            .unwrap();
        assert!(!done.failed);
        assert_eq!(jobs.pending_bytes, 0);
    }

    #[test]
    fn oversized_legacy_source_is_terminal_without_tickets_or_navigation_spin() {
        let mut servers = resident_model(1, 1);
        let mut jobs = Jobs::default();
        let source = format!("<p>{}</p>", "x".repeat(nuschl_core::MAX_CONTENT_BYTES));
        let origin = model_origin(&servers[0], 0, 0);
        let message = model_message_mut(&mut servers, &origin);
        message.source = source.clone();
        assert_eq!(
            jobs.request(origin.clone(), &source, SourceMode::Legacy, 0),
            Err(RequestError::TooLarge)
        );
        message.document = RequestError::TooLarge.terminal().unwrap();
        for _ in 0..100 {
            assert!(
                jobs.recover_rows("owned", 1, &servers[0].channels[0])
                    .is_empty()
            );
            assert!(jobs.take(0, |_, _| true).is_empty());
        }
        assert_eq!(jobs.next, 0);
        assert_eq!(jobs.pending_bytes, 0);
        assert!(jobs.handles.is_empty());
        assert_eq!(
            model_message_mut(&mut servers, &origin).source,
            source,
            "raw source/representation is never guessed or silently replaced"
        );
        assert_eq!(
            jobs.request(
                origin,
                "🙂".repeat(nuschl_core::MAX_CONTENT_BYTES / 4 + 1).as_str(),
                SourceMode::Plain,
                0
            ),
            Err(RequestError::TooLarge)
        );
    }

    #[test]
    fn counter_exhaustion_keeps_exact_issued_failure_identity_for_queued_and_posted_jobs() {
        for posted in [false, true] {
            let mut jobs = Jobs {
                next: u64::MAX - 1,
                ..Jobs::default()
            };
            jobs.request(origin(1), "source", SourceMode::Plain, 0)
                .unwrap();
            let token = u64::MAX.to_string();
            if posted {
                assert_eq!(jobs.take(0, |_, _| true).len(), 1);
            }
            let held = jobs.pending_bytes;
            assert_eq!(
                jobs.request(origin(2), "new", SourceMode::Plain, 1),
                Err(RequestError::Exhausted)
            );
            let expired = jobs.take_with_failures(QUEUE_DEADLINE_MS, |_, _| true);
            assert_eq!(expired.failures.len(), 1);
            assert_eq!(expired.failures[0].terminal, Some(RequestError::Exhausted));
            assert_eq!(jobs.token(&origin(1)), Some(token.as_str()));
            assert!(jobs.is_failed(&origin(1)));
            assert_eq!(jobs.next, u64::MAX);
            assert_eq!(jobs.pending_bytes, if posted { held } else { 0 });
            assert!(
                jobs.complete(&token, Some(REPLY), QUEUE_DEADLINE_MS + 1, |_, _| true)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(jobs.pending_bytes, 0);
            assert_eq!(jobs.token(&origin(1)), Some(token.as_str()));
            assert!(jobs.is_failed(&origin(1)));
            assert_eq!(
                jobs.request(origin(2), "new", SourceMode::Plain, QUEUE_DEADLINE_MS + 2),
                Err(RequestError::Exhausted)
            );
        }
    }

    #[test]
    fn canceled_late_ack_cannot_overwrite_retry_same_source_revision_or_new_revision() {
        for revision in [0, 1] {
            let mut jobs = Jobs::default();
            let old = origin(1);
            let mut fresh = old.clone();
            if let Origin::Message {
                revision: value, ..
            } = &mut fresh
            {
                *value = revision;
            }
            jobs.request(old.clone(), "same source", SourceMode::Plain, 0)
                .unwrap();
            jobs.take(0, |_, _| true);
            let expired = jobs.take_with_failures(EXECUTION_DEADLINE_MS, |_, _| true);
            assert_eq!(expired.failures.len(), 1);
            assert_eq!(jobs.token(&old), Some("1"));
            jobs.request(
                fresh.clone(),
                "same source",
                SourceMode::Plain,
                EXECUTION_DEADLINE_MS + 1,
            )
            .unwrap();
            let work = jobs.take(EXECUTION_DEADLINE_MS + 1, |_, _| true);
            assert!(work.iter().any(
                |operation| matches!(operation,Operation::Render{request,..} if request=="2")
            ));
            let done = jobs
                .complete(
                    "2",
                    Some(REPLY),
                    EXECUTION_DEADLINE_MS + 2,
                    |origin, source| origin == &fresh && source == "same source",
                )
                .unwrap()
                .unwrap();
            assert!(!done.failed);
            assert_eq!(jobs.token(&fresh), Some("2"));
            let held = jobs.pending_bytes;
            assert!(held > 0);
            assert!(
                jobs.complete("1", Some(REPLY), EXECUTION_DEADLINE_MS + 3, |_, _| true)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(jobs.pending_bytes, 0);
            assert_eq!(jobs.token(&fresh), Some("2"));
            assert!(!jobs.is_failed(&fresh));
        }
    }

    #[test]
    fn overloaded_is_temporary_and_duplicate_posted_request_does_not_need_a_new_counter() {
        let mut jobs = Jobs::default();
        for serial in 0..MAX_PENDING_JOBS {
            jobs.request(origin(serial as u64), "", SourceMode::Plain, 0)
                .unwrap();
        }
        assert_eq!(
            jobs.request(origin(999), "", SourceMode::Plain, 0),
            Err(RequestError::Overloaded)
        );
        assert!(RequestError::Overloaded.terminal().is_none());
        jobs.take(0, |_, _| true);
        jobs.next = u64::MAX;
        assert!(
            jobs.request(origin(0), "", SourceMode::Plain, 1).is_ok(),
            "duplicate existing work creates no new ticket"
        );
        assert_eq!(jobs.pending.len(), MAX_PENDING_JOBS);
    }
}
