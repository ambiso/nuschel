// SPDX-License-Identifier: EUPL-1.2
//! Session-scoped knocking and compatible-server admission. No ACL or storage
//! record is changed. The history floor applies even if ACLs later grant Enter.
//! MLS admission is refused until cryptographic no-history joins are supported.
use crate::{authz::Action, state::State};
use nuschl_protocol::{
    ControlFrame, control, kind as stock,
    nuschl::{capability, kind, v1},
};
use prost::Message as _;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

const TTL: Duration = Duration::from_secs(300);
const COOLDOWN: Duration = Duration::from_secs(10);
const MAX_PENDING: usize = 4;
type Failure = (v1::error::Code, u64);

#[derive(Clone)]
struct Pending {
    id: u64,
    session: u32,
    channel: u32,
    created: Instant,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        acl::{AclEntry, perm},
        state::Channel,
    };

    #[test]
    fn requests_coalesce_expire_and_bound_per_session_fanout() {
        let mut h = crate::fuzzing::Harness::new();
        let requester = h
            .state
            .users
            .values()
            .find(|u| u.name == "Anonymous")
            .unwrap()
            .session;
        for id in 100..105 {
            let mut c = Channel::new(id, Some(0), format!("Private{id}"));
            c.node.acls.push(AclEntry {
                apply_here: true,
                apply_subs: true,
                user_id: None,
                group: "all".into(),
                grant: 0,
                deny: perm::ENTER,
            });
            h.state.conversations.add_channel(c.conversation_id, id);
            h.state.channels.0.insert(id, c);
        }
        let id = h.state.knock_request(requester, 100).unwrap();
        assert_eq!(h.state.knock_request(requester, 100), Ok(id));
        assert_eq!(h.state.knocks.pending.len(), 1);
        assert_eq!(
            h.state.knock_request(requester, 101),
            Err((v1::error::Code::RateLimited, 10_000))
        );
        for channel in 101..104 {
            h.state
                .knocks
                .last
                .insert(requester, Instant::now() - COOLDOWN);
            h.state.knock_request(requester, channel).unwrap();
        }
        h.state
            .knocks
            .last
            .insert(requester, Instant::now() - COOLDOWN);
        assert_eq!(
            h.state.knock_request(requester, 104),
            Err((v1::error::Code::TooManyRequests, 0))
        );
        assert_eq!(h.state.knocks.pending.len(), MAX_PENDING);
        for pending in h.state.knocks.pending.values_mut() {
            pending.created = Instant::now() - TTL;
        }
        h.state.sweep_knocks_at(Instant::now());
        assert!(h.state.knocks.pending.is_empty());
        assert!(h.state.knock_decide(1, id, true).is_err());
        h.state.knock_request(requester, 104).unwrap();
        h.state.disconnect(requester, false);
        assert!(h.state.knocks.pending.is_empty());
        assert!(!h.state.knocks.last.contains_key(&requester));
    }
}

#[derive(Clone)]
pub struct Grant {
    pub id: u64,
    pub channel: u32,
    pub moderator: u32,
    pub first_seq: u64,
}

#[derive(Default)]
pub struct Knocks {
    pending: HashMap<u64, Pending>,
    last: HashMap<u32, Instant>,
    pub(crate) grants: HashMap<u32, Grant>,
    next_sweep: Option<Instant>,
}

impl State {
    pub fn admission_floor(&self, session: u32, channel: u32) -> Option<u64> {
        self.knocks
            .grants
            .get(&session)
            .filter(|grant| grant.channel == channel)
            .map(|grant| grant.first_seq)
    }

    fn knock_capable(&self, session: u32) -> bool {
        self.users
            .get(&session)
            .and_then(|u| u.extension.as_ref())
            .is_some_and(|e| e.has(capability::KNOCK))
    }

    fn knock_notice(&self, session: u32, text: String) {
        // Escape all names: stock TextMessage is HTML, never trusted markup.
        let text = text
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        self.send(
            session,
            stock::TEXT_MESSAGE,
            &control::TextMessage {
                session: vec![session],
                message: text,
                ..Default::default()
            },
        );
    }

    pub(crate) fn knock_sweep(&mut self) {
        let now = Instant::now();
        if self.knocks.next_sweep.is_some_and(|next| now < next) {
            return;
        }
        self.sweep_knocks_at(now);
    }

    fn sweep_knocks_at(&mut self, now: Instant) {
        // `finish` runs for tunneled voice as well: never scan all pending
        // knocks on each packet. Decisions check their own expiry precisely.
        self.knocks.next_sweep = Some(now + Duration::from_secs(1));
        let expired: Vec<_> = self
            .knocks
            .pending
            .values()
            .filter(|p| {
                now.saturating_duration_since(p.created) >= TTL
                    || !self.users.contains_key(&p.session)
                    || self.channels.get(p.channel).is_none()
            })
            .map(|p| p.id)
            .collect();
        for id in expired {
            self.resolve_knock(id, None);
        }
        self.knocks
            .last
            .retain(|session, _| self.users.contains_key(session));
    }

    pub(crate) fn knock_request(&mut self, session: u32, channel: u32) -> Result<u64, Failure> {
        use v1::error::Code;
        self.knock_sweep();
        if self.end_to_end() {
            return Err((Code::Unsupported, 0));
        }
        self.authorize(session, Action::Knock { channel })?;
        if let Some(p) = self
            .knocks
            .pending
            .values()
            .find(|p| p.session == session && p.channel == channel)
        {
            return Ok(p.id);
        }
        let now = Instant::now();
        if self
            .knocks
            .last
            .get(&session)
            .is_some_and(|last| now.saturating_duration_since(*last) < COOLDOWN)
        {
            return Err((Code::RateLimited, COOLDOWN.as_millis() as u64));
        }
        if self
            .knocks
            .pending
            .values()
            .filter(|p| p.session == session)
            .count()
            >= MAX_PENDING
        {
            return Err((Code::TooManyRequests, 0));
        }
        let id = loop {
            let id = u64::from_be_bytes(crate::state::random());
            if id != 0 && !self.knocks.pending.contains_key(&id) {
                break id;
            }
        };
        self.knocks.last.insert(session, now);
        self.knocks.pending.insert(
            id,
            Pending {
                id,
                session,
                channel,
                created: now,
            },
        );
        let requester = self
            .users
            .get(&session)
            .map(|u| u.name.as_str())
            .unwrap_or("");
        let channel_name = self
            .channels
            .get(channel)
            .map(|c| c.name.as_str())
            .unwrap_or("");
        for &recipient in self.users.keys() {
            if !self.allowed(recipient, Action::ModerateKnock { channel }) {
                continue;
            }
            if self.knock_capable(recipient) {
                self.send(
                    recipient,
                    kind::KNOCK_NOTICE,
                    &v1::KnockNotice {
                        knock_id: id,
                        session,
                        channel,
                    },
                );
            } else {
                self.knock_notice(recipient, format!("{requester} (session {session}) knocks on {channel_name}. Move them into that channel to admit them temporarily without earlier history."));
            }
        }
        if !self.knock_capable(session) {
            self.knock_notice(session, "Knock sent; a moderator may admit you.".into());
        }
        Ok(id)
    }

    fn resolve_knock(&mut self, id: u64, grant: Option<Grant>) {
        let Some(p) = self.knocks.pending.remove(&id) else {
            return;
        };
        let message = v1::KnockAdmission {
            knock_id: id,
            session: p.session,
            channel: p.channel,
            admitted_by: grant.as_ref().map_or(0, |g| g.moderator),
            first_seq: grant.as_ref().map_or(0, |g| g.first_seq),
            active: grant.is_some(),
        };
        for &recipient in self.users.keys() {
            if (recipient == p.session
                || self.allowed(recipient, Action::ModerateKnock { channel: p.channel }))
                && self.knock_capable(recipient)
            {
                self.send(recipient, kind::KNOCK_ADMISSION, &message);
            }
        }
        if !self.knock_capable(p.session) && grant.is_none() {
            self.knock_notice(p.session, "Your knock was closed.".into());
        }
    }

    pub(crate) fn knock_decide(&mut self, actor: u32, id: u64, admit: bool) -> Result<(), Failure> {
        use v1::error::Code;
        self.knock_sweep();
        let p = self
            .knocks
            .pending
            .get(&id)
            .cloned()
            .ok_or((Code::NotFound, 0))?;
        if Instant::now().saturating_duration_since(p.created) >= TTL {
            self.resolve_knock(id, None);
            return Err((Code::NotFound, 0));
        }
        self.authorize(actor, Action::ModerateKnock { channel: p.channel })?;
        if !admit {
            self.resolve_knock(id, None);
            return Ok(());
        }
        if self.end_to_end() {
            return Err((Code::Unsupported, 0));
        }
        // Stock moving rules additionally require Move in the source channel.
        self.authorize(
            actor,
            Action::Move {
                user: p.session,
                to: p.channel,
            },
        )?;
        if self.channel_full(p.channel, Some(actor)) {
            return Err((Code::Busy, 1000));
        }
        if self
            .users
            .get(&p.session)
            .is_none_or(|u| u.channel == p.channel)
        {
            return Err((Code::NotFound, 0));
        }
        let conversation = self
            .conversation_of_channel(p.channel)
            .ok_or((Code::NotFound, 0))?;
        let first_seq = self
            .conversations
            .head(&conversation)
            .checked_add(1)
            .ok_or((Code::LimitReached, 0))?;
        let grant = Grant {
            id,
            channel: p.channel,
            moderator: actor,
            first_seq,
        };
        self.knocks.grants.insert(p.session, grant.clone());
        let mut message = control::UserState {
            session: Some(p.session),
            actor: Some(actor),
            channel_id: Some(p.channel),
            ..Default::default()
        };
        self.enter_channel(p.session, p.channel, &mut message);
        self.broadcast(stock::USER_STATE, &message, None);
        self.resolve_knock(id, Some(grant));
        let name = self
            .users
            .get(&actor)
            .map(|u| u.name.as_str())
            .unwrap_or("");
        self.knock_notice(p.session, format!("Admitted by {name}; access lasts until you leave or disconnect. Earlier history is unavailable."));
        Ok(())
    }

    /// A moderator using a stock client admits a pending requester by moving it.
    pub(crate) fn stock_knock_move(&mut self, actor: u32, target: u32, channel: u32) -> bool {
        let id = self
            .knocks
            .pending
            .values()
            .find(|p| p.session == target && p.channel == channel)
            .map(|p| p.id);
        let Some(id) = id else {
            return false;
        };
        if self.allowed(target, Action::Enter { channel }) {
            return false;
        }
        if self.knock_decide(actor, id, true).is_err() {
            self.knock_notice(
                actor,
                "Temporary admission was refused; check current permissions and channel capacity."
                    .into(),
            );
        }
        true
    }

    pub(crate) fn knock_left(&mut self, session: u32, destination: Option<u32>) {
        if self
            .knocks
            .grants
            .get(&session)
            .is_some_and(|g| Some(g.channel) != destination)
            && let Some(grant) = self.knocks.grants.remove(&session)
        {
            let message = v1::KnockAdmission {
                knock_id: grant.id,
                session,
                channel: grant.channel,
                admitted_by: grant.moderator,
                first_seq: grant.first_seq,
                active: false,
            };
            for &recipient in self.users.keys() {
                if self.knock_capable(recipient)
                    && (recipient == session
                        || self.allowed(
                            recipient,
                            Action::ModerateKnock {
                                channel: grant.channel,
                            },
                        ))
                {
                    self.send(recipient, kind::KNOCK_ADMISSION, &message);
                }
            }
        }
        if destination.is_none() {
            let ids: Vec<_> = self
                .knocks
                .pending
                .values()
                .filter(|p| p.session == session)
                .map(|p| p.id)
                .collect();
            for id in ids {
                self.resolve_knock(id, None);
            }
            self.knocks.last.remove(&session);
        }
    }

    pub(crate) fn knock_frame(&mut self, session: u32, frame: &ControlFrame) {
        let request_id = v1::Done::decode(frame.payload.clone()).map_or(0, |m| m.request_id);
        self.immediate(session, request_id, |state| {
            use v1::error::Code;
            match frame.kind {
                kind::KNOCK_REQUEST => {
                    let m = v1::KnockRequest::decode(frame.payload.clone())
                        .map_err(|_| (Code::Malformed, 0))?;
                    let id = state.knock_request(session, m.channel)?;
                    state.send(
                        session,
                        kind::KNOCK_PENDING,
                        &v1::KnockPending {
                            request_id,
                            knock_id: id,
                            channel: m.channel,
                        },
                    );
                }
                kind::KNOCK_DECIDE => {
                    let m = v1::KnockDecide::decode(frame.payload.clone())
                        .map_err(|_| (Code::Malformed, 0))?;
                    state.knock_decide(session, m.knock_id, m.admit)?;
                    state.send(session, kind::DONE, &v1::Done { request_id });
                }
                kind::KNOCK_CANCEL => {
                    let m = v1::KnockCancel::decode(frame.payload.clone())
                        .map_err(|_| (Code::Malformed, 0))?;
                    if !state
                        .knocks
                        .pending
                        .get(&m.knock_id)
                        .is_some_and(|p| p.session == session)
                    {
                        return Err((Code::NotFound, 0));
                    }
                    state.resolve_knock(m.knock_id, None);
                    state.send(session, kind::DONE, &v1::Done { request_id });
                }
                _ => return Err((Code::Unsupported, 0)),
            }
            Ok(())
        });
    }
}
