// SPDX-License-Identifier: EUPL-1.2
//! End-to-end-encrypted direct conversations (`mls/1`, docs/e2ee.md §6, §7) for a
//! session. The crypto worker owns every MLS secret and decision; this module moves
//! bytes between it and the server, inside the key-transparency driver of
//! [`crate::identity`] (credentials are checked against the rosters its lookups
//! bring in, and its audits keep the log honest):
//!
//! - **Key packages.** On connect, and whenever the server reports fewer than
//!   [`LOW_WATER`] one-time packages, upload a batch with a last-resort one. Until
//!   our key-transparency binding is published the server refuses them; retried
//!   with backoff.
//! - **Welcomes.** Fetched from our device's mailbox on connect and pushed live;
//!   each is joined with the server's ratchet tree once the peer's identity was
//!   looked up, then acknowledged.
//! - **Opening a direct conversation** ([`MlsCommand::OpenDirect`]): the
//!   conversation from the server, key-transparency lookups of the peer and of our
//!   own account, and, without a group, key packages claimed for both accounts and
//!   one commit that creates the group (`MlsCommit` with `create`).
//! - **Sending** ([`MlsCommand::Send`]): encrypted by the worker, sent as
//!   `MlsSend`; after `STALE_EPOCH` the stream is processed and the same envelope
//!   re-encrypted under the same idempotency key.
//! - **Receiving.** Every row of a pinned conversation goes to the worker in `seq`
//!   order: live `ChatEvent`s when they follow the cursor, otherwise a page
//!   `after(cursor)` first. Plaintext rows in a pinned conversation are refused
//!   (§7.2), never shown.
//! - **New devices.** When the worker reports accounts with devices outside the
//!   group (`ConversationView::missing`), their key packages are claimed and added.
use crate::identity::{Context, Reply};
use nuschel_ipc::crypto::{ConversationView, CryptoRequest, CryptoResponse, MlsOutgoing, Received};
use nuschel_protocol::nuschel::{
    self, kind,
    v1::{self, error::Code},
};
use prost::Message as _;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

/// Upload more one-time key packages below this many.
pub const LOW_WATER: u32 = 4;
/// One-time key packages per upload.
const BATCH: u32 = 8;
/// Attempts to send one message across stale epochs.
const MAX_RESENDS: usize = 4;
/// Pages fetched in one catch-up.
const MAX_PAGES: usize = 64;

/// What the frontend asks for in end-to-end conversations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MlsCommand {
    /// Open (creating it if needed) the end-to-end direct conversation with this
    /// registered user.
    OpenDirect { user_id: u32 },
    /// Send chat text; `key` is the idempotency key (keep it to retry).
    Send {
        conversation: [u8; 16],
        text: String,
        key: [u8; 16],
    },
    /// The conversation's state now (lock, members, why sending is blocked).
    Status { conversation: [u8; 16] },
    /// Catch up from the server's stream.
    Sync { conversation: [u8; 16] },
}

/// What happens in end-to-end conversations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MlsEvent {
    /// A conversation's end-to-end state: lock, members, blocked sending; `peer`
    /// is the other user's registration number when known.
    Conversation {
        view: ConversationView,
        peer: Option<u32>,
    },
    /// A message of an end-to-end conversation (decrypted, or explicitly not).
    Received {
        conversation: [u8; 16],
        received: Received,
    },
    /// Our message was stored as `seq`.
    Sent {
        conversation: [u8; 16],
        key: [u8; 16],
        seq: u64,
    },
    /// Our message was not sent (blocked, or the server refused it).
    NotSent {
        conversation: [u8; 16],
        key: [u8; 16],
        reason: String,
    },
    /// The server delivered plaintext into a pinned conversation; not shown.
    PlaintextRefused { conversation: [u8; 16], seq: u64 },
    /// The server holds `one_time` one-time key packages of this device (others can
    /// start conversations with it while it is offline).
    KeyPackages { one_time: u32 },
    /// A step failed; shown as text.
    Failed(String),
}

/// The driver's end-to-end state on one connection.
#[derive(Default)]
pub(crate) struct MlsState {
    /// Our device id (16 bytes), once the worker made key packages.
    device: Vec<u8>,
    /// When to try uploading key packages again (`None`: not needed now).
    upload_at: Option<Instant>,
    backoff: u32,
    /// The last seq handed to the worker, per pinned conversation.
    cursors: BTreeMap<[u8; 16], u64>,
    /// Direct conversations: the peer's account and registration number.
    directs: BTreeMap<[u8; 16], ([u8; 16], Option<u32>)>,
    /// Our own account was looked up on this connection (own devices' roster).
    own_checked: bool,
}

impl MlsState {
    pub(crate) fn upload_due(&self) -> Option<Instant> {
        self.upload_at
    }
}

fn id16(bytes: &[u8]) -> Option<[u8; 16]> {
    bytes.try_into().ok()
}

impl Context {
    async fn mls_emit(&self, event: MlsEvent) {
        let _ = self
            .events
            .send(crate::connection::SessionEvent::Mls(event))
            .await;
    }

    /// Emits a conversation's view with its peer.
    async fn mls_view(&mut self, view: ConversationView) {
        let peer = id16(&view.conversation)
            .and_then(|conversation| self.mls().directs.get(&conversation).copied())
            .and_then(|(_, user)| user);
        self.mls_emit(MlsEvent::Conversation { view, peer }).await;
    }

    fn mls(&mut self) -> &mut MlsState {
        self.mls.get_or_insert_with(MlsState::default)
    }

    async fn mls_worker(&self, request: CryptoRequest) -> Result<CryptoResponse, crate::Error> {
        self.crypto.request(request).await
    }

    /// On connect: identity, key packages, Welcomes, and catching up every pinned
    /// direct conversation.
    pub(crate) async fn mls_start(&mut self) -> Result<(), crate::Error> {
        if let Ok(CryptoResponse::Identity(view)) = self
            .mls_worker(CryptoRequest::CreateIdentity {
                device_name: self.device_name.clone(),
                recovery: self.recovery,
            })
            .await
            && view.created
        {
            self.emit(crate::identity::IdentityEvent::Own(view)).await;
        }
        self.mls().upload_at = Some(Instant::now());
        self.mls_upload().await?;
        self.mls_directs().await?;
        self.mls_fetch_welcomes().await?;
        let conversations: Vec<[u8; 16]> = self.mls().directs.keys().copied().collect();
        for conversation in conversations {
            if let Ok(view) = self.mls_status(conversation).await
                && view.pinned
            {
                self.mls_sync(conversation).await?;
            }
        }
        Ok(())
    }

    /// Uploads key packages when due; reschedules with backoff while the server
    /// refuses them (our binding waits for its epoch).
    pub(crate) async fn mls_upload(&mut self) -> Result<(), crate::Error> {
        let due = self
            .mls
            .as_ref()
            .and_then(|state| state.upload_at)
            .is_some_and(|at| at <= Instant::now());
        if !due {
            return Ok(());
        }
        let mut one_time = BATCH;
        let mut last_resort = true;
        for _ in 0..3 {
            let batch = match self
                .mls_worker(CryptoRequest::MlsKeyPackages {
                    server: self.server.key.clone(),
                    one_time,
                    last_resort,
                })
                .await?
            {
                CryptoResponse::MlsKeyPackages(batch) => batch,
                _ => return Err(crate::Error::Worker("unexpected worker answer".into())),
            };
            self.mls().device = batch.device.clone();
            let key_packages = batch.one_time;
            let last = batch.last_resort;
            let reply = self
                .ask(kind::MLS_KEY_PACKAGE_UPLOAD, move |request_id| {
                    v1::MlsKeyPackageUpload {
                        request_id,
                        key_packages,
                        last_resort: last,
                    }
                    .encode_to_vec()
                })
                .await?;
            match reply {
                Reply::Frame(kind::MLS_KEY_PACKAGE_STATUS, payload) => {
                    let status = v1::MlsKeyPackageStatus::decode(payload)
                        .map_err(|_| crate::Error::Protocol("invalid key package status".into()))?;
                    self.mls().backoff = 0;
                    self.mls_emit(MlsEvent::KeyPackages {
                        one_time: status.one_time,
                    })
                    .await;
                    if status.one_time >= LOW_WATER {
                        self.mls().upload_at = None;
                        return Ok(());
                    }
                    one_time = BATCH.min(16);
                    last_resort = !status.last_resort;
                }
                Reply::Error(_) | Reply::Frame(..) => {
                    // Usually: our identity is not published in this server's log yet.
                    let state = self.mls();
                    state.backoff = (state.backoff + 1).min(8);
                    state.upload_at =
                        Some(Instant::now() + Duration::from_secs(1 << state.backoff.min(6)));
                    return Ok(());
                }
            }
        }
        self.mls().upload_at = None;
        Ok(())
    }

    async fn mls_directs(&mut self) -> Result<(), crate::Error> {
        let reply = self
            .ask(kind::LIST_DIRECT, |request_id| {
                v1::ListDirect { request_id }.encode_to_vec()
            })
            .await?;
        if let Reply::Frame(kind::DIRECT_LIST, payload) = reply
            && let Ok(list) = v1::DirectList::decode(payload)
        {
            for direct in list.directs.iter().take(nuschel::MAX_DIRECTS) {
                if let (Some(conversation), Some(account)) =
                    (id16(&direct.conversation), id16(&direct.account))
                {
                    let user = (direct.user_id != 0).then_some(direct.user_id);
                    self.mls().directs.insert(conversation, (account, user));
                }
            }
        }
        Ok(())
    }

    async fn mls_status(
        &mut self,
        conversation: [u8; 16],
    ) -> Result<ConversationView, crate::Error> {
        match self
            .mls_worker(CryptoRequest::MlsStatus {
                server: self.server.key.clone(),
                conversation: conversation.to_vec(),
            })
            .await?
        {
            CryptoResponse::MlsConversation(view) => {
                if view.pinned {
                    let cursor = self.mls().cursors.entry(conversation).or_insert(0);
                    *cursor = (*cursor).max(view.cursor);
                } else {
                    self.mls().cursors.remove(&conversation);
                }
                Ok(view)
            }
            _ => Err(crate::Error::Worker("unexpected worker answer".into())),
        }
    }

    /// Looks up the peer and (once per connection) our own account, so the
    /// worker has both rosters.
    async fn mls_rosters(&mut self, peer: [u8; 16]) -> Result<(), crate::Error> {
        self.check_contact(None, Some(peer), true).await?;
        let own = id16(&self.account);
        if let Some(own) = own
            && !self.mls().own_checked
        {
            self.check_contact(None, Some(own), false).await?;
            self.mls().own_checked = true;
        }
        Ok(())
    }

    pub(crate) async fn mls_fetch_welcomes(&mut self) -> Result<(), crate::Error> {
        let device = self.mls().device.clone();
        if device.len() != 16 {
            return Ok(());
        }
        for _ in 0..8 {
            let device_for = device.clone();
            let reply = self
                .ask(kind::MLS_WELCOME_FETCH, move |request_id| {
                    v1::MlsWelcomeFetch {
                        request_id,
                        device: device_for,
                    }
                    .encode_to_vec()
                })
                .await?;
            let welcomes = match reply {
                Reply::Frame(kind::MLS_WELCOMES, payload) => {
                    v1::MlsWelcomes::decode(payload)
                        .map_err(|_| crate::Error::Protocol("invalid welcomes".into()))?
                        .welcomes
                }
                _ => return Ok(()),
            };
            if welcomes.is_empty() {
                return Ok(());
            }
            let mut done = Vec::new();
            for welcome in welcomes.into_iter().take(nuschel::MAX_MLS_WELCOMES) {
                done.push(welcome.id.clone());
                self.mls_join(welcome).await?;
            }
            let device_for = device.clone();
            self.ask(kind::MLS_WELCOME_ACK, move |request_id| {
                v1::MlsWelcomeAck {
                    request_id,
                    device: device_for,
                    ids: done,
                }
                .encode_to_vec()
            })
            .await?;
        }
        Ok(())
    }

    /// Joins one Welcome (fetched or pushed); failures are reported, and the
    /// Welcome is acknowledged either way by the caller.
    async fn mls_join(&mut self, welcome: v1::MlsWelcome) -> Result<(), crate::Error> {
        let Some(conversation) = id16(&welcome.conversation) else {
            return Ok(());
        };
        if !self.mls().directs.contains_key(&conversation) {
            self.mls_directs().await?;
        }
        let Some((peer, _)) = self.mls().directs.get(&conversation).copied() else {
            self.mls_emit(MlsEvent::Failed(
                "A Welcome for a conversation that is not one of your direct conversations was ignored."
                    .into(),
            ))
            .await;
            return Ok(());
        };
        self.mls_rosters(peer).await?;
        let tree = match self.mls_join_info(conversation, true).await? {
            Some(info) => info.tree,
            None => return Ok(()),
        };
        match self
            .mls_worker(CryptoRequest::MlsWelcome {
                server: self.server.key.clone(),
                conversation: conversation.to_vec(),
                peer: peer.to_vec(),
                welcome: welcome.welcome,
                tree,
                seq: welcome.seq,
            })
            .await
        {
            Ok(CryptoResponse::MlsConversation(view)) => {
                let cursor = self.mls().cursors.entry(conversation).or_insert(0);
                *cursor = (*cursor).max(view.cursor.max(welcome.seq));
                self.mls_view(view).await;
                self.mls_sync(conversation).await?;
            }
            Ok(_) => {}
            Err(error) => {
                self.mls_emit(MlsEvent::Failed(format!(
                    "An invitation to an end-to-end conversation was refused: {error}"
                )))
                .await
            }
        }
        Ok(())
    }

    async fn mls_join_info(
        &mut self,
        conversation: [u8; 16],
        tree: bool,
    ) -> Result<Option<v1::MlsJoinInfo>, crate::Error> {
        match self
            .ask(kind::MLS_JOIN_INFO_REQUEST, move |request_id| {
                v1::MlsJoinInfoRequest {
                    request_id,
                    conversation: conversation.to_vec(),
                    tree,
                }
                .encode_to_vec()
            })
            .await?
        {
            Reply::Frame(kind::MLS_JOIN_INFO, payload) => {
                Ok(Some(v1::MlsJoinInfo::decode(payload).map_err(|_| {
                    crate::Error::Protocol("invalid join info".into())
                })?))
            }
            _ => Ok(None),
        }
    }

    async fn mls_claim(&mut self, account: [u8; 16]) -> Result<Vec<Vec<u8>>, crate::Error> {
        match self
            .ask(kind::MLS_KEY_PACKAGE_CLAIM, move |request_id| {
                v1::MlsKeyPackageClaim {
                    request_id,
                    account: account.to_vec(),
                }
                .encode_to_vec()
            })
            .await?
        {
            Reply::Frame(kind::MLS_KEY_PACKAGES, payload) => {
                let mut packages = v1::MlsKeyPackages::decode(payload)
                    .map_err(|_| crate::Error::Protocol("invalid key packages".into()))?
                    .key_packages;
                packages.truncate(nuschel::MAX_MLS_CLAIM_PACKAGES);
                Ok(packages)
            }
            _ => Ok(Vec::new()),
        }
    }

    /// Sends a commit the worker made, and tells the worker the answer.
    async fn mls_commit(&mut self, outgoing: MlsOutgoing) -> Result<Option<u64>, crate::Error> {
        let Some(conversation) = id16(&outgoing.conversation) else {
            return Ok(None);
        };
        let message = v1::MlsCommit {
            request_id: 0,
            conversation: outgoing.conversation.clone(),
            idempotency_key: outgoing.idempotency_key.clone(),
            commit: outgoing.commit.clone(),
            group_info: outgoing.group_info.clone(),
            welcome: outgoing.welcome.clone(),
            create: outgoing.create.as_ref().map(|init| v1::MlsGroupInit {
                group_info: init.group_info.clone(),
                tree: init.tree.clone(),
            }),
        };
        let reply = self
            .ask(kind::MLS_COMMIT, move |request_id| {
                v1::MlsCommit {
                    request_id,
                    ..message
                }
                .encode_to_vec()
            })
            .await?;
        let (accepted, seq) = match reply {
            Reply::Frame(kind::MLS_ACK, payload) => {
                let ack = v1::MlsAck::decode(payload)
                    .map_err(|_| crate::Error::Protocol("invalid acknowledgement".into()))?;
                // Application messages stored before our commit are of the old
                // epoch: read them before the commit is merged, which forgets that
                // epoch's secrets (max_past_epochs = 0). A new group has none.
                if outgoing.create.is_none() {
                    self.mls_rows(conversation, ack.seq.saturating_sub(1))
                        .await?;
                }
                (true, ack.seq)
            }
            Reply::Error(error) => {
                if error.code != Code::StaleEpoch as i32 {
                    self.mls_emit(MlsEvent::Failed(format!(
                        "The server refused a change to the conversation: {}",
                        error.detail
                    )))
                    .await;
                }
                (false, 0)
            }
            Reply::Frame(..) => (false, 0),
        };
        if let CryptoResponse::MlsConversation(view) = self
            .mls_worker(CryptoRequest::MlsCommitDone {
                server: self.server.key.clone(),
                conversation: conversation.to_vec(),
                accepted,
                seq,
            })
            .await?
        {
            if accepted {
                self.mls().cursors.insert(conversation, seq);
            }
            self.mls_view(view).await;
        }
        Ok(accepted.then_some(seq))
    }

    /// Opens the direct conversation with `user_id`, creating its group if it has
    /// none.
    async fn mls_open_direct(&mut self, user_id: u32) -> Result<(), crate::Error> {
        let reply = self
            .ask(kind::OPEN_DIRECT, move |request_id| {
                v1::OpenDirect {
                    request_id,
                    user_id,
                }
                .encode_to_vec()
            })
            .await?;
        let direct = match reply {
            Reply::Frame(kind::DIRECT_OPENED, payload) => v1::DirectOpened::decode(payload)
                .ok()
                .and_then(|opened| opened.direct),
            Reply::Error(error) => {
                self.mls_emit(MlsEvent::Failed(error.detail)).await;
                return Ok(());
            }
            Reply::Frame(..) => None,
        };
        let Some((conversation, peer)) = direct
            .as_ref()
            .and_then(|direct| Some((id16(&direct.conversation)?, id16(&direct.account)?)))
        else {
            self.mls_emit(MlsEvent::Failed(
                "The server did not open the direct conversation.".into(),
            ))
            .await;
            return Ok(());
        };
        self.mls()
            .directs
            .insert(conversation, (peer, Some(user_id)));
        self.mls_rosters(peer).await?;
        let view = self.mls_status(conversation).await?;
        if view.pinned {
            self.mls_view(view).await;
            return self.mls_sync(conversation).await;
        }
        // A group made by the peer (or our other device) arrives as a Welcome.
        if self.mls_join_info(conversation, false).await?.is_some() {
            self.mls_fetch_welcomes().await?;
            let view = self.mls_status(conversation).await?;
            self.mls_view(view).await;
            return Ok(());
        }
        let mut key_packages = self.mls_claim(peer).await?;
        if let Some(own) = id16(&self.account) {
            key_packages.extend(self.mls_claim(own).await?);
        }
        let outgoing = match self
            .mls_worker(CryptoRequest::MlsCreateDirect {
                server: self.server.key.clone(),
                conversation: conversation.to_vec(),
                peer: peer.to_vec(),
                key_packages,
            })
            .await
        {
            Ok(CryptoResponse::MlsOutgoing(outgoing)) => outgoing,
            Ok(_) => return Err(crate::Error::Worker("unexpected worker answer".into())),
            Err(error) => {
                self.mls_emit(MlsEvent::Failed(format!(
                    "The end-to-end conversation could not be created: {error}"
                )))
                .await;
                let view = self.mls_status(conversation).await?;
                self.mls_view(view).await;
                return Ok(());
            }
        };
        if self.mls_commit(outgoing).await?.is_none() {
            // Someone else created it meanwhile: their Welcome is in our mailbox.
            self.mls_fetch_welcomes().await?;
            let view = self.mls_status(conversation).await?;
            self.mls_view(view).await;
        }
        Ok(())
    }

    /// Hands the worker every row after the cursor, in order, then adds devices
    /// the worker found missing.
    pub(crate) async fn mls_sync(&mut self, conversation: [u8; 16]) -> Result<(), crate::Error> {
        self.mls_rows(conversation, u64::MAX).await?;
        self.mls_add_missing(conversation).await
    }

    /// Rows after the cursor up to `until`, in order.
    async fn mls_rows(&mut self, conversation: [u8; 16], until: u64) -> Result<(), crate::Error> {
        for _ in 0..MAX_PAGES {
            let after = self.mls().cursors.get(&conversation).copied().unwrap_or(0);
            if after >= until {
                return Ok(());
            }
            let reply = self
                .ask(kind::HISTORY_REQUEST, move |request_id| {
                    v1::HistoryRequest {
                        request_id,
                        conversation: conversation.to_vec(),
                        anchor: Some(v1::history_request::Anchor::After(after)),
                        limit: nuschel::MAX_PAGE_MESSAGES as u32,
                    }
                    .encode_to_vec()
                })
                .await?;
            let page = match reply {
                Reply::Frame(kind::HISTORY_PAGE, payload) => v1::HistoryPage::decode(payload)
                    .map_err(|_| crate::Error::Protocol("invalid history page".into()))?,
                _ => return Ok(()),
            };
            if page.messages.len() > nuschel::MAX_PAGE_MESSAGES
                || !page.messages.iter().all(nuschel::valid_message)
            {
                return Err(crate::Error::Protocol("invalid history page".into()));
            }
            let more = page.more_after && !page.messages.is_empty();
            for message in page.messages {
                if message.seq > until {
                    return Ok(());
                }
                self.mls_row(conversation, message).await?;
            }
            if !more {
                return Ok(());
            }
        }
        Ok(())
    }

    /// One row of a pinned conversation, in order.
    async fn mls_row(
        &mut self,
        conversation: [u8; 16],
        message: v1::Message,
    ) -> Result<(), crate::Error> {
        let cursor = self.mls().cursors.get(&conversation).copied().unwrap_or(0);
        if message.seq <= cursor {
            return Ok(());
        }
        self.mls().cursors.insert(conversation, message.seq);
        if message.kind == v1::message::Kind::Void as i32 {
            return Ok(());
        }
        if !nuschel::is_mls_format(message.format) {
            // §7.2: plaintext for a pinned conversation is refused.
            self.mls_emit(MlsEvent::PlaintextRefused {
                conversation,
                seq: message.seq,
            })
            .await;
            return Ok(());
        }
        let seq = message.seq;
        match self
            .mls_worker(CryptoRequest::MlsReceive {
                server: self.server.key.clone(),
                conversation: conversation.to_vec(),
                seq,
                message: message.mls,
            })
            .await
        {
            Ok(CryptoResponse::MlsReceived(received)) => {
                self.mls_emit(MlsEvent::Received {
                    conversation,
                    received,
                })
                .await;
            }
            Ok(_) => {}
            Err(error) => {
                self.mls_emit(MlsEvent::Received {
                    conversation,
                    received: Received::Undecryptable {
                        seq,
                        reason: error.to_string(),
                    },
                })
                .await
            }
        }
        Ok(())
    }

    /// Adds devices the worker found in a roster but not in the group.
    async fn mls_add_missing(&mut self, conversation: [u8; 16]) -> Result<(), crate::Error> {
        let view = self.mls_status(conversation).await?;
        if !view.pinned || !view.active || view.missing.is_empty() || view.pending_commit {
            return Ok(());
        }
        let mut key_packages = Vec::new();
        for account in view
            .missing
            .iter()
            .filter_map(|account| id16(account))
            .take(2)
        {
            key_packages.extend(self.mls_claim(account).await?);
        }
        if key_packages.is_empty() {
            return Ok(());
        }
        if let Ok(CryptoResponse::MlsOutgoing(outgoing)) = self
            .mls_worker(CryptoRequest::MlsAddDevices {
                server: self.server.key.clone(),
                conversation: conversation.to_vec(),
                key_packages,
            })
            .await
            && self.mls_commit(outgoing).await?.is_none()
        {
            // Lost to another commit: catch up; the next sync tries again.
            self.mls_rows(conversation, u64::MAX).await?;
        }
        Ok(())
    }

    async fn mls_send(
        &mut self,
        conversation: [u8; 16],
        text: String,
        key: [u8; 16],
    ) -> Result<(), crate::Error> {
        for _ in 0..MAX_RESENDS {
            let encrypted = self
                .mls_worker(CryptoRequest::MlsEncrypt {
                    server: self.server.key.clone(),
                    conversation: conversation.to_vec(),
                    idempotency_key: key.to_vec(),
                    text: text.clone(),
                })
                .await;
            let message = match encrypted {
                Ok(CryptoResponse::MlsCiphertext { message, .. }) => message,
                Ok(_) => return Err(crate::Error::Worker("unexpected worker answer".into())),
                Err(error) => {
                    self.mls_emit(MlsEvent::NotSent {
                        conversation,
                        key,
                        reason: error.to_string(),
                    })
                    .await;
                    return Ok(());
                }
            };
            let reply = self
                .ask(kind::MLS_SEND, move |request_id| {
                    v1::MlsSend {
                        request_id,
                        conversation: conversation.to_vec(),
                        idempotency_key: key.to_vec(),
                        message,
                    }
                    .encode_to_vec()
                })
                .await?;
            match reply {
                Reply::Frame(kind::MLS_ACK, payload) => {
                    let ack = v1::MlsAck::decode(payload)
                        .map_err(|_| crate::Error::Protocol("invalid acknowledgement".into()))?;
                    self.mls_worker(CryptoRequest::MlsSent {
                        server: self.server.key.clone(),
                        conversation: conversation.to_vec(),
                        idempotency_key: key.to_vec(),
                        seq: ack.seq,
                    })
                    .await?;
                    let cursor = self.mls().cursors.entry(conversation).or_insert(0);
                    if ack.seq == *cursor + 1 {
                        *cursor = ack.seq;
                    }
                    self.mls_emit(MlsEvent::Sent {
                        conversation,
                        key,
                        seq: ack.seq,
                    })
                    .await;
                    return Ok(());
                }
                Reply::Error(error) if error.code == Code::StaleEpoch as i32 => {
                    // Catch up to the new epoch, then the same envelope again.
                    self.mls_rows(conversation, u64::MAX).await?;
                }
                Reply::Error(error) => {
                    self.mls_emit(MlsEvent::NotSent {
                        conversation,
                        key,
                        reason: error.detail,
                    })
                    .await;
                    return Ok(());
                }
                Reply::Frame(..) => return Err(crate::Error::Protocol("unexpected reply".into())),
            }
        }
        self.mls_emit(MlsEvent::NotSent {
            conversation,
            key,
            reason: "The conversation kept changing; try again.".into(),
        })
        .await;
        Ok(())
    }

    pub(crate) async fn mls_command(&mut self, command: MlsCommand) -> Result<(), crate::Error> {
        match command {
            MlsCommand::OpenDirect { user_id } => self.mls_open_direct(user_id).await,
            MlsCommand::Send {
                conversation,
                text,
                key,
            } => self.mls_send(conversation, text, key).await,
            MlsCommand::Status { conversation } => {
                let view = self.mls_status(conversation).await?;
                self.mls_view(view).await;
                Ok(())
            }
            MlsCommand::Sync { conversation } => self.mls_sync(conversation).await,
        }
    }

    /// A push from the server: a Welcome for us, or a row of a conversation.
    pub(crate) async fn mls_push(
        &mut self,
        kind: u16,
        payload: bytes::Bytes,
    ) -> Result<(), crate::Error> {
        match kind {
            kind::MLS_WELCOME_PUSH => {
                let Some(welcome) = v1::MlsWelcomePush::decode(payload)
                    .ok()
                    .and_then(|push| push.welcome)
                else {
                    return Ok(());
                };
                let id = welcome.id.clone();
                self.mls_join(welcome).await?;
                let device = self.mls().device.clone();
                self.ask(kind::MLS_WELCOME_ACK, move |request_id| {
                    v1::MlsWelcomeAck {
                        request_id,
                        device,
                        ids: vec![id],
                    }
                    .encode_to_vec()
                })
                .await?;
            }
            kind::CHAT_EVENT => {
                let Ok(event) = v1::ChatEvent::decode(payload) else {
                    return Ok(());
                };
                let (Some(conversation), Some(message)) =
                    (id16(&event.conversation), event.message)
                else {
                    return Ok(());
                };
                let cursor = match self.mls().cursors.get(&conversation) {
                    Some(cursor) => *cursor,
                    None => {
                        let view = self.mls_status(conversation).await?;
                        if !view.pinned {
                            // No group of ours yet: its Welcome comes first.
                            return Ok(());
                        }
                        view.cursor
                    }
                };
                if message.seq == cursor + 1 {
                    self.mls_row(conversation, message).await?;
                    if self.mls_status(conversation).await?.missing.is_empty() {
                        return Ok(());
                    }
                    self.mls_add_missing(conversation).await?;
                } else if message.seq > cursor {
                    self.mls_sync(conversation).await?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}
