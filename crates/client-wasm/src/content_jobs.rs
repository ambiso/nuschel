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
        let mut operations = Vec::new();
        self.pending.retain(|request, job| {
            if now >= job.deadline || !current(&job.origin, &job.source) {
                job.cancelled = true;
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
        operations
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
        if job.cancelled || now >= job.deadline || !current(&job.origin, &job.source) {
            let job = self.pending.remove(request).expect("observed job");
            self.pending_bytes -= job.charge;
            return Ok(None);
        }
        let job = self.pending.remove(request).expect("observed job");
        self.pending_bytes -= job.charge;
        let result = reply.map(validate_render).transpose();
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
        if let Some(previous) = self.handles.remove(&origin) {
            self.plain_bytes -= previous.plain_bytes;
        }
        let mut evicted = Vec::new();
        while self.handles.len() >= MAX_HANDLES
            || self.plain_bytes.saturating_add(plain_bytes) > MAX_PLAIN_BYTES
        {
            let Some((old_origin, old)) = self.handles.pop_first() else {
                break;
            };
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
                source_digest: Sha256::digest(source.as_bytes()).into(),
                failed,
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
        self.next = self
            .next
            .checked_add(1)
            .ok_or("Content ticket counter exhausted.")?;
        Ok(self.install(origin, source, mode, self.next.to_string(), 0, true))
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
        assert_eq!(jobs.take(DEADLINE_MS, |_, _| true).len(), 4);
        assert_eq!(jobs.pending.len(), 4);
        assert!(
            jobs.complete("1", Some("[]"), DEADLINE_MS, |_, _| true)
                .unwrap()
                .is_none()
        );
        jobs.request(origin(999), "new", SourceMode::Plain, DEADLINE_MS)
            .unwrap();
        assert_eq!(jobs.take(DEADLINE_MS, |_, _| true).len(), 1);
        assert!(
            jobs.complete("129", Some("[]"), DEADLINE_MS + 1, |_, _| true)
                .unwrap()
                .unwrap()
                .failed
        );
        assert_eq!(jobs.token(&origin(999)), Some("129"));
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
}
