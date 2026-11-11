// SPDX-License-Identifier: EUPL-1.2
//! The worker's requests (see `nuschel_ipc::crypto`). Every request is checked
//! against its bounds, handled, and every change is persisted before the answer
//! goes out.
use crate::state::{
    Checkpoint, Contact, MAX_CONTACTS, MAX_EVIDENCE, MAX_PUBLISHED, MAX_SERVERS, ServerState,
    State, Verified,
};
use crate::store::Store;
use nuschel_identity::{
    AccountStatement, DeviceCertificate, DeviceKeystore, DeviceRevocation,
    DeviceSigningCertificate, DisplayName, IdentityFingerprint, NewIdentity, RecoveryBlob,
    RecoveryKey, SafetyNumber, SafetyQr, Validity,
};
use nuschel_ipc::crypto::{
    ContactState, ContactView, CryptoRequest, CryptoResponse, CryptoServer, Due, HeadOutcome,
    IdentityView, KtRecordBytes, MonitorOutcome, OwnKeyAlarm, PublishBundle, RecoveryPolicy,
};
use nuschel_kt::{
    AccountBinding, AccountId, Assessment, AuditResponse, ContactTrust, DirectoryValue, ErrorClass,
    GossipOutcome, HeadGossip, HistoryResponse, IdentityRecord, KnownHeads, KtError, Label,
    LogDescriptor, LookupResponse, MonitorCheckpoint, Observation, Relation, RosterCommitment,
    SignedHead, VerifiedHead, check_freshness, check_gossip, monitor_own, should_gossip,
    verify_account_record, verify_audit, verify_head, verify_history, verify_lookup,
};
use std::io;
use zeroize::Zeroizing;

const DAY_MS: u64 = 24 * 3600 * 1000;
/// Device certificates are valid this long and renewed this long before they expire.
const DEVICE_VALIDITY_S: u64 = 365 * 86_400;
const RENEW_BEFORE_S: u64 = 30 * 86_400;
/// Histories asked for at once by the schedule.
const MAX_DUE_HISTORIES: usize = 64;
const STATE_FILE: &str = "state";
const RECOVERY_FILE: &str = "recovery";

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (index, pair) in text.as_bytes().chunks(2).enumerate() {
        let digit = |c: u8| (c as char).to_digit(16);
        out[index] = (digit(pair[0])? * 16 + digit(pair[1])?) as u8;
    }
    Some(out)
}

fn fingerprint(text: &str) -> Option<IdentityFingerprint> {
    unhex::<32>(text).map(IdentityFingerprint::from_bytes)
}

fn account_id(bytes: &[u8]) -> Result<[u8; 16], String> {
    bytes
        .try_into()
        .map_err(|_| "an account id is 16 bytes".to_owned())
}

/// A request's head is newer than the latest accepted one.
struct NeedAudit {
    from: u64,
    to: u64,
}

enum Step {
    Audit(NeedAudit),
    Kt(KtError),
}

impl From<KtError> for Step {
    fn from(error: KtError) -> Self {
        Self::Kt(error)
    }
}

pub struct Worker {
    store: Store,
    keystore: Option<DeviceKeystore>,
    state: State,
    /// The recovery key made at creation, until it is saved or skipped.
    recovery: Option<Zeroizing<String>>,
    clock: Box<dyn Fn() -> u64 + Send>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

impl Worker {
    pub fn open(store: Store) -> io::Result<Self> {
        let keystore = store.load_keystore()?;
        let state = match store.load(STATE_FILE)? {
            Some(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| io::Error::other(format!("the worker state: {error}")))?,
            None => State {
                version: 1,
                ..State::default()
            },
        };
        let recovery = match store.load(RECOVERY_FILE)? {
            Some(bytes) => Some(Zeroizing::new(
                String::from_utf8(bytes.to_vec())
                    .map_err(|_| io::Error::other("the pending recovery key"))?,
            )),
            None => None,
        };
        Ok(Self {
            store,
            keystore,
            state,
            recovery,
            clock: Box::new(now_ms),
        })
    }

    /// Replaces the clock (milliseconds since the Unix epoch), for tests.
    pub fn with_clock(mut self, clock: impl Fn() -> u64 + Send + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    fn save(&self) -> Result<(), String> {
        let json = Zeroizing::new(
            serde_json::to_vec(&self.state).map_err(|error| format!("state: {error}"))?,
        );
        self.store
            .save(STATE_FILE, &json)
            .map_err(|error| format!("cannot save the worker state: {error}"))
    }

    fn save_keystore(&self) -> Result<(), String> {
        match &self.keystore {
            Some(keystore) => self
                .store
                .save_keystore(keystore)
                .map_err(|error| format!("cannot save the keystore: {error}")),
            None => Ok(()),
        }
    }

    pub fn handle(&mut self, request: CryptoRequest) -> CryptoResponse {
        if let Err(what) = request.check() {
            return CryptoResponse::Rejected(format!("out of bounds: {what}"));
        }
        self.dispatch(request)
            .unwrap_or_else(CryptoResponse::Rejected)
    }

    fn dispatch(&mut self, request: CryptoRequest) -> Result<CryptoResponse, String> {
        Ok(match request {
            CryptoRequest::Identity => CryptoResponse::Identity(self.identity_view(false)),
            CryptoRequest::CreateIdentity {
                device_name,
                recovery,
            } => {
                let created = self.ensure_identity(&device_name, recovery)?;
                CryptoResponse::Identity(self.identity_view(created))
            }
            CryptoRequest::ShowRecoveryKey => match &self.recovery {
                Some(key) => CryptoResponse::RecoveryKey(key.to_string()),
                None => return Err("no recovery key is waiting".into()),
            },
            CryptoRequest::RecoveryKeySaved => {
                self.store
                    .remove(RECOVERY_FILE)
                    .map_err(|error| error.to_string())?;
                self.recovery = None;
                CryptoResponse::Identity(self.identity_view(false))
            }
            CryptoRequest::SkipRecoveryKey => {
                if self.state.recovery == RecoveryPolicy::Require {
                    return Err("a recovery key is required".into());
                }
                if let Some(keystore) = self.keystore.take() {
                    self.keystore = Some(keystore.without_recovery_blob());
                    self.save_keystore()?;
                }
                self.store
                    .remove(RECOVERY_FILE)
                    .map_err(|error| error.to_string())?;
                self.recovery = None;
                CryptoResponse::Identity(self.identity_view(false))
            }
            CryptoRequest::Connect {
                server,
                descriptor,
                account,
                head,
            } => CryptoResponse::Head(self.connect(server, descriptor, account, &head)?),
            CryptoRequest::Advance {
                server,
                head,
                audit,
            } => CryptoResponse::Head(self.advance(&server, &head, &audit)?),
            CryptoRequest::PublishBundle {
                server,
                device_name,
                recovery,
            } => CryptoResponse::Bundle(self.publish_bundle(&server, &device_name, recovery)?),
            CryptoRequest::Published {
                server,
                value,
                current,
            } => {
                let entry = self.server(&server)?;
                if !entry.published.contains(&value) {
                    return Err(
                        "the server would publish a binding this device did not make".into(),
                    );
                }
                let _ = current;
                CryptoResponse::Done
            }
            CryptoRequest::Monitor { server, history } => match self.monitor(&server, &history)? {
                Ok(outcome) => CryptoResponse::Monitor(outcome),
                Err(NeedAudit { from, to }) => CryptoResponse::NeedAudit { from, to },
            },
            CryptoRequest::Contact {
                server,
                account,
                lookup,
                record,
                direct,
                user_id,
            } => {
                let account = account_id(&account)?;
                match self.contact(&server, account, &lookup, record.as_ref(), direct)? {
                    Some(NeedAudit { from, to }) => CryptoResponse::NeedAudit { from, to },
                    None => {
                        if let Some(user_id) = user_id {
                            self.alias(&server, user_id, &account)?;
                        }
                        CryptoResponse::Contact(self.view(&server, &account))
                    }
                }
            }
            CryptoRequest::ContactHistory {
                server,
                account,
                history,
            } => {
                let account = account_id(&account)?;
                match self.contact_history(&server, account, &history)? {
                    Some(NeedAudit { from, to }) => CryptoResponse::NeedAudit { from, to },
                    None => CryptoResponse::Contact(self.view(&server, &account)),
                }
            }
            CryptoRequest::ContactUnavailable {
                server,
                account,
                not_found,
                reason,
            } => {
                let account = account_id(&account)?;
                self.contact_unavailable(&server, account, not_found, &reason)?;
                CryptoResponse::Contact(self.view(&server, &account))
            }
            CryptoRequest::ContactStatus { server, account } => {
                let account = account_id(&account)?;
                CryptoResponse::Contact(self.view(&server, &account))
            }
            CryptoRequest::MarkVerified { server, account } => {
                let account = account_id(&account)?;
                self.mark_verified(&server, &account, true)?;
                CryptoResponse::Contact(self.view(&server, &account))
            }
            CryptoRequest::UnmarkVerified { server, account } => {
                let account = account_id(&account)?;
                self.mark_verified(&server, &account, false)?;
                CryptoResponse::Contact(self.view(&server, &account))
            }
            CryptoRequest::AcceptKeyChange { server, account } => {
                let account = account_id(&account)?;
                self.accept_key_change(&server, &account)?;
                CryptoResponse::Contact(self.view(&server, &account))
            }
            CryptoRequest::Due { server, connected } => {
                CryptoResponse::Due(self.due(&server, connected))
            }
            CryptoRequest::MakeGossip {
                server,
                last_gossip_ms,
                newest_seen_epoch,
            } => CryptoResponse::Gossip(self.make_gossip(
                &server,
                last_gossip_ms,
                newest_seen_epoch,
            )?),
            CryptoRequest::CheckGossip { server, gossip } => {
                CryptoResponse::GossipChecked(self.check_gossip(&server, &gossip)?)
            }
            CryptoRequest::AcknowledgeAlarm { server } => {
                let entry = self.server_mut(&server)?;
                entry.alarm = None;
                entry.misbehaviour = None;
                self.save()?;
                CryptoResponse::Identity(self.identity_view(false))
            }
            CryptoRequest::ForgetServer { server } => {
                self.state.servers.remove(&server);
                self.save()?;
                CryptoResponse::Done
            }
            _ => return Err("MLS is not implemented yet".into()),
        })
    }

    // ---- identity ----

    fn identity_view(&self, created: bool) -> IdentityView {
        let mut view = match &self.keystore {
            Some(keystore) => IdentityView {
                fingerprint: Some(keystore.identity().fingerprint().to_hex()),
                device_name: keystore
                    .device_certificate()
                    .display_name()
                    .as_str()
                    .to_owned(),
                created,
                recovery_pending: self.recovery.is_some(),
                recoverable: keystore.recovery_blob().is_some(),
                can_link: keystore.device_signing().is_some(),
                alarms: Vec::new(),
            },
            None => IdentityView::default(),
        };
        view.alarms = self
            .state
            .servers
            .iter()
            .filter_map(|(key, entry)| {
                entry
                    .alarm
                    .as_ref()
                    .or(entry.misbehaviour.as_ref())
                    .map(|reason| OwnKeyAlarm {
                        server: key.clone(),
                        label: entry.label.clone(),
                        reason: reason.clone(),
                    })
            })
            .collect();
        view
    }

    fn own_fingerprint(&self) -> Option<IdentityFingerprint> {
        self.keystore
            .as_ref()
            .map(|keystore| keystore.identity().fingerprint())
    }

    /// Creates the identity unless there is one; `true` if it was created.
    fn ensure_identity(
        &mut self,
        device_name: &str,
        policy: RecoveryPolicy,
    ) -> Result<bool, String> {
        if self.keystore.is_some() {
            return Ok(false);
        }
        let now_s = self.now() / 1000;
        let name = DisplayName::new(if device_name.trim().is_empty() {
            "Nuschel"
        } else {
            device_name.trim()
        })
        .map_err(|error| format!("device name: {error}"))?;
        let validity = Validity::starting_at(now_s.saturating_sub(3600), DEVICE_VALIDITY_S)
            .map_err(|error| error.to_string())?;
        let identity =
            NewIdentity::create(&name, validity, now_s).map_err(|error| error.to_string())?;
        let mut keystore = identity.keystore().map_err(|error| error.to_string())?;
        let mut recovery = None;
        if policy != RecoveryPolicy::Never {
            // The root survives creation only under a recovery key, which waits here
            // (sealed) until the user saves or skips it (docs/e2ee.md §3.5).
            let key = RecoveryKey::generate().map_err(|error| error.to_string())?;
            let blob =
                RecoveryBlob::seal(&identity.root, &key).map_err(|error| error.to_string())?;
            keystore = keystore.with_recovery_blob(blob);
            recovery = Some(key.display());
        }
        drop(identity);
        self.store
            .save_keystore(&keystore)
            .map_err(|error| format!("cannot save the keystore: {error}"))?;
        if let Some(key) = &recovery {
            self.store
                .save(RECOVERY_FILE, key.as_bytes())
                .map_err(|error| format!("cannot keep the recovery key: {error}"))?;
        }
        self.keystore = Some(keystore);
        self.recovery = recovery;
        self.state.device_name = name.as_str().to_owned();
        self.state.recovery = policy;
        self.save()?;
        Ok(true)
    }

    // ---- servers and heads ----

    fn server(&self, key: &str) -> Result<&ServerState, String> {
        self.state
            .servers
            .get(key)
            .ok_or_else(|| "not connected to this server's key-transparency log".to_owned())
    }

    fn server_mut(&mut self, key: &str) -> Result<&mut ServerState, String> {
        self.state
            .servers
            .get_mut(key)
            .ok_or_else(|| "not connected to this server's key-transparency log".to_owned())
    }

    fn log(&self, key: &str) -> Result<(LogDescriptor, KnownHeads), String> {
        let entry = self.server(key)?;
        let descriptor =
            LogDescriptor::decode(&entry.descriptor).map_err(|error| error.to_string())?;
        let heads = entry
            .heads
            .iter()
            .filter_map(|bytes| SignedHead::decode(bytes).ok())
            .collect::<Vec<_>>();
        let known = KnownHeads::restore(&descriptor, &heads).map_err(|error| error.to_string())?;
        Ok((descriptor, known))
    }

    fn store_heads(&mut self, key: &str, known: &KnownHeads) -> Result<(), String> {
        let heads = known.heads().map(|head| head.signed().encode()).collect();
        self.server_mut(key)?.heads = heads;
        Ok(())
    }

    /// Keeps a misbehaviour's reason and evidence; returns the reason.
    fn misbehaved(&mut self, key: &str, error: &KtError) -> String {
        let reason = error.to_string();
        if let Ok(entry) = self.server_mut(key) {
            if let Some(evidence) = error.evidence()
                && entry.evidence.len() < MAX_EVIDENCE
            {
                entry.evidence.push(evidence.encode());
            }
            entry.misbehaviour = Some(reason.clone());
        }
        reason
    }

    fn head_failure(&mut self, key: &str, error: KtError) -> Result<HeadOutcome, String> {
        Ok(match error.class() {
            ErrorClass::ServerMisbehaviour => {
                let reason = self.misbehaved(key, &error);
                self.save()?;
                HeadOutcome::Misbehaviour { reason }
            }
            _ => HeadOutcome::Failed {
                reason: error.to_string(),
            },
        })
    }

    fn connect(
        &mut self,
        server: CryptoServer,
        descriptor: Vec<u8>,
        account: Vec<u8>,
        head: &[u8],
    ) -> Result<HeadOutcome, String> {
        if let Err(error) = LogDescriptor::decode(&descriptor) {
            return Ok(HeadOutcome::Failed {
                reason: format!("the server's log descriptor: {error}"),
            });
        }
        if !self.state.servers.contains_key(&server.key) && self.state.servers.len() >= MAX_SERVERS
        {
            return Err("too many servers".into());
        }
        let entry = self.state.servers.entry(server.key.clone()).or_default();
        if !entry.descriptor.is_empty() && entry.descriptor != descriptor {
            let reason = "The server's key-transparency log changed (another signing or VRF \
                          key). Nuschel keeps checking against the log it knew. If the operator \
                          really replaced it, reset this server's security record."
                .to_owned();
            entry.misbehaviour = Some(reason.clone());
            self.save()?;
            return Ok(HeadOutcome::Misbehaviour { reason });
        }
        entry.descriptor = descriptor;
        entry.label = server.label;
        if entry.account != account {
            // Another account on this server: its binding and monitoring start over.
            entry.account = account;
            entry.statement.clear();
            entry.published.clear();
            entry.checkpoint = None;
            entry.alarm = None;
        }
        let outcome = self.check_head(&server.key, head)?;
        self.save()?;
        Ok(outcome)
    }

    /// A head the server presents as its newest.
    fn check_head(&mut self, key: &str, bytes: &[u8]) -> Result<HeadOutcome, String> {
        let (descriptor, mut known) = self.log(key)?;
        let head = match SignedHead::decode(bytes)
            .map_err(KtError::from)
            .and_then(|signed| verify_head(&descriptor, &signed))
        {
            Ok(head) => head,
            Err(error) => return self.head_failure(key, error),
        };
        match self.accept_head(key, &mut known, &head) {
            Ok(()) => {
                self.store_heads(key, &known)?;
                Ok(HeadOutcome::Accepted {
                    epoch: head.epoch(),
                })
            }
            Err(Step::Audit(NeedAudit { from, to })) => Ok(HeadOutcome::NeedAudit { from, to }),
            Err(Step::Kt(error)) => self.head_failure(key, error),
        }
    }

    /// Fresh, and consistent with every accepted head; a newer one needs an audit
    /// first and becomes the target.
    fn accept_head(
        &mut self,
        key: &str,
        known: &mut KnownHeads,
        head: &VerifiedHead,
    ) -> Result<(), Step> {
        check_freshness(head, self.now(), Default::default())?;
        match known.classify_from_server(head)? {
            Relation::First | Relation::Known => {
                known.record(head.clone())?;
                Ok(())
            }
            Relation::Newer { since } => {
                if let Ok(entry) = self.server_mut(key) {
                    entry.target = head.signed().encode();
                }
                let _ = self.save();
                Err(Step::Audit(NeedAudit {
                    from: since.epoch(),
                    to: head.epoch(),
                }))
            }
            // `classify_from_server` reports older heads as rollbacks.
            Relation::Older => Err(Step::Kt(KtError::Rollback {
                latest: known.latest().map_or(0, VerifiedHead::epoch),
                got: head.epoch(),
            })),
        }
    }

    fn advance(&mut self, key: &str, head: &[u8], audit: &[u8]) -> Result<HeadOutcome, String> {
        let (descriptor, mut known) = self.log(key)?;
        let result = (|| -> Result<(), KtError> {
            let head = verify_head(&descriptor, &SignedHead::decode(head)?)?;
            match known.classify_from_server(&head)? {
                Relation::Newer { since } => {
                    let audit = AuditResponse::decode(audit)?;
                    verify_audit(&descriptor, &since, &head, &audit)?;
                    known.record(head)
                }
                Relation::First => known.record(head),
                Relation::Known | Relation::Older => Ok(()),
            }
        })();
        if let Err(error) = result {
            return self.head_failure(key, error);
        }
        self.store_heads(key, &known)?;
        let latest = known.latest().map_or(0, VerifiedHead::epoch);
        let target = self.server(key)?.target.clone();
        let outcome = match SignedHead::decode(&target) {
            Ok(target) if target.epoch > latest => HeadOutcome::NeedAudit {
                from: latest,
                to: target.epoch,
            },
            Ok(target) => {
                if let Ok(target) = verify_head(&descriptor, &target)
                    && let Err(error) = known.record(target)
                {
                    return self.head_failure(key, error);
                }
                self.store_heads(key, &known)?;
                self.server_mut(key)?.target.clear();
                HeadOutcome::Accepted { epoch: latest }
            }
            Err(_) => HeadOutcome::Accepted { epoch: latest },
        };
        self.save()?;
        Ok(outcome)
    }

    // ---- our own binding ----

    fn publish_bundle(
        &mut self,
        key: &str,
        device_name: &str,
        policy: RecoveryPolicy,
    ) -> Result<PublishBundle, String> {
        let created = self.ensure_identity(device_name, policy)?;
        let entry = self.server(key)?;
        let account =
            account_id(&entry.account).map_err(|_| "a guest has no account to bind".to_owned())?;
        let log_id = LogDescriptor::decode(&entry.descriptor)
            .map_err(|error| error.to_string())?
            .log_id();
        let previous = entry.statement.clone();
        let now_s = self.now() / 1000;
        let keystore = self.keystore.as_mut().ok_or("no identity")?;
        if keystore.device_certificate().validity().not_after() < now_s + RENEW_BEFORE_S {
            let validity = Validity::starting_at(now_s.saturating_sub(3600), DEVICE_VALIDITY_S)
                .map_err(|error| error.to_string())?;
            keystore
                .renew_device(validity)
                .map_err(|error| format!("cannot renew this device's certificate: {error}"))?;
            self.save_keystore()?;
        }
        let keystore = self.keystore.as_ref().ok_or("no identity")?;
        let identity = keystore.identity();
        let statement = match AccountStatement::decode(&previous) {
            Ok(statement)
                if statement.verify(identity).is_ok()
                    && statement.server() == &log_id.0
                    && statement.account() == &account =>
            {
                statement
            }
            _ => keystore
                .device_signing()
                .ok_or("this device cannot sign for the identity (no device-signing key)")?
                .sign_account(log_id.0, account, now_s)
                .map_err(|error| error.to_string())?,
        };
        let signing = identity.device_signing_certificate();
        let devices = vec![keystore.device_certificate().clone()];
        let roster =
            RosterCommitment::compute(signing, &devices, &[]).map_err(|error| error.to_string())?;
        let value = AccountBinding::with_statement(identity.root_key().clone(), roster, &statement)
            .to_value()
            .encode();
        let bundle = PublishBundle {
            root: identity.root_key().encode(),
            record: KtRecordBytes {
                device_signing_certificate: signing.encode(),
                devices: devices.iter().map(DeviceCertificate::encode).collect(),
                revocations: Vec::new(),
                statement: statement.encode(),
            },
            created,
        };
        let entry = self.server_mut(key)?;
        entry.statement = statement.encode();
        entry.published.retain(|known| *known != value);
        entry.published.push(value);
        if entry.published.len() > MAX_PUBLISHED {
            entry.published.remove(0);
        }
        self.save()?;
        Ok(bundle)
    }

    fn monitor(
        &mut self,
        key: &str,
        bytes: &[u8],
    ) -> Result<Result<MonitorOutcome, NeedAudit>, String> {
        let (descriptor, mut known) = self.log(key)?;
        let entry = self.server(key)?;
        let account = account_id(&entry.account)?;
        let label = Label::Account(AccountId(account));
        let history = match HistoryResponse::decode(bytes)
            .map_err(KtError::from)
            .and_then(|response| verify_history(&descriptor, &label, &response))
        {
            Ok(history) => history,
            Err(error) => return Ok(Ok(self.monitor_failure(key, error)?)),
        };
        match self.accept_head(key, &mut known, &history.head) {
            Ok(()) => self.store_heads(key, &known)?,
            Err(Step::Audit(audit)) => return Ok(Err(audit)),
            Err(Step::Kt(error)) => return Ok(Ok(self.monitor_failure(key, error)?)),
        }
        let entry = self.server(key)?;
        let published = entry.published.clone();
        let authorised = |value: &DirectoryValue| published.contains(&value.encode());
        // The first check starts at our first binding the log holds: what the account
        // carried before this device bound it is not ours to judge.
        let previous = match &entry.checkpoint {
            Some(checkpoint) => Some(MonitorCheckpoint {
                version: checkpoint.version,
                epoch: checkpoint.epoch,
                value: DirectoryValue::decode(&checkpoint.value)
                    .map_err(|error| error.to_string())?,
            }),
            None => match history
                .entries
                .iter()
                .rev()
                .find(|entry| authorised(&entry.value))
            {
                Some(first) => Some(MonitorCheckpoint {
                    version: first.version,
                    epoch: first.epoch,
                    value: first.value.clone(),
                }),
                None => {
                    self.save()?;
                    return Ok(Ok(MonitorOutcome::Consistent {
                        version: history.latest().version,
                        current: false,
                    }));
                }
            },
        };
        match monitor_own(&history, previous.as_ref(), authorised) {
            Ok(report) => {
                let now = self.now();
                let entry = self.server_mut(key)?;
                let current = entry.published.last() == Some(&report.checkpoint.value.encode());
                entry.checkpoint = Some(Checkpoint {
                    version: report.checkpoint.version,
                    epoch: report.checkpoint.epoch,
                    value: report.checkpoint.value.encode(),
                });
                entry.last_monitor_ms = now;
                self.save()?;
                Ok(Ok(MonitorOutcome::Consistent {
                    version: report.checkpoint.version,
                    current,
                }))
            }
            Err(error) => Ok(Ok(self.monitor_failure(key, error)?)),
        }
    }

    fn monitor_failure(&mut self, key: &str, error: KtError) -> Result<MonitorOutcome, String> {
        let outcome = match error.class() {
            ErrorClass::OwnKeyAlarm => {
                let reason = format!(
                    "Someone else's key was bound to your account on this server ({error}). \
                     If this wasn't you, the server or someone with access to it is \
                     impersonating you."
                );
                self.server_mut(key)?.alarm = Some(reason.clone());
                MonitorOutcome::OwnKeyAlarm { reason }
            }
            ErrorClass::ServerMisbehaviour => MonitorOutcome::Misbehaviour {
                reason: self.misbehaved(key, &error),
            },
            ErrorClass::Failure => MonitorOutcome::Failed {
                reason: error.to_string(),
            },
        };
        self.save()?;
        Ok(outcome)
    }

    // ---- contacts ----

    fn contact_entry(&mut self, key: &str, account: &[u8; 16]) -> Result<&mut Contact, String> {
        let entry = self.server_mut(key)?;
        let full = entry.contacts.len() >= MAX_CONTACTS;
        let id = hex(account);
        if full && !entry.contacts.contains_key(&id) {
            return Err("too many contacts on this server".into());
        }
        Ok(entry.contacts.entry(id).or_default())
    }

    fn is_verified(&self, root: &str) -> bool {
        self.state.verified.contains_key(root)
    }

    fn trust(&self, contact: &Contact) -> ContactTrust {
        ContactTrust {
            pinned: contact.pinned.as_deref().and_then(fingerprint),
            manually_verified: contact
                .pinned
                .as_deref()
                .is_some_and(|root| self.is_verified(root)),
            checked_version: contact.checked_version,
            pending_change: contact.pending_change.as_deref().and_then(fingerprint),
            failures: contact.failures,
            first_failure_ms: contact.first_failure_ms,
        }
    }

    /// Folds an observation into a contact (docs/e2ee.md §4, §5.5).
    fn observe(
        &mut self,
        key: &str,
        account: &[u8; 16],
        observation: Observation,
    ) -> Result<(), String> {
        let now = self.now();
        let contact = self.contact_entry(key, account)?.clone();
        let mut trust = self.trust(&contact);
        let assessment = trust.assess(observation, now);
        let mut contact = contact;
        contact.pinned = trust.pinned.map(|root| root.to_hex());
        contact.checked_version = trust.checked_version;
        contact.pending_change = trust.pending_change.map(|root| root.to_hex());
        contact.failures = trust.failures;
        contact.first_failure_ms = trust.first_failure_ms;
        match assessment {
            Assessment::FirstContact { .. } => contact.state = ContactState::Unverified,
            Assessment::Match { .. } => {
                if !matches!(contact.state, ContactState::KeyChanged { .. }) {
                    contact.state = ContactState::Unverified;
                }
            }
            Assessment::KeyChanged { was_verified, .. } => {
                contact.state = ContactState::KeyChanged {
                    blocking: was_verified,
                };
                contact.notice = Some(if was_verified {
                    "This contact's identity changed. They were verified; compare safety \
                     numbers again before you trust the new identity."
                        .into()
                } else {
                    "This contact's identity changed.".into()
                });
            }
            Assessment::TransientBinding { .. } => {
                contact.notice = Some(
                    "Another identity was bound to this account for a while and removed again."
                        .into(),
                );
                if !matches!(contact.state, ContactState::KeyChanged { .. }) {
                    contact.state = ContactState::Unverified;
                }
            }
            Assessment::Intermittent(_) => {}
            Assessment::Unavailable {
                error,
                manually_verified,
            } => {
                contact.state = ContactState::Unavailable {
                    reason: error.to_string(),
                    verified: manually_verified,
                };
            }
            Assessment::ServerMisbehaviour(error) | Assessment::OwnKeyAlarm(error) => {
                let reason = self.misbehaved(key, &error);
                contact.state = ContactState::Misbehaviour { reason };
            }
        }
        *self.contact_entry(key, account)? = contact;
        Ok(())
    }

    fn contact(
        &mut self,
        key: &str,
        account: [u8; 16],
        lookup: &[u8],
        record: Option<&KtRecordBytes>,
        direct: bool,
    ) -> Result<Option<NeedAudit>, String> {
        let (descriptor, mut known) = self.log(key)?;
        self.contact_entry(key, &account)?.direct |= direct;
        let label = Label::Account(AccountId(account));
        let verified = LookupResponse::decode(lookup)
            .map_err(KtError::from)
            .and_then(|response| verify_lookup(&descriptor, &label, &response));
        let observation = match verified {
            Ok((head, entry)) => match self.accept_head(key, &mut known, &head) {
                Ok(()) => {
                    self.store_heads(key, &known)?;
                    match check_record(&descriptor, account, &entry.value, record) {
                        Ok(root) => Observation::Lookup {
                            root,
                            version: entry.version,
                        },
                        Err(error) => Observation::Failed(error),
                    }
                }
                Err(Step::Audit(audit)) => return Ok(Some(audit)),
                Err(Step::Kt(error)) => Observation::Failed(error),
            },
            Err(error) => Observation::Failed(error),
        };
        self.observe(key, &account, observation)?;
        self.save()?;
        Ok(None)
    }

    /// Remembers which account the server showed for a registration number; another
    /// account for the same number gets a notice (the server cannot rename someone
    /// into a stranger silently).
    fn alias(&mut self, key: &str, user_id: u32, account: &[u8; 16]) -> Result<(), String> {
        let id = hex(account);
        let entry = self.server_mut(key)?;
        let previous = entry.aliases.get(&user_id).cloned();
        if previous.is_none() && entry.aliases.len() >= MAX_CONTACTS {
            return Ok(());
        }
        entry.aliases.insert(user_id, id.clone());
        if previous.is_some_and(|previous| previous != id) {
            self.contact_entry(key, account)?.notice = Some(format!(
                "The server now shows another account for this person (registration \
                 #{user_id}). Treat them as someone new until you compare safety numbers."
            ));
        }
        self.save()
    }

    fn contact_history(
        &mut self,
        key: &str,
        account: [u8; 16],
        bytes: &[u8],
    ) -> Result<Option<NeedAudit>, String> {
        let (descriptor, mut known) = self.log(key)?;
        let label = Label::Account(AccountId(account));
        let checked = self.contact_entry(key, &account)?.checked_version;
        let history = HistoryResponse::decode(bytes)
            .map_err(KtError::from)
            .and_then(|response| verify_history(&descriptor, &label, &response));
        let observation = match history {
            Ok(history) => match self.accept_head(key, &mut known, &history.head) {
                Ok(()) => {
                    self.store_heads(key, &known)?;
                    Observation::from_history(Ok(history), checked)
                }
                Err(Step::Audit(audit)) => return Ok(Some(audit)),
                Err(Step::Kt(error)) => Observation::Failed(error),
            },
            Err(error) => Observation::Failed(error),
        };
        self.observe(key, &account, observation)?;
        let now = self.now();
        self.contact_entry(key, &account)?.last_history_ms = now;
        self.save()?;
        Ok(None)
    }

    fn contact_unavailable(
        &mut self,
        key: &str,
        account: [u8; 16],
        not_found: bool,
        reason: &str,
    ) -> Result<(), String> {
        let contact = self.contact_entry(key, &account)?;
        if contact.pinned.is_none() {
            if not_found {
                contact.state = ContactState::NoIdentity;
                self.save()?;
            }
            return Ok(());
        }
        if not_found {
            // A key the server served before and now cannot: a failure (retried).
            self.observe(key, &account, Observation::Failed(KtError::NotFound))?;
            self.save()?;
        } else {
            let _ = reason;
        }
        Ok(())
    }

    fn mark_verified(
        &mut self,
        key: &str,
        account: &[u8; 16],
        verified: bool,
    ) -> Result<(), String> {
        let label = self.server(key)?.label.clone();
        let contact = self.contact_entry(key, account)?.clone();
        let Some(root) = contact.pinned else {
            return Err("this contact has no identity to verify yet".into());
        };
        if verified {
            if contact.pending_change.is_some() {
                return Err("accept or reject the contact's new identity first".into());
            }
            let at_ms = self.now();
            self.state.verified.insert(
                root,
                Verified {
                    server: key.to_owned(),
                    label,
                    at_ms,
                },
            );
            // Comparing safety numbers settles what the notices asked for.
            let contact = self.contact_entry(key, account)?;
            if matches!(contact.state, ContactState::KeyChanged { blocking: false }) {
                contact.state = ContactState::Unverified;
            }
            contact.notice = None;
        } else {
            self.state.verified.remove(&root);
        }
        self.save()
    }

    fn accept_key_change(&mut self, key: &str, account: &[u8; 16]) -> Result<(), String> {
        let contact = self.contact_entry(key, account)?.clone();
        let mut trust = self.trust(&contact);
        trust.accept_change();
        let contact = self.contact_entry(key, account)?;
        contact.pinned = trust.pinned.map(|root| root.to_hex());
        contact.pending_change = None;
        if matches!(contact.state, ContactState::KeyChanged { .. }) {
            contact.state = ContactState::Unverified;
        }
        contact.notice = None;
        self.save()
    }

    /// The contact as the UI shows it; verification is looked up live, so verifying
    /// someone on one server shows on every other server where the log binds the
    /// same identity.
    fn view(&self, key: &str, account: &[u8; 16]) -> ContactView {
        let entry = self.state.servers.get(key);
        let contact = entry
            .and_then(|entry| entry.contacts.get(&hex(account)))
            .cloned()
            .unwrap_or_default();
        let own_account = entry.is_some_and(|entry| entry.account == account);
        let theirs = contact.pinned.as_deref().and_then(fingerprint);
        let verified = |root: &str| {
            self.state
                .verified
                .get(root)
                .map(|verified| (verified.server != key).then(|| verified.label.clone()))
        };
        let state = match (&contact.state, contact.pinned.as_deref()) {
            (ContactState::Unverified | ContactState::Verified { .. }, Some(root)) => {
                match verified(root) {
                    Some(elsewhere) => ContactState::Verified { elsewhere },
                    None => ContactState::Unverified,
                }
            }
            (ContactState::Unavailable { reason, .. }, root) => ContactState::Unavailable {
                reason: reason.clone(),
                verified: root.is_some_and(|root| self.is_verified(root)),
            },
            (state, _) => state.clone(),
        };
        let (safety_number, qr) = match (self.own_fingerprint(), theirs) {
            (Some(own), Some(theirs)) if !own_account => (
                Some(SafetyNumber::new(&own, &theirs).to_string()),
                SafetyQr::new(&own, &theirs).as_bytes().to_vec(),
            ),
            _ => (None, Vec::new()),
        };
        ContactView {
            server: key.to_owned(),
            account: account.to_vec(),
            state,
            identity: contact.pinned.clone(),
            safety_number,
            qr,
            notice: contact.notice.clone(),
            direct: contact.direct,
        }
    }

    // ---- schedule and gossip ----

    fn due(&self, key: &str, connected: bool) -> Due {
        let Some(entry) = self.state.servers.get(key) else {
            return Due::default();
        };
        let now = self.now();
        let registered = entry.account.len() == 16;
        let current = entry
            .checkpoint
            .as_ref()
            .is_some_and(|checkpoint| entry.published.last() == Some(&checkpoint.value));
        let histories = entry
            .contacts
            .iter()
            .filter(|(_, contact)| {
                (contact.direct
                    || contact
                        .pinned
                        .as_deref()
                        .is_some_and(|r| self.is_verified(r)))
                    && (connected || now.saturating_sub(contact.last_history_ms) >= DAY_MS)
            })
            .filter_map(|(id, _)| unhex::<16>(id).map(|id| id.to_vec()))
            .take(MAX_DUE_HISTORIES)
            .collect();
        Due {
            publish: registered && (connected || !current),
            monitor: registered
                && (connected || now.saturating_sub(entry.last_monitor_ms) >= DAY_MS),
            histories,
        }
    }

    fn make_gossip(
        &self,
        key: &str,
        last_gossip_ms: Option<u64>,
        newest_seen_epoch: Option<u64>,
    ) -> Result<Vec<u8>, String> {
        let (_, known) = self.log(key)?;
        Ok(match known.latest() {
            Some(latest)
                if should_gossip(latest, newest_seen_epoch, last_gossip_ms, self.now()) =>
            {
                HeadGossip {
                    head: latest.signed().clone(),
                }
                .encode()
            }
            _ => Vec::new(),
        })
    }

    fn check_gossip(&mut self, key: &str, bytes: &[u8]) -> Result<String, String> {
        let (descriptor, mut known) = self.log(key)?;
        let gossip = match HeadGossip::decode(bytes) {
            Ok(gossip) => gossip,
            // Members cannot forge heads: a malformed one is the sender's, and dropped.
            Err(error) => return Ok(format!("dropped: {error}")),
        };
        Ok(match check_gossip(&descriptor, &known, &gossip) {
            Ok((head, GossipOutcome::Known | GossipOutcome::Older)) => {
                known.record(head).map_err(|error| error.to_string())?;
                self.store_heads(key, &known)?;
                self.save()?;
                "consistent".into()
            }
            Ok((head, GossipOutcome::Newer { since })) => format!(
                "newer: audit from epoch {} to {}",
                since.map_or(0, |since| since.epoch()),
                head.epoch()
            ),
            Err(error) if error.class() == ErrorClass::ServerMisbehaviour => {
                let reason = self.misbehaved(key, &error);
                self.save()?;
                format!("misbehaviour: {reason}")
            }
            Err(error) => format!("dropped: {error}"),
        })
    }
}

/// The record behind a verified binding (docs/e2ee.md §3.8): the identity's own
/// statement for this account and log, and the roster the log commits to.
fn check_record(
    descriptor: &LogDescriptor,
    account: [u8; 16],
    value: &DirectoryValue,
    record: Option<&KtRecordBytes>,
) -> Result<IdentityFingerprint, KtError> {
    let binding = AccountBinding::from_value(value).map_err(|_| KtError::MalformedValue)?;
    let record = record.ok_or_else(|| KtError::InvalidRecord("no record".into()))?;
    let invalid = |what: &str| KtError::InvalidRecord(format!("malformed {what}"));
    let record = IdentityRecord {
        device_signing: DeviceSigningCertificate::decode(&record.device_signing_certificate)
            .map_err(|_| invalid("device-signing certificate"))?,
        devices: record
            .devices
            .iter()
            .map(|bytes| DeviceCertificate::decode(bytes))
            .collect::<Result<_, _>>()
            .map_err(|_| invalid("device certificate"))?,
        revocations: record
            .revocations
            .iter()
            .map(|bytes| DeviceRevocation::decode(bytes))
            .collect::<Result<_, _>>()
            .map_err(|_| invalid("revocation"))?,
        statement: AccountStatement::decode(&record.statement)
            .map_err(|_| invalid("account statement"))?,
    };
    let verified =
        verify_account_record(&binding, &record, descriptor.log_id(), AccountId(account))?;
    Ok(verified.identity)
}
