// SPDX-License-Identifier: EUPL-1.2
//! Witnessing (docs/witness.md, docs/e2ee.md §5.9): this server submits its epoch
//! log to a small fixed set of witnesses (the project auditor first) and keeps their
//! cosignatures, which it attaches to the heads it serves. Clients never contact a
//! witness, so no witness learns who uses which server, and no server learns which
//! other servers its users are on.
//!
//! - **Origins** (`kt.origins`): one per name clients reach the server under; each
//!   is enrolled and cosigned separately, and a reply carries the checkpoint of the
//!   origin the client asked for (`KtStatus.origin`).
//! - **Submissions**: after new epochs, at most every `kt.witness_interval`, and the
//!   unchanged checkpoint again every `kt.witness_refresh_interval` for a fresh
//!   timestamp. A 409 names the witness's size; the next submission starts there. A
//!   long history goes in steps of at most 4096 epochs.
//! - Each cosignature is verified with the witness's configured key before it is
//!   kept; nothing a witness answers reaches clients unchecked.

use crate::config::Kt as KtConfig;
use crate::kt::StorageKt;
use nuschel_kt::witness::note::{CosignerKey, Note, NoteSignature};
use nuschel_kt::witness::{Checkpoint, WitnessedHead};
use nuschel_kt::{AkdDirectory, DEFAULT_WITNESSES, Directory, MAX_AUDIT_SPAN};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The path below a witness's URL (`nuschel_witness::ADD_EPOCHS_PATH`).
const ADD_EPOCHS: &str = "/nuschel-kt/v1/add-epochs";
/// The largest submission sent; larger spans are halved.
const MAX_SUBMISSION: usize = 15 * 1024 * 1024;
/// Submissions in one round for one origin and witness (conflicts and steps).
const MAX_STEPS: usize = 64;

struct Target {
    key: CosignerKey,
    url: String,
}

/// A witness's newest cosignature for one origin.
struct Cosigned {
    checkpoint: Checkpoint,
    /// The log-signed note it cosigned.
    note: Note,
    line: NoteSignature,
    at: Instant,
}

#[derive(Default)]
struct Book {
    /// By (origin, witness).
    cosigned: HashMap<(usize, usize), Cosigned>,
    /// The size each witness holds, as far as this server knows.
    sizes: HashMap<(usize, usize), u64>,
    /// Whether the last round with a witness failed (logged once per change).
    failing: HashMap<usize, bool>,
}

/// The witness set and what they cosigned.
pub struct Witnesses {
    origins: Vec<String>,
    targets: Vec<Target>,
    book: Mutex<Book>,
    agent: ureq::Agent,
    interval: Duration,
    refresh: Duration,
}

impl Witnesses {
    /// From `[kt]`: the built-in witnesses unless `default_witnesses = false`, and the
    /// configured ones (checked when the configuration was loaded).
    pub fn new(config: &KtConfig) -> Self {
        let mut targets: Vec<Target> = Vec::new();
        let builtin = DEFAULT_WITNESSES
            .iter()
            .filter(|_| config.default_witnesses)
            .map(|w| (w.vkey.to_owned(), w.url.to_owned()));
        let configured = config
            .witnesses
            .iter()
            .map(|w| (w.key.clone(), w.url.clone()));
        for (vkey, url) in builtin.chain(configured) {
            match CosignerKey::from_vkey(&vkey) {
                Ok(key) if !targets.iter().any(|t| t.key == key) => targets.push(Target {
                    key,
                    url: url.trim_end_matches('/').to_owned(),
                }),
                Ok(_) => {}
                Err(error) => tracing::error!(%error, "ignoring a witness with an invalid key"),
            }
        }
        let origins = config
            .origins
            .iter()
            .filter_map(|o| nuschel_kt::witness::origin(o))
            .collect();
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(120)))
            .user_agent("nuscheld")
            .build()
            .new_agent();
        Self {
            origins,
            targets,
            book: Mutex::default(),
            agent,
            interval: Duration::from_secs(config.witness_interval),
            refresh: Duration::from_secs(config.witness_refresh_interval),
        }
    }

    fn book(&self) -> std::sync::MutexGuard<'_, Book> {
        self.book.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The configured origin a client asked for (`host:port`, the session's sticky
    /// key), if this server has it.
    pub fn origin_index(&self, requested: &str) -> Option<usize> {
        let origin = nuschel_kt::witness::origin(requested)?;
        self.origins.iter().position(|o| *o == origin)
    }

    /// What to attach to a head of `epoch` for a client of `origin`: the checkpoint
    /// most witnesses cosigned (the newest of those), or, before any did, the current
    /// checkpoint signed by the log alone; with the head's inclusion proof.
    pub async fn witnessed(
        &self,
        directory: &AkdDirectory<StorageKt>,
        origin: usize,
        epoch: u64,
    ) -> Option<Vec<u8>> {
        let name = self.origins.get(origin)?;
        let chosen = {
            let book = self.book();
            let mut by_size: HashMap<u64, Vec<&Cosigned>> = HashMap::new();
            for ((o, _), cosigned) in &book.cosigned {
                if *o == origin {
                    by_size
                        .entry(cosigned.checkpoint.size)
                        .or_default()
                        .push(cosigned);
                }
            }
            by_size
                .into_iter()
                .max_by_key(|(size, lines)| (lines.len(), *size))
                .map(|(size, lines)| {
                    let mut note = lines[0].note.clone();
                    note.signatures
                        .extend(lines.iter().map(|cosigned| cosigned.line.clone()));
                    (size, note.format())
                })
        };
        let (size, note) = match chosen {
            Some(chosen) => chosen,
            None => {
                let (checkpoint, note) = directory.checkpoint(name).await.ok()?;
                (checkpoint.size, note)
            }
        };
        let inclusion = if epoch < size {
            directory.inclusion(epoch, size).await.ok()?
        } else {
            Vec::new()
        };
        WitnessedHead {
            checkpoint: note.into_bytes(),
            inclusion,
        }
        .encode()
        .ok()
    }

    /// Submits to every witness for every origin, forever.
    pub async fn run(self: Arc<Self>, directory: Arc<AkdDirectory<StorageKt>>) {
        if self.origins.is_empty() || self.targets.is_empty() {
            if !self.targets.is_empty() {
                tracing::warn!(
                    "kt.origins is empty: nothing is submitted to the witnesses, and clients \
                     that require them show \"Automatic verification unavailable\""
                );
            }
            return;
        }
        let mut tick = tokio::time::interval(self.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            self.round(&directory).await;
        }
    }

    /// One round: every (origin, witness) that is behind or due for a refresh.
    pub async fn round(&self, directory: &AkdDirectory<StorageKt>) {
        let Ok(size) = directory.log_size().await else {
            return;
        };
        for origin in 0..self.origins.len() {
            for witness in 0..self.targets.len() {
                let due = self
                    .book()
                    .cosigned
                    .get(&(origin, witness))
                    .is_none_or(|c| c.checkpoint.size < size || c.at.elapsed() >= self.refresh);
                if due {
                    let result = self.submit(directory, origin, witness, size).await;
                    let failed = result.is_err();
                    let was_failing = self.book().failing.insert(witness, failed);
                    match result {
                        Err(error) if was_failing != Some(true) => tracing::warn!(
                            witness = self.targets[witness].key.name(),
                            origin = self.origins[origin],
                            %error,
                            "a witness did not cosign"
                        ),
                        Ok(()) if was_failing != Some(false) => tracing::info!(
                            witness = self.targets[witness].key.name(),
                            origin = self.origins[origin],
                            "a witness cosigns this server's log"
                        ),
                        _ => {}
                    }
                }
            }
        }
    }

    async fn submit(
        &self,
        directory: &AkdDirectory<StorageKt>,
        origin: usize,
        witness: usize,
        size: u64,
    ) -> Result<(), String> {
        let name = &self.origins[origin];
        let target = &self.targets[witness];
        let mut old = self
            .book()
            .sizes
            .get(&(origin, witness))
            .copied()
            .unwrap_or(size)
            .min(size);
        let mut span = MAX_AUDIT_SPAN;
        for _ in 0..MAX_STEPS {
            let request = directory
                .add_epochs(name, old, span)
                .await
                .map_err(|e| format!("the log: {e}"))?;
            let body = request.encode().map_err(|e| e.to_string())?;
            if body.len() > MAX_SUBMISSION && span > 1 {
                span /= 2;
                continue;
            }
            let url = format!("{}{ADD_EPOCHS}", target.url);
            let agent = self.agent.clone();
            let (status, text) =
                tokio::task::spawn_blocking(move || -> Result<(u16, String), String> {
                    let mut response = agent
                        .post(&url)
                        .header("Content-Type", "application/octet-stream")
                        .send(&body[..])
                        .map_err(|e| e.to_string())?;
                    let status = response.status().as_u16();
                    let text = response
                        .body_mut()
                        .with_config()
                        .limit(64 * 1024)
                        .read_to_string()
                        .map_err(|e| e.to_string())?;
                    Ok((status, text))
                })
                .await
                .map_err(|e| e.to_string())??;
            match status {
                200 => {
                    let note = Note::parse(&request.checkpoint).map_err(|e| e.to_string())?;
                    let checkpoint = Checkpoint::parse(&note.text).map_err(|e| e.to_string())?;
                    let line = text
                        .lines()
                        .filter_map(|line| NoteSignature::parse_line(line).ok())
                        .find(|line| line.name == target.key.name())
                        .ok_or("the witness answered without a cosignature")?;
                    let mut cosigned = note.clone();
                    cosigned.signatures.push(line.clone());
                    match target.key.verify(&cosigned, &checkpoint) {
                        Ok(Some(_)) => {}
                        _ => return Err("the witness's cosignature does not verify".into()),
                    }
                    let mut book = self.book();
                    book.sizes.insert((origin, witness), checkpoint.size);
                    let done = checkpoint.size >= size;
                    old = checkpoint.size;
                    book.cosigned.insert(
                        (origin, witness),
                        Cosigned {
                            checkpoint,
                            note,
                            line,
                            at: Instant::now(),
                        },
                    );
                    if done {
                        return Ok(());
                    }
                }
                409 => {
                    let held: u64 = text
                        .trim()
                        .parse()
                        .map_err(|_| format!("a conflict without a size: {text:?}"))?;
                    if held > size {
                        return Err(format!(
                            "the witness holds {held} epochs of this origin, more than the log's {size}: \
                             it has another log for this name"
                        ));
                    }
                    self.book().sizes.insert((origin, witness), held);
                    if held == old {
                        return Err("the witness keeps answering a conflict".into());
                    }
                    old = held;
                }
                status => {
                    return Err(format!(
                        "HTTP {status}: {}",
                        text.chars().take(200).collect::<String>().trim()
                    ));
                }
            }
        }
        Err("too many steps".into())
    }
}
