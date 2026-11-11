// SPDX-License-Identifier: EUPL-1.2
//! Authorisation: the one place that decides whether an actor may do something
//! (docs/reviews/2026-11-11-server-security.md §3, structural decision 1).
//!
//! A handler asks [`State::authorize`] whether a connected user (the actor, by
//! session) may perform an [`Action`], which names its target, and gets `Ok` or
//! a [`Denial`] in the terms its protocol needs: the `PermissionDenied` stock
//! Murmur sends (with the same permission, channel and session, or the same
//! type), a request Murmur ignores silently, or the extension's uniform
//! `not_found` (missing and forbidden look the same). [`State::check`] sends
//! the stock denial and answers `false`; extension handlers turn a denial into
//! their error with `?`.
//!
//! The ACL engine ([`acl`], Murmur's `ACL.cpp` and `Group.cpp`) is evaluated
//! only here: its evaluation functions are private to this module, as are the
//! effective-permission cache and the lookups built on it. What this module
//! also owns: reporting effective permissions to clients (`PermissionQuery`,
//! `ServerSync`), keeping `suppress` in step with Speak after a change
//! (Murmur's `clearACLCache`) and listeners in step with Listen, and which
//! part of a conversation a reader may see (`since_access`).
//!
//! What is not decided here: limits and validation (sizes, counts, rate
//! limits, name rules, a channel's user limit), which depend on the request
//! rather than on who sends it; and membership (who is in a channel or listens
//! to it, a direct conversation's two accounts), which Mumble uses to deliver
//! text and voice without asking the ACLs.
use crate::{
    config::{GuestHistory, KtHistoryAccess},
    conversations::{Id, Kind},
    state::{Registered, State, User},
};
use nuschel_ipc::server::HistoryVisibility;
use nuschel_protocol::{control as proto, kind, nuschel::v1::error::Code};
use proto::permission_denied::DenyType;
use std::{cell::RefCell, collections::HashMap};

pub mod acl;

use acl::{Subject, perm};

/// SuperUser's registration number: all permissions but Speak and Whisper, and
/// nobody else may act on it.
pub const SUPERUSER: u32 = 0;

/// Whether a login skips the server's admission limits (`users` and
/// `certificate_required`): only SuperUser does, as in Murmur.
pub fn exempt_from_admission(user_id: Option<u32>) -> bool {
    user_id == Some(SUPERUSER)
}

/// Effective permissions per (session, channel), cleared whenever anything
/// they depend on changes ([`State::invalidate`]).
#[derive(Default)]
pub struct Cache(RefCell<HashMap<(u32, u32), u32>>);

impl Cache {
    pub fn clear(&mut self) {
        self.0.get_mut().clear();
    }
}

/// Something an actor wants to do, with its target. Sessions name connected
/// users, channels are channel ids, conversations and accounts are 128-bit ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action<'a> {
    // ---- Mumble: presence and voice ----
    /// Be in `channel`: the first channel after login, the one a user lands in
    /// when theirs is removed, and `ChannelState.can_enter`.
    Enter {
        channel: u32,
    },
    /// Move `user` (oneself or someone else) into `to`.
    Move {
        user: u32,
        to: u32,
    },
    /// Join `channel` although it is full (Write there).
    IgnoreUserLimit {
        channel: u32,
    },
    /// `user` listens to `channel` (evaluated for `user`, as in Murmur).
    Listen {
        user: u32,
        channel: u32,
    },
    /// Speak in `channel`: one's own (otherwise suppressed) or a linked one.
    Speak {
        channel: u32,
    },
    /// Whisper or shout into `channel`.
    Whisper {
        channel: u32,
    },
    /// Whisper to `user` (Whisper in their channel).
    WhisperUser {
        user: u32,
    },
    /// A text message to `channel`, or to a tree that starts there.
    TextChannel {
        channel: u32,
    },
    /// A text message to `user` (TextMessage in their channel).
    TextUser {
        user: u32,
    },

    // ---- Mumble: users ----
    /// Any `UserState` change to `user`: SuperUser changes only itself.
    ChangeUser {
        user: u32,
    },
    /// Rename `user`: names never change through `UserState`.
    RenameUser {
        user: u32,
    },
    /// Server mute, deafen, suppress or priority speaker of `user`; `suppress`
    /// when the request sets suppress (never allowed), `muting` when it touches
    /// mute, deafen or suppress (in a temporary channel those need Mute/Deafen
    /// in the first permanent parent).
    SpeakState {
        user: u32,
        suppress: bool,
        muting: bool,
    },
    /// Set `user`'s comment; someone else's may only be cleared.
    SetComment {
        user: u32,
        clearing: bool,
    },
    /// Set `user`'s avatar; someone else's may only be cleared.
    SetTexture {
        user: u32,
        clearing: bool,
    },
    /// Register `user` (oneself: Register Self).
    Register {
        user: u32,
    },
    /// Self-mute, self-deafen, plugin data, recording and listeners: only for
    /// oneself (others' requests are ignored).
    SelfState {
        user: u32,
    },
    Kick {
        user: u32,
    },
    Ban {
        user: u32,
    },
    /// The list of registered users (`UserList`): query, rename, unregister.
    ManageRegistrations,
    /// Rename or unregister one registration through `UserList`.
    EditRegistration {
        user_id: u32,
    },
    /// The ban list (`BanList`).
    ManageBans,
    /// Look up registered names (`QueryUsers`): Write in some channel.
    QueryUsers,
    /// `UserStats` of `user` at all (Enter in their channel, unless details).
    UserStats {
        user: u32,
    },
    /// Their connection statistics (the same channel, or details).
    UserStatsLocal {
        user: u32,
    },
    /// Their certificates, address and version (oneself, or Ban at the root).
    UserStatsDetails {
        user: u32,
    },

    // ---- Mumble: channels ----
    MakeChannel {
        parent: u32,
        temporary: bool,
    },
    /// Description, position and user limit of `channel`, and Write there.
    EditChannel {
        channel: u32,
    },
    RenameChannel {
        channel: u32,
    },
    MoveChannel {
        channel: u32,
        to: u32,
    },
    /// Link or unlink from `channel`; a new link needs this on both ends.
    LinkChannel {
        channel: u32,
    },
    RemoveChannel {
        channel: u32,
    },
    /// Read or replace `channel`'s ACL and groups.
    EditAcl {
        channel: u32,
    },

    // ---- Extension (docs/extension.md) ----
    /// `can_read` (§5): history pages, subscriptions.
    ReadHistory(&'a Id),
    /// See message `seq` of a conversation live, as a reader already in its
    /// audience: always in the channel, otherwise within its visible ranges.
    SeeLive {
        conversation: &'a Id,
        seq: u64,
    },
    /// Post to a conversation (`ChatSend`).
    Send(&'a Id),
    /// Message an account's direct conversation, existing or not.
    SendDirect {
        peer: &'a Id,
    },
    /// Edit one's own message (authorship is checked by the storage).
    Edit(&'a Id),
    /// Delete one's own message, even after losing access.
    DeleteOwn(&'a Id),
    /// Delete anyone's message as a moderator (`moderators_delete`).
    Moderate(&'a Id),
    /// A channel's extension settings: visibility and retention.
    ChannelSettings {
        channel: u32,
    },
    ReadMarker(&'a Id),
    Typing(&'a Id),
    /// Open (or create) the direct conversation with `peer`.
    OpenDirect {
        peer: &'a Id,
    },
    ListDirects,

    // ---- Key transparency, kt/1 (docs/e2ee.md §5) ----
    KtStatus,
    /// Publish a binding for one's own account.
    KtPublish,
    /// Look up `account`'s binding: any registered account of this server.
    KtLookup {
        account: &'a Id,
    },
    /// `account`'s binding history: its owner and contacts (`kt.history`).
    KtHistory {
        account: &'a Id,
    },
    KtAudit,
    KtHead,

    // ---- MLS, mls/1 (docs/e2ee.md §6.3) ----
    /// Upload key packages for one's own devices (registered accounts; the
    /// delivery service checks them against the account's key-transparency
    /// record).
    MlsUpload,
    /// Claim `account`'s key packages: one's own, or a direct-conversation
    /// partner's.
    MlsClaim {
        account: &'a Id,
    },
    /// Create `conversation`'s group or commit to it: those who may post there.
    MlsCommit(&'a Id),
    /// An application message to `conversation`'s group.
    MlsSend(&'a Id),
    /// One's own devices' Welcome mailboxes.
    MlsMailbox,
    /// `conversation`'s GroupInfo and tree: its readers.
    MlsJoinInfo(&'a Id),
}

/// Why an action is refused, in the protocol's terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Denial {
    /// Murmur's `PERM_DENIED`: `session` lacks `permission` in `channel`.
    Permission {
        session: u32,
        channel: u32,
        permission: u32,
    },
    /// A `PermissionDenied` of this type alone (`PERM_DENIED_TYPE`).
    Type(DenyType),
    /// `PERM_DENIED_HASH`: `session` needs a certificate.
    MissingCertificate { session: u32 },
    /// Refused without an answer (Murmur ignores the request).
    Ignored,
    /// The extension's answer for missing and forbidden alike.
    NotFound,
}

impl From<Denial> for (Code, u64) {
    fn from(_: Denial) -> Self {
        (Code::NotFound, 0)
    }
}

fn found(allowed: bool) -> Result<(), Denial> {
    if allowed {
        Ok(())
    } else {
        Err(Denial::NotFound)
    }
}

/// What groups and ACLs match on for a connected user.
fn subject(user: &User) -> Subject<'_> {
    Subject {
        user_id: user.user_id,
        channel: user.channel,
        hash: user.certificate_hash.as_deref(),
        tokens: &user.tokens,
        strong: user.strong,
    }
}

impl State {
    /// May `actor` (a session) do `action`?
    pub fn authorize(&self, actor: u32, action: Action<'_>) -> Result<(), Denial> {
        use Action as A;
        use Denial::{Ignored, MissingCertificate, Type};
        let Some(user) = self.users.get(&actor) else {
            return Err(Ignored);
        };
        match action {
            A::Enter { channel } => self.require(actor, channel, perm::ENTER),
            A::Move { user: target, to } => {
                let from = self.channel_of(target)?;
                if actor != target {
                    self.require(actor, from, perm::MOVE)?;
                }
                if self.has(actor, to, perm::MOVE) || self.has(target, to, perm::ENTER) {
                    Ok(())
                } else {
                    Err(Denial::Permission {
                        session: target,
                        channel: to,
                        permission: perm::ENTER,
                    })
                }
            }
            A::IgnoreUserLimit { channel } => self.require(actor, channel, perm::WRITE),
            A::Listen {
                user: target,
                channel,
            } => self.require(target, channel, perm::LISTEN),
            A::Speak { channel } => self.require(actor, channel, perm::SPEAK),
            A::Whisper { channel } => self.require(actor, channel, perm::WHISPER),
            A::WhisperUser { user: target } => {
                self.require(actor, self.channel_of(target)?, perm::WHISPER)
            }
            A::TextChannel { channel } => self.require(actor, channel, perm::TEXT_MESSAGE),
            A::TextUser { user: target } => {
                self.require(actor, self.channel_of(target)?, perm::TEXT_MESSAGE)
            }

            A::ChangeUser { user: target } => {
                if self.superuser(target) && !self.superuser(actor) {
                    Err(Type(DenyType::SuperUser))
                } else {
                    Ok(())
                }
            }
            A::RenameUser { .. } => Err(Type(DenyType::UserName)),
            A::SpeakState {
                user: target,
                suppress,
                muting,
            } => {
                let channel = self.channel_of(target)?;
                if self.superuser(target) {
                    return Err(Type(DenyType::SuperUser));
                }
                if suppress || !self.has(actor, channel, perm::MUTE_DEAFEN) {
                    return Err(Denial::Permission {
                        session: actor,
                        channel,
                        permission: perm::MUTE_DEAFEN,
                    });
                }
                if muting && self.channels.get(channel).is_some_and(|c| c.temporary) {
                    // In a temporary channel, Mute/Deafen is needed in the first
                    // permanent ancestor, so a channel's creator cannot rule it alone.
                    let mut current = Some(channel);
                    while let Some(id) = current {
                        match self.channels.get(id) {
                            Some(channel) if channel.temporary => current = channel.parent(),
                            _ => break,
                        }
                    }
                    if !current.is_some_and(|id| self.has(actor, id, perm::MUTE_DEAFEN)) {
                        return Err(Type(DenyType::TemporaryChannel));
                    }
                }
                Ok(())
            }
            A::SetComment {
                user: target,
                clearing,
            }
            | A::SetTexture {
                user: target,
                clearing,
            } => {
                if actor == target {
                    return Ok(());
                }
                self.require(actor, 0, perm::RESET_USER_CONTENT)?;
                if clearing {
                    Ok(())
                } else {
                    Err(Type(DenyType::TextTooLong))
                }
            }
            A::Register { user: target } => {
                let target_user = self.users.get(&target).ok_or(Ignored)?;
                let permission = if actor == target {
                    perm::SELF_REGISTER
                } else {
                    perm::REGISTER
                };
                if target_user.user_id.is_some() || !self.has(actor, 0, permission) {
                    return Err(Denial::Permission {
                        session: actor,
                        channel: 0,
                        permission,
                    });
                }
                if target_user.certificate_hash.is_none() {
                    return Err(MissingCertificate { session: target });
                }
                Ok(())
            }
            A::SelfState { user: target } => {
                if actor == target {
                    Ok(())
                } else {
                    Err(Ignored)
                }
            }
            A::Kick { user: target } | A::Ban { user: target } => {
                let permission = if matches!(action, A::Ban { .. }) {
                    perm::BAN
                } else {
                    perm::BAN | perm::KICK
                };
                if self.superuser(target) || !self.has(actor, 0, permission) {
                    return Err(Denial::Permission {
                        session: actor,
                        channel: 0,
                        permission,
                    });
                }
                Ok(())
            }
            A::ManageRegistrations => self.require(actor, 0, perm::REGISTER),
            A::EditRegistration { user_id } => {
                if user_id == SUPERUSER {
                    Err(Ignored)
                } else {
                    Ok(())
                }
            }
            A::ManageBans => self.require(actor, 0, perm::BAN),
            A::QueryUsers => {
                if self
                    .channels
                    .0
                    .keys()
                    .any(|&channel| self.has(actor, channel, perm::WRITE))
                {
                    Ok(())
                } else {
                    Err(Ignored)
                }
            }
            A::UserStatsDetails { user: target } => {
                if actor == target || self.has(actor, 0, perm::BAN) {
                    Ok(())
                } else {
                    Err(Ignored)
                }
            }
            A::UserStats { user: target } => {
                let channel = self.channel_of(target)?;
                if self
                    .authorize(actor, A::UserStatsDetails { user: target })
                    .is_ok()
                {
                    return Ok(());
                }
                self.require(actor, channel, perm::ENTER)
            }
            A::UserStatsLocal { user: target } => {
                if self
                    .authorize(actor, A::UserStatsDetails { user: target })
                    .is_ok()
                    || self.channel_of(target) == Ok(user.channel)
                {
                    Ok(())
                } else {
                    Err(Ignored)
                }
            }

            A::MakeChannel { parent, temporary } => {
                let permission = if temporary {
                    perm::MAKE_TEMP_CHANNEL
                } else {
                    perm::MAKE_CHANNEL
                };
                self.require(actor, parent, permission)?;
                if user.user_id.is_none() && user.certificate_hash.is_none() {
                    return Err(MissingCertificate { session: actor });
                }
                if self.channels.get(parent).is_some_and(|c| c.temporary) {
                    return Err(Type(DenyType::TemporaryChannel));
                }
                Ok(())
            }
            A::EditChannel { channel } => self.require(actor, channel, perm::WRITE),
            A::RenameChannel { channel } | A::RemoveChannel { channel } => {
                if channel == 0 {
                    return Err(Denial::Permission {
                        session: actor,
                        channel,
                        permission: perm::WRITE,
                    });
                }
                self.require(actor, channel, perm::WRITE)
            }
            A::MoveChannel { channel, to } => {
                if self.channels.get(to).is_some_and(|c| c.temporary) {
                    return Err(Type(DenyType::TemporaryChannel));
                }
                self.require(actor, channel, perm::WRITE)?;
                let permission = if self.channels.get(channel).is_some_and(|c| c.temporary) {
                    perm::MAKE_TEMP_CHANNEL
                } else {
                    perm::MAKE_CHANNEL
                };
                self.require(actor, to, permission)
            }
            A::LinkChannel { channel } => self.require(actor, channel, perm::LINK_CHANNEL),
            A::EditAcl { channel } => {
                // Write in the channel, or Write at the root: administrators can
                // always edit, so a channel's creator cannot lock them out.
                if self.has(actor, channel, perm::WRITE) || self.has(actor, 0, perm::WRITE) {
                    Ok(())
                } else {
                    Err(Denial::Permission {
                        session: actor,
                        channel,
                        permission: perm::WRITE,
                    })
                }
            }

            A::ReadHistory(id) => found(self.can_read(actor, id)),
            A::SeeLive { conversation, seq } => {
                if self.sees(actor, conversation, seq) {
                    Ok(())
                } else {
                    Err(Ignored)
                }
            }
            A::Send(id) | A::Typing(id) => found(self.can_write(actor, id)),
            A::SendDirect { peer } => found(self.may_message(actor, peer)),
            A::Edit(id) => found(user.account.is_some() && self.can_write(actor, id)),
            A::DeleteOwn(id) => {
                found(user.account.is_some() && self.conversations.kind(id).is_some())
            }
            A::Moderate(id) => found(
                self.settings.config.history.moderators_delete
                    && matches!(self.conversations.kind(id), Some(Kind::Channel(channel))
                        if self.has(actor, channel, perm::WRITE)),
            ),
            A::ChannelSettings { channel } => {
                found(self.channels.get(channel).is_some() && self.has(actor, channel, perm::WRITE))
            }
            A::ReadMarker(id) => found(user.account.is_some() && self.can_read(actor, id)),
            A::OpenDirect { peer } => found(
                user.account.is_some_and(|me| me != *peer)
                    && self.registry.by_account(peer).is_some(),
            ),
            A::ListDirects | A::KtPublish => found(user.account.is_some()),

            A::KtStatus | A::KtAudit | A::KtHead => Ok(()),
            A::KtLookup { account } => found(self.registry.by_account(account).is_some()),
            A::KtHistory { account } => found(self.kt_history_allowed(user, account)),

            A::MlsUpload | A::MlsMailbox => found(user.account.is_some()),
            A::MlsClaim { account } => found(user.account.is_some_and(|me| {
                me == *account || self.conversations.direct(me, *account).is_some()
            })),
            A::MlsCommit(id) | A::MlsSend(id) => found(self.can_write(actor, id)),
            A::MlsJoinInfo(id) => found(self.can_read(actor, id)),
        }
    }

    /// [`State::authorize`] as a yes or no.
    pub fn allowed(&self, actor: u32, action: Action<'_>) -> bool {
        self.authorize(actor, action).is_ok()
    }

    /// For stock handlers: authorise, and on refusal send the client Murmur's
    /// `PermissionDenied` (if Murmur sends one) and answer `false`.
    pub fn check(&self, actor: u32, action: Action<'_>) -> bool {
        match self.authorize(actor, action) {
            Ok(()) => true,
            Err(denial) => {
                self.refuse(actor, denial);
                false
            }
        }
    }

    /// Tell `to` why it was refused, as Murmur would.
    pub fn refuse(&self, to: u32, denial: Denial) {
        match denial {
            Denial::Permission {
                session,
                channel,
                permission,
            } => self.send(
                to,
                kind::PERMISSION_DENIED,
                &proto::PermissionDenied {
                    permission: Some(permission),
                    channel_id: Some(channel),
                    session: Some(session),
                    r#type: Some(DenyType::Permission as i32),
                    ..Default::default()
                },
            ),
            Denial::Type(deny) => self.deny_type(to, deny),
            Denial::MissingCertificate { session } => {
                self.deny_with(to, DenyType::MissingCertificate, None, Some(session));
            }
            Denial::Ignored | Denial::NotFound => {}
        }
    }

    // ---- the engine, private to this module ----

    fn permissions(&self, session: u32, channel: u32) -> u32 {
        if let Some(&cached) = self.permission_cache.0.borrow().get(&(session, channel)) {
            return cached;
        }
        let Some(user) = self.users.get(&session) else {
            return perm::NONE;
        };
        let granted = acl::effective_permissions(&self.channels, &subject(user), channel);
        self.permission_cache
            .0
            .borrow_mut()
            .insert((session, channel), granted);
        granted
    }

    fn has(&self, session: u32, channel: u32, permission: u32) -> bool {
        self.permissions(session, channel) & permission != 0
    }

    fn require(&self, session: u32, channel: u32, permission: u32) -> Result<(), Denial> {
        if self.has(session, channel, permission) {
            Ok(())
        } else {
            Err(Denial::Permission {
                session,
                channel,
                permission,
            })
        }
    }

    fn superuser(&self, session: u32) -> bool {
        self.users
            .get(&session)
            .is_some_and(|user| user.user_id == Some(SUPERUSER))
    }

    fn channel_of(&self, session: u32) -> Result<u32, Denial> {
        self.users
            .get(&session)
            .map(|user| user.channel)
            .ok_or(Denial::Ignored)
    }

    /// Whether `session` is in group `group` as seen from `channel` (whisper
    /// and shout targets that name a group). This selects receivers; it is not
    /// a permission.
    pub fn in_group(&self, session: u32, channel: u32, group: &str) -> bool {
        self.users.get(&session).is_some_and(|user| {
            acl::group_applies(&self.channels, channel, channel, group, &subject(user))
        })
    }

    // ---- reporting permissions to clients ----

    /// The permissions `ServerSync` carries: the root's, or all of them for
    /// SuperUser (as Murmur sends).
    pub fn sync_permissions(&self, session: u32) -> u32 {
        if self.superuser(session) {
            perm::ALL
        } else {
            self.permissions(session, 0)
        }
    }

    /// Send `PermissionQuery` for `channel` if it changed since last sent (or
    /// always when the client asked). SuperUser gets none.
    pub fn send_permissions(&mut self, session: u32, channel: u32, requested: bool) {
        let granted = self.permissions(session, channel);
        if self.superuser(session) {
            return;
        }
        let Some(user) = self.users.get_mut(&session) else {
            return;
        };
        if requested {
            user.last_permission_check = Some(channel);
        }
        if requested || user.permission_sent.get(&channel) != Some(&granted) {
            user.permission_sent.insert(channel, granted);
            user.link.message(
                kind::PERMISSION_QUERY,
                &proto::PermissionQuery {
                    channel_id: Some(channel),
                    permissions: Some(granted),
                    flush: None,
                },
            );
        }
    }

    /// After permissions may have changed for `who` (or everyone): Murmur's
    /// `clearACLCache`. Stale permission caches in clients are flushed, and
    /// suppression follows the Speak permission in the user's channel.
    pub fn permissions_changed(&mut self, who: Option<u32>) {
        self.invalidate();
        let sessions: Vec<u32> = match who {
            Some(session) => vec![session],
            None => self.users.keys().copied().collect(),
        };
        for session in sessions {
            self.flush_permission_cache(session);
            let Some(user) = self.users.get(&session) else {
                continue;
            };
            let may_speak = self.allowed(
                session,
                Action::Speak {
                    channel: user.channel,
                },
            );
            if may_speak == user.suppress {
                if let Some(user) = self.users.get_mut(&session) {
                    user.suppress = !may_speak;
                }
                self.broadcast(
                    kind::USER_STATE,
                    &proto::UserState {
                        session: Some(session),
                        suppress: Some(!may_speak),
                        ..Default::default()
                    },
                    None,
                );
            }
        }
    }

    /// A listener whose user no longer has Listen on its channel ends at once
    /// (the user's decision on review D3.4; Murmur keeps it): checked after
    /// every change to authorisation state (`authz_epoch` moved), before the
    /// lock is released ([`State::finish`]), so live text and voice stop and
    /// the channel is no longer readable through it. Everyone learns it as
    /// from Murmur, a `UserState` with `listening_channel_remove` (no actor);
    /// the user also gets a `PermissionDenied` for Listen in that channel
    /// (stock clients log "You were denied Listen privileges in …"), whose
    /// reason Nuschel shows: listening stopped, the permission was removed.
    /// A registered user's remembered listener is forgotten too.
    pub fn revoke_listeners(&mut self) {
        if self.listeners_epoch == self.authz_epoch {
            return;
        }
        let mut revoked: Vec<(u32, Vec<u32>)> = Vec::new();
        for user in self.users.values() {
            let channels: Vec<u32> = user
                .listening
                .keys()
                .copied()
                .filter(|&channel| {
                    !self.allowed(
                        user.session,
                        Action::Listen {
                            user: user.session,
                            channel,
                        },
                    )
                })
                .collect();
            if !channels.is_empty() {
                revoked.push((user.session, channels));
            }
        }
        for (session, channels) in &revoked {
            for &channel in channels {
                self.forget_listener(*session, channel);
            }
            self.broadcast(
                kind::USER_STATE,
                &proto::UserState {
                    session: Some(*session),
                    listening_channel_remove: channels.clone(),
                    ..Default::default()
                },
                None,
            );
            for &channel in channels {
                let name = self
                    .channels
                    .get(channel)
                    .map(|channel| channel.name.clone())
                    .unwrap_or_default();
                self.send(
                    *session,
                    kind::PERMISSION_DENIED,
                    &proto::PermissionDenied {
                        permission: Some(perm::LISTEN),
                        channel_id: Some(channel),
                        session: Some(*session),
                        r#type: Some(DenyType::Permission as i32),
                        reason: Some(format!(
                            "Listening to {name} stopped: the permission to listen there was removed."
                        )),
                        ..Default::default()
                    },
                );
            }
            tracing::info!(user = %self.log_name(*session), ?channels, "ended listeners that lost Listen");
        }
        if !revoked.is_empty() {
            // Whisper and shout target caches list listeners.
            self.generation += 1;
        }
        self.listeners_epoch = self.authz_epoch;
    }

    fn flush_permission_cache(&mut self, session: u32) {
        let Some(user) = self.users.get(&session) else {
            return;
        };
        let sent: Vec<(u32, u32)> = user.permission_sent.iter().map(|(c, p)| (*c, *p)).collect();
        let matches = sent.len() < 20
            && sent.iter().all(|(channel, granted)| {
                self.channels.get(*channel).is_some()
                    && self.permissions(session, *channel) == *granted
            });
        if matches {
            return;
        }
        let channel = user
            .last_permission_check
            .filter(|channel| self.channels.get(*channel).is_some())
            .unwrap_or(user.channel);
        let granted = self.permissions(session, channel);
        if let Some(user) = self.users.get_mut(&session) {
            user.permission_sent.clear();
            user.permission_sent.insert(channel, granted);
            user.last_permission_check = Some(channel);
            user.link.message(
                kind::PERMISSION_QUERY,
                &proto::PermissionQuery {
                    channel_id: Some(channel),
                    permissions: Some(granted),
                    flush: Some(true),
                },
            );
        }
    }

    // ---- conversations (docs/extension.md §5) ----

    fn guests_read(&self) -> bool {
        self.settings.config.history.guests == GuestHistory::Full
    }

    /// `can_read`: Enter on the channel, being in it or listening to it, with
    /// persistent credentials only (temporary access tokens are gone before
    /// subscriptions are re-evaluated); guests only with `history.guests =
    /// "full"`. Direct conversations: their two accounts. A deleted channel's
    /// archive: SuperUser.
    fn can_read(&self, session: u32, id: &Id) -> bool {
        let Some(user) = self.users.get(&session) else {
            return false;
        };
        match self.conversations.kind(id) {
            None => false,
            Some(Kind::Channel(channel)) => {
                (user.account.is_some() || self.guests_read())
                    && (user.channel == channel
                        || user.listening.contains_key(&channel)
                        || self.has(session, channel, perm::ENTER))
            }
            Some(Kind::Direct(a, b)) => user.account.is_some_and(|me| me == a || me == b),
            Some(Kind::Archived) => user.user_id == Some(SUPERUSER),
        }
    }

    /// `can_read` for an account that is not online: its registration and
    /// certificate only (no tokens, in no channel).
    fn offline_reads(&self, registered: &Registered, channel: u32) -> bool {
        let subject = Subject {
            user_id: Some(registered.id),
            channel: u32::MAX,
            hash: registered.certificate_hash.as_deref(),
            tokens: &[],
            strong: false,
        };
        acl::effective_permissions(&self.channels, &subject, channel) & perm::ENTER != 0
    }

    /// Whether an account can read a channel now: through any of its
    /// connections, or by its persistent identity while offline (`online`
    /// maps accounts to their sessions).
    pub fn account_may_read(
        &self,
        online: &HashMap<Id, Vec<u32>>,
        account: &Id,
        channel: u32,
    ) -> bool {
        let Some(id) = self.conversation_of_channel(channel) else {
            return false;
        };
        match online.get(account) {
            Some(sessions) => sessions.iter().any(|session| self.can_read(*session, &id)),
            None => self
                .registry
                .by_account(account)
                .is_some_and(|registered| self.offline_reads(registered, channel)),
        }
    }

    /// May this connection post to the conversation (send, edit, typing)?
    fn can_write(&self, session: u32, id: &Id) -> bool {
        let Some(user) = self.users.get(&session) else {
            return false;
        };
        match self.conversations.kind(id) {
            Some(Kind::Channel(channel)) => self.has(session, channel, perm::TEXT_MESSAGE),
            Some(Kind::Direct(a, b)) => {
                let Some(me) = user.account.filter(|me| *me == a || *me == b) else {
                    return false;
                };
                self.may_message(session, if me == a { &b } else { &a })
            }
            Some(Kind::Archived) | None => false,
        }
    }

    /// Whether `session` may message account `peer` directly: both registered
    /// and different, the peer not deleted (its conversations are frozen), and
    /// Murmur's rule for a message to a user (TextMessage in their channel) for
    /// each of the peer's connections.
    fn may_message(&self, session: u32, peer: &Id) -> bool {
        let Some(user) = self.users.get(&session) else {
            return false;
        };
        user.account.is_some_and(|me| me != *peer)
            && self.registry.by_account(peer).is_some()
            && self
                .users
                .values()
                .filter(|other| other.account == Some(*peer))
                .all(|other| self.has(session, other.channel, perm::TEXT_MESSAGE))
    }

    /// The seq ranges this connection may see, and whether they are clipped
    /// (`since_access`, docs/extension.md §6).
    pub fn visible_ranges(&self, session: u32, id: &Id) -> (Vec<(u64, u64)>, bool) {
        if self.visibility(id) == HistoryVisibility::Members {
            return (vec![(1, u64::MAX)], false);
        }
        let ranges = self
            .users
            .get(&session)
            .and_then(|user| user.account)
            .map(|account| self.conversations.intervals(id, &account))
            .unwrap_or_default();
        (ranges, true)
    }

    /// Whether a connection may see `seq` live: always in the channel's live
    /// audience, otherwise within its visible ranges.
    fn sees(&self, session: u32, id: &Id, seq: u64) -> bool {
        if let Some(Kind::Channel(channel)) = self.conversations.kind(id)
            && self.users.get(&session).is_some_and(|user| {
                user.channel == channel || user.listening.contains_key(&channel)
            })
        {
            return true;
        }
        let (ranges, clipped) = self.visible_ranges(session, id);
        !clipped || ranges.iter().any(|(lo, hi)| (*lo..=*hi).contains(&seq))
    }

    // ---- key transparency ----

    /// Whether `user` may see the binding history of `account`: its owner;
    /// with `kt.history = "members"` everyone; with `"contacts"` those with a
    /// direct conversation with it or in the same channel as one of its
    /// connections now.
    fn kt_history_allowed(&self, user: &User, account: &Id) -> bool {
        if user.account.as_ref() == Some(account) {
            return true;
        }
        if self.registry.by_account(account).is_none() {
            return false;
        }
        match self.kt.as_ref().map(|kt| kt.history) {
            Some(KtHistoryAccess::Members) => true,
            Some(KtHistoryAccess::Contacts) => {
                let direct = user
                    .account
                    .is_some_and(|own| self.conversations.direct(own, *account).is_some());
                direct
                    || self.users.values().any(|other| {
                        other.account.as_ref() == Some(account) && other.channel == user.channel
                    })
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests;
