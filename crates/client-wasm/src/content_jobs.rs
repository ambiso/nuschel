// SPDX-License-Identifier: EUPL-1.2
//! Bounded SDK-owned source leases. Posted work retains credit until its reply
//! acknowledges completion, even after cancellation or expiry.

use crate::content_broker::{
    MAX_PENDING_BYTES, MAX_PENDING_JOBS, MAX_POSTED_JOBS, SourceMode, ValidatedRender,
    validate_render,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const DEADLINE_MS: u64 = 24_000;
const MAX_HANDLES: usize = 4096;
const MAX_PLAIN_BYTES: usize = 16 * 1024 * 1024;

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
    deadline: u64,
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
    pending: BTreeMap<String, Job>,
    pending_bytes: usize,
    handles: BTreeMap<Origin, Handle>,
    plain_bytes: usize,
}

impl Jobs {
    pub fn request(
        &mut self,
        origin: Origin,
        source: &str,
        mode: SourceMode,
        now: u64,
    ) -> Result<(), &'static str> {
        if source.len() > nuschl_core::MAX_CONTENT_BYTES {
            return Err("Content source exceeds the display limit.");
        }
        if self.pending.values().any(|job| {
            !job.cancelled && job.origin == origin && job.source == source && job.mode == mode
        }) {
            return Ok(());
        }
        // Account for the retained SDK source AND its outstanding transport copy.
        // Metadata is bounded independently, including very short/empty sources.
        let charge = source.len().saturating_mul(2).saturating_add(4096);
        if self.pending.len() >= MAX_PENDING_JOBS
            || self.pending_bytes.saturating_add(charge) > MAX_PENDING_BYTES
        {
            return Err("Content processing is busy. Retry when pending work finishes.");
        }
        self.next = self
            .next
            .checked_add(1)
            .ok_or("Content ticket counter exhausted.")?;
        self.pending.insert(
            self.next.to_string(),
            Job {
                origin,
                source: source.into(),
                mode,
                charge,
                deadline: now.saturating_add(DEADLINE_MS),
                posted: false,
                cancelled: false,
                cancel_sent: false,
            },
        );
        self.pending_bytes += charge;
        Ok(())
    }

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
                } else if now >= job.deadline {
                    job.cancelled = true;
                    expired.push((
                        job.origin.clone(),
                        job.mode,
                        <[u8; 32]>::from(Sha256::digest(job.source.as_bytes())),
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
                    operations.push(Operation::Cancel {
                        request: request.clone(),
                    });
                }
            }
            true
        });
        let mut failures = Vec::new();
        for (origin, mode, digest) in expired {
            if let Some(next) = self.next.checked_add(1) {
                self.next = next;
                let evicted =
                    self.install_digest(origin.clone(), digest, mode, next.to_string(), 0, true);
                failures.push(Failure { origin, evicted });
            }
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
            posted += 1;
            operations.push(Operation::Render {
                request: request.clone(),
                source: job.source.clone(),
                mode: job.mode,
            });
        }
        Batch {
            operations,
            failures,
        }
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
        let Some(job) = self.pending.get(request).filter(|job| job.posted) else {
            return Ok(None);
        };
        if job.cancelled || !current(&job.origin, &job.source) {
            let job = self.pending.remove(request).expect("observed job");
            self.pending_bytes -= job.charge;
            return Ok(None);
        }
        let job = self.pending.remove(request).expect("observed job");
        self.pending_bytes -= job.charge;
        let result = if now >= job.deadline {
            Ok(None)
        } else {
            reply.map(validate_render).transpose()
        };
        let failed = !matches!(&result, Ok(Some(_)));
        let rendered = result.ok().flatten().unwrap_or_else(|| ValidatedRender {
            document: nuschl_core::MessageDocument {
                blocks: vec![nuschl_core::ContentBlock::text(
                    "[Message could not be prepared. Retry or reload to try again.]",
                )],
            },
            plain_text: "[Message could not be prepared. Retry or reload to try again.]".into(),
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
        while self.handles.len() >= MAX_HANDLES
            || self.plain_bytes.saturating_add(plain_bytes) > MAX_PLAIN_BYTES
        {
            let Some(old_origin) = self
                .handles
                .iter()
                .min_by_key(|(_, handle)| handle.used)
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
        if let Some(handle) = self.handles.get_mut(origin) {
            if let Some(next) = self.next.checked_add(1) {
                self.next = next;
                handle.used = next;
            }
        }
    }

    /// Explicit navigation only: bounded metadata recovery preserves source
    /// representation even when a document/handle was evicted. No source copy,
    /// processing or authority rebinding occurs in snapshots or this planner.
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
                Some((
                    Origin::Message {
                        server: server.into(),
                        generation,
                        channel: channel.id,
                        instance: channel.instance,
                        serial: message.serial,
                        revision: message.revision,
                    },
                    message.source_format?,
                ))
            })
            .collect();
        for (origin, _) in &rows {
            self.touch(origin);
        }
        rows.into_iter()
            .filter(|(origin, _)| self.token(origin).is_none() && !self.pending(origin))
            .collect()
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
        let expired = jobs.take_with_failures(DEADLINE_MS, |_, _| true);
        assert_eq!(expired.operations.len(), 4);
        assert_eq!(expired.failures.len(), MAX_PENDING_JOBS);
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
        assert_eq!(expired.failures.len(), 6);
        assert_eq!(expired.operations.len(), MAX_POSTED_JOBS);
        assert_eq!(jobs.pending.len(), MAX_POSTED_JOBS);
        let held = jobs.pending_bytes;
        for serial in 0..6 {
            assert!(jobs.is_failed(&origin(serial)));
            assert!(jobs.token(&origin(serial)).is_some());
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
        assert!(expired.failures.is_empty());
        assert!(jobs.token(&origin(2)).is_none());
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
        for (origin, mode) in recovery {
            let source = model_message_mut(&mut servers, &origin).source.clone();
            if jobs.request(origin.clone(), &source, mode, 0).is_err() {
                let evicted = jobs.failed(origin, &source, mode).unwrap();
                jobs.clear_evicted(&mut servers, &evicted);
            }
        }
        assert!(jobs.pending.len() <= MAX_PENDING_JOBS);
        assert!(jobs.pending_bytes <= MAX_PENDING_BYTES);
        for row in 0..900 {
            let origin = model_origin(&servers[0], 0, row);
            assert!(jobs.token(&origin).is_some() || jobs.pending(&origin));
        }
        while !jobs.pending.is_empty() {
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
                apply_model(&jobs, &mut servers, completed);
            }
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
}
