// SPDX-License-Identifier: EUPL-1.2
//! Exercise the actual session authorization, rate queue and wire receipts.
use super::{
    ActionSubject, Channel, Command, CommandAuthority, SessionEvent, State, TextTarget, User,
};
use nuschl_protocol::{control as proto, kind};
use prost::Message as _;
use tokio::{
    sync::mpsc,
    time::{Duration, Instant},
};

const HTML: &str = "<p>Message <a href=\"https://example.org/a\">link</a></p>";

fn observed() -> State {
    let mut state = State {
        own_session: Some(1),
        ..State::default()
    };
    for (session, instance) in [(1, 11), (2, 22)] {
        state.users.insert(
            session,
            User {
                session,
                instance,
                channel: 5,
                ..User::default()
            },
        );
    }
    for (id, instance) in [(0, 40), (5, 45)] {
        state.channels.insert(
            id,
            Channel {
                id,
                instance,
                ..Channel::default()
            },
        );
    }
    state
}

fn authority(state: &State, target: &TextTarget) -> CommandAuthority {
    let (subject, users, channels) = match target {
        TextTarget::Channel(id) => {
            let instance = state.channels[id].instance;
            (
                Some(ActionSubject::Channel(*id, instance)),
                vec![],
                vec![(*id, instance)],
            )
        }
        TextTarget::Direct(id) => {
            let instance = state.users[id].instance;
            (
                Some(ActionSubject::User(*id, instance)),
                vec![(*id, instance)],
                vec![],
            )
        }
        TextTarget::DirectConversation(_) => unreachable!("fixture uses stock targets"),
    };
    CommandAuthority {
        own: (1, state.users[&1].instance),
        subject,
        users,
        channels,
        conversations: vec![],
        accounts: vec![],
    }
}

fn command(state: &State, id: u64, target: TextTarget, html: &str) -> Command {
    let expected_peer = match target {
        TextTarget::Direct(peer) => Some(state.users[&peer].instance),
        _ => None,
    };
    Command::SendPreview {
        id,
        authority: authority(state, &target),
        target,
        html: html.to_owned(),
        expected_peer,
        limits: state.limits.clone(),
    }
}

fn receipts(events: &[SessionEvent]) -> Vec<(u64, bool)> {
    events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::LinkPreviewSendResult { id, result } => Some((*id, result.is_ok())),
            _ => None,
        })
        .collect()
}

fn drain(events: &mut mpsc::Receiver<SessionEvent>) -> Vec<SessionEvent> {
    let mut result = Vec::new();
    while let Ok(event) = events.try_recv() {
        result.push(event);
    }
    result
}

fn texts(state: &State) -> Vec<proto::TextMessage> {
    state
        .outbound
        .iter()
        .filter(|(tag, _)| *tag == kind::TEXT_MESSAGE)
        .map(|(_, body)| proto::TextMessage::decode(body.as_slice()).unwrap())
        .collect()
}

fn hold(state: &mut State) {
    // Use the real bucket, not a mock queue. No wall-clock sleep is needed.
    state.outbox.charge(Instant::now());
    state.outbox.level = state.outbox.capacity();
}

async fn release(state: &mut State, events: &mpsc::Sender<SessionEvent>) {
    state.outbox.at -= Duration::from_secs(10);
    state.flush_outbox(events).await.unwrap();
}

async fn enqueue(state: &mut State, command: Command, events: &mpsc::Sender<SessionEvent>) {
    let command = state
        .authorize(command)
        .expect("current explicit preview authority");
    state.command(command, events).await.unwrap();
}

#[tokio::test]
async fn valid_preview_emits_one_wire_message_and_one_matching_success_only_after_rate_release() {
    for target in [TextTarget::Channel(5), TextTarget::Direct(2)] {
        let mut state = observed();
        let (tx, mut rx) = mpsc::channel(16);
        hold(&mut state);
        let send = command(&state, 71, target.clone(), HTML);
        enqueue(&mut state, send, &tx).await;
        assert_eq!(state.outbox.waiting.len(), 1);
        assert!(texts(&state).is_empty());
        assert!(receipts(&drain(&mut rx)).is_empty());
        release(&mut state, &tx).await;
        let wire = texts(&state);
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0].message, HTML);
        match target {
            TextTarget::Channel(id) => {
                assert_eq!(wire[0].channel_id, [id]);
                assert!(wire[0].session.is_empty());
            }
            TextTarget::Direct(id) => {
                assert_eq!(wire[0].session, [id]);
                assert!(wire[0].channel_id.is_empty());
            }
            TextTarget::DirectConversation(_) => unreachable!(),
        }
        let events = drain(&mut rx);
        assert_eq!(receipts(&events), [(71, true)]);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, SessionEvent::Text(text) if text.outgoing))
                .count(),
            1
        );
        assert!(state.outbox.waiting.is_empty());
        state.flush_outbox(&tx).await.unwrap();
        assert_eq!(texts(&state).len(), 1);
        assert!(receipts(&drain(&mut rx)).is_empty());
    }
}

#[tokio::test]
async fn queued_channel_preview_rechecks_presence_permissions_and_projection_limits() {
    for change in 0..9 {
        let mut state = observed();
        let (tx, mut rx) = mpsc::channel(16);
        hold(&mut state);
        let send = command(&state, 80 + change, TextTarget::Channel(5), HTML);
        enqueue(&mut state, send, &tx).await;
        assert_eq!(state.outbox.waiting.len(), 1);
        assert!(receipts(&drain(&mut rx)).is_empty());
        match change {
            0 => state.channels.get_mut(&5).unwrap().instance += 1,
            1 => state.users.get_mut(&1).unwrap().instance += 1,
            2 => state.own_session = None,
            3 => {
                state.permissions.insert(5, 0);
            }
            4 => state.limits.allow_html = false,
            5 => state.limits.message_length = 1,
            6 => state.limits.image_message_length = 1,
            7 => {
                state.channels.remove(&5);
            }
            _ => state.extension = Some(vec![nuschl_protocol::nuschl::capability::E2EE.into()]),
        }
        release(&mut state, &tx).await;
        assert!(
            texts(&state).is_empty(),
            "queued change {change} produced TEXT_MESSAGE"
        );
        let events = drain(&mut rx);
        assert_eq!(
            receipts(&events),
            [(80 + change, false)],
            "queued change {change}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, SessionEvent::Text(_)))
        );
        assert!(state.outbox.waiting.is_empty());
    }
}

#[tokio::test]
async fn queued_direct_preview_refuses_replaced_or_removed_peer_and_current_permission_revocation()
{
    for change in 0..3 {
        let mut state = observed();
        let (tx, mut rx) = mpsc::channel(16);
        hold(&mut state);
        let send = command(&state, 101, TextTarget::Direct(2), HTML);
        enqueue(&mut state, send, &tx).await;
        assert!(receipts(&drain(&mut rx)).is_empty());
        match change {
            0 => state.users.get_mut(&2).unwrap().instance += 1,
            1 => {
                state.users.remove(&2);
            }
            _ => {
                state.permissions.insert(5, 0);
            }
        }
        release(&mut state, &tx).await;
        assert!(texts(&state).is_empty());
        assert_eq!(receipts(&drain(&mut rx)), [(101, false)]);
    }
}

#[tokio::test]
async fn unrelated_server_limit_changes_do_not_revoke_a_valid_projection() {
    let mut state = observed();
    let (tx, mut rx) = mpsc::channel(16);
    hold(&mut state);
    let send = command(&state, 111, TextTarget::Channel(5), HTML);
    enqueue(&mut state, send, &tx).await;
    drain(&mut rx);
    state.limits.max_users = Some(99);
    state.limits.recording_allowed = false;
    state.limits.max_bandwidth = Some(24_000);
    release(&mut state, &tx).await;
    assert_eq!(texts(&state).len(), 1);
    assert_eq!(receipts(&drain(&mut rx)), [(111, true)]);
}

#[test]
fn preview_scope_covers_only_its_explicit_target_and_does_not_enable_generic_html_scope() {
    let mut state = observed();
    let preview = command(&state, 121, TextTarget::Channel(5), HTML);
    let lease = authority(&state, &TextTarget::Channel(5));
    let held = lease.for_command(&preview).unwrap();
    assert_eq!(held.channels, [(5, 45)]);
    assert!(held.users.is_empty());
    assert!(state.authorize(preview).is_some());
    for mismatch_kind in 0..3 {
        let mut mismatch = lease.clone();
        match mismatch_kind {
            0 => mismatch.subject = None,
            1 => mismatch.own = (1, 12),
            _ => mismatch.channels = vec![(0, 40)],
        }
        assert!(
            state
                .authorize(Command::Scoped {
                    authority: mismatch,
                    command: Box::new(command(&state, 122, TextTarget::Channel(5), HTML)),
                })
                .is_none()
        );
    }
    let html = Command::SendHtml {
        target: TextTarget::Channel(5),
        html: vec![HTML.into()],
        expected_peer: None,
    };
    assert!(lease.for_command(&html).is_none());
    assert!(
        state
            .authorize(Command::Scoped {
                authority: lease,
                command: Box::new(html)
            })
            .is_none()
    );
    let mut missing = command(&state, 123, TextTarget::Channel(5), HTML);
    let Command::SendPreview { authority, .. } = &mut missing else {
        unreachable!()
    };
    authority.channels.clear();
    assert!(state.authorize(missing).is_none());
}

#[tokio::test]
async fn preview_queue_has_one_item_and_returns_capacity_after_delivery_or_refusal() {
    for revoke in [false, true] {
        let mut state = observed();
        state.limits.message_length = 0;
        state.limits.image_message_length = 0;
        let (tx, mut rx) = mpsc::channel(32);
        hold(&mut state);
        let maximal = "x".repeat(nuschl_core::MAX_CONTENT_BYTES);
        let first = command(&state, 131, TextTarget::Channel(5), &maximal);
        enqueue(&mut state, first, &tx).await;
        assert_eq!(state.outbox.waiting.len(), 1);
        assert_eq!(
            state.outbox.waiting[0].wire.len() + state.outbox.waiting[0].body.len(),
            2 * nuschl_core::MAX_CONTENT_BYTES
        );
        let second = command(&state, 132, TextTarget::Channel(5), HTML);
        enqueue(&mut state, second, &tx).await;
        assert_eq!(state.outbox.waiting.len(), 1);
        assert_eq!(receipts(&drain(&mut rx)), [(132, false)]);
        if revoke {
            state.channels.get_mut(&5).unwrap().instance += 1;
        }
        release(&mut state, &tx).await;
        assert_eq!(receipts(&drain(&mut rx)), [(131, !revoke)]);
        assert_eq!(texts(&state).len(), usize::from(!revoke));
        let third = command(&state, 133, TextTarget::Channel(5), HTML);
        enqueue(&mut state, third, &tx).await;
        release(&mut state, &tx).await;
        assert_eq!(receipts(&drain(&mut rx)), [(133, true)]);
        assert_eq!(texts(&state).len(), usize::from(!revoke) + 1);
    }
}

#[tokio::test]
async fn malformed_preview_admission_has_matching_failure_and_no_wire_or_waiting_item() {
    for html in [
        String::new(),
        "x".repeat(nuschl_core::MAX_CONTENT_BYTES + 1),
    ] {
        let mut state = observed();
        let (tx, mut rx) = mpsc::channel(16);
        let send = command(&state, 141, TextTarget::Channel(5), &html);
        enqueue(&mut state, send, &tx).await;
        assert_eq!(receipts(&drain(&mut rx)), [(141, false)]);
        assert!(texts(&state).is_empty());
        assert!(state.outbox.waiting.is_empty());
    }
    let mut state = observed();
    assert!(
        state
            .authorize(command(&state, 0, TextTarget::Channel(5), HTML))
            .is_none()
    );

    let (tx, mut rx) = mpsc::channel(16);
    let mut wrong_peer = command(&state, 142, TextTarget::Direct(2), HTML);
    let Command::SendPreview { expected_peer, .. } = &mut wrong_peer else {
        unreachable!()
    };
    *expected_peer = Some(23);
    enqueue(&mut state, wrong_peer, &tx).await;
    assert_eq!(receipts(&drain(&mut rx)), [(142, false)]);
    assert!(texts(&state).is_empty());
    assert!(state.outbox.waiting.is_empty());
}

#[tokio::test(start_paused = true)]
async fn expired_preview_is_refused_even_when_the_rate_bucket_still_has_no_capacity() {
    let mut state = observed();
    let (tx, mut rx) = mpsc::channel(16);
    hold(&mut state);
    state.outbox.level = 100.0;
    let send = command(&state, 151, TextTarget::Channel(5), HTML);
    enqueue(&mut state, send, &tx).await;
    assert_eq!(state.outbox.waiting.len(), 1);
    assert!(receipts(&drain(&mut rx)).is_empty());
    tokio::time::advance(Duration::from_secs(30)).await;
    assert!(!state.outbox.ready(Instant::now()));
    state.flush_outbox(&tx).await.unwrap();
    assert_eq!(receipts(&drain(&mut rx)), [(151, false)]);
    assert!(texts(&state).is_empty());
    assert!(state.outbox.waiting.is_empty());
    // Refusal frees the preview slot even though ordinary rate credit has not
    // returned, so a fresh intent can now wait for its own bounded interval.
    let send = command(&state, 152, TextTarget::Channel(5), HTML);
    enqueue(&mut state, send, &tx).await;
    assert_eq!(state.outbox.waiting.len(), 1);
    assert!(receipts(&drain(&mut rx)).is_empty());
}

#[tokio::test]
async fn actual_wire_queue_receipt_precedes_a_backpressured_own_text_event() {
    let mut state = observed();
    let (tx, mut rx) = mpsc::channel(1);
    let send = command(&state, 161, TextTarget::Channel(5), HTML);
    let consume = async {
        assert!(matches!(
            rx.recv().await,
            Some(SessionEvent::LinkPreviewSendResult {
                id: 161,
                result: Ok(())
            })
        ));
        assert!(
            matches!(rx.recv().await, Some(SessionEvent::Text(text)) if text.outgoing && text.body == HTML)
        );
    };
    tokio::join!(enqueue(&mut state, send, &tx), consume);
    assert_eq!(texts(&state).len(), 1);
    assert!(receipts(&drain(&mut rx)).is_empty());
}
