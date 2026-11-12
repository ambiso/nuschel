// SPDX-License-Identifier: EUPL-1.2
//! Your voice across servers (docs/clients.md, "Voice on several servers"):
//! a green microphone ring on the talk server in the rail and a headphone
//! badge on each server you listen to, pulsing while someone talks there;
//! a server-scoped voice card distinguishing destination intent from observed
//! speech, whose rows take you there; and the question before your voice goes to a second server.
use crate::{
    assets::icon,
    dialogs::{Tone, mix, modal, styled_button},
    layout,
    motion::Ease,
    state::{Desktop, ServerView, Status},
    theme::*,
};
use gpui_kit::{Role as Role_, prelude::*, *};
use nuschl_client::{Client, SecondServer, model::TextTarget};
use std::{collections::BTreeMap, time::Duration};

mod status;
use status::{Availability, Connection, Facts, State};

/// Largest rail decoration extension (mention badges). Layout reserves this
/// plus an antialiasing gutter for every tile, not just the first/last item.
pub const RAIL_DECORATION_OUTSET: f32 = 6.;

/// Half a breath of a listening badge's pulse.
const PULSE: Duration = Duration::from_millis(700);
/// How often a breathing badge is drawn: smooth enough for a slow pulse,
/// without redrawing the window at the display's rate for as long as
/// someone talks.
const BREATH_FRAME: Duration = Duration::from_millis(40);

/// The rail's animations, by server id.
#[derive(Default)]
pub struct Motion {
    servers: BTreeMap<String, Rings>,
    hovered: Option<String>,
    dragged: Option<String>,
    /// A timer redraws breathing badges (see `BREATH_FRAME`).
    breathing: bool,
}

struct Rings {
    /// Shared roundedness for tile, image clipping and its outline.
    rounded: Ease,
    /// The talk ring's presence, and its colour (green while you are live).
    ring: Ease,
    live: Ease,
    /// The listening badge: breathing while someone talks there.
    pulse: Ease,
}

impl Motion {
    /// Retarget every server's ring and badge: whether rings move (every
    /// frame), and whether badges breathe (at `BREATH_FRAME`).
    pub fn animate(&mut self, client: &Client, push_muted: bool, reduce: bool) -> (bool, bool) {
        self.servers
            .retain(|id, _| client.servers.iter().any(|server| &server.id == id));
        let (mut moving, mut breathing) = (false, false);
        for server in &client.servers {
            let rounded = self.dragged.as_deref() == Some(server.id.as_str())
                || self.hovered.as_deref() == Some(server.id.as_str())
                || client
                    .navigation
                    .selection()
                    .is_some_and(|selected| selected.server == server.id);
            let state = projected(client, server, push_muted);
            let talk = client.voice.is_talk(&server.id) && server.status == Status::Connected;
            let live = state.microphone_enabled();
            let busy = state == State::Listening && someone_talks(server);
            let rings = self
                .servers
                .entry(server.id.clone())
                .or_insert_with(|| Rings {
                    rounded: Ease::new(rounded),
                    ring: Ease::new(talk),
                    live: Ease::new(live),
                    pulse: Ease::taking(false, PULSE),
                });
            rings.ring.set(talk, reduce);
            rings.rounded.set(rounded, reduce);
            rings.live.set(live, reduce);
            if busy && !reduce {
                // Back and forth for as long as someone talks.
                if !rings.pulse.moving() {
                    let next = !rings.pulse.target();
                    rings.pulse.set(next, false);
                }
            } else {
                // Without motion, the badge just lights up.
                rings.pulse.set(busy, reduce);
            }
            moving |= rings.rounded.moving() || rings.ring.moving() || rings.live.moving();
            breathing |= rings.pulse.moving();
        }
        (moving, breathing)
    }

    pub fn dragged(&mut self, server: Option<&str>) {
        if self.dragged.as_deref() != server {
            self.dragged = server.map(str::to_owned);
        }
    }

    pub fn hover(&mut self, server: &str, hovered: bool) {
        if hovered {
            self.hovered = Some(server.into());
        } else if self.hovered.as_deref() == Some(server) {
            self.hovered = None;
        }
    }

    pub fn radius(&self, server: &str, selected: bool) -> f32 {
        self.servers
            .get(server)
            .map_or(if selected { 16. } else { 24. }, |motion| {
                24. - 8. * motion.rounded.value()
            })
    }

    fn values(&self, server: &str) -> (f32, f32, f32) {
        self.servers.get(server).map_or((0., 0., 0.), |rings| {
            (rings.ring.value(), rings.live.value(), rings.pulse.value())
        })
    }
}

/// Someone other than you talks on this server.
pub fn someone_talks(server: &ServerView) -> bool {
    server
        .talking
        .iter()
        .any(|session| Some(*session) != server.own_session)
}

/// One current-state projection for card, rail and selected-server footer.
/// This reads bounded model facts; channel labels are resolved only by `places`.
pub fn projected(client: &Client, server: &ServerView, push_muted: bool) -> State {
    let own = server.own_session.and_then(|id| server.users.get(&id));
    Facts {
        connection: match server.status {
            Status::Disconnected => Connection::Offline,
            Status::Connecting if server.reconnecting.is_some() => Connection::Reconnecting,
            Status::Connecting => Connection::Connecting,
            Status::Connected => Connection::Connected,
        },
        availability: if !server.has_mumble_voice() {
            Availability::EndToEnd
        } else {
            match server.opus_available {
                None => Availability::Negotiating,
                Some(true) => Availability::Available,
                Some(false) => Availability::Unsupported,
            }
        },
        destination: client.voice.is_talk(&server.id),
        muted: server.muted,
        deafened: server.deafened,
        server_muted: own.is_some_and(|user| user.muted),
        server_deafened: own.is_some_and(|user| user.deafened),
        suppressed: own.is_some_and(|user| user.suppressed),
        acknowledged: own.is_some_and(|user| {
            user.self_muted == server.muted && user.self_deafened == server.deafened
        }),
        transmit_budget: server
            .limits
            .as_ref()
            .and_then(|limits| limits.max_bandwidth)
            .is_none_or(|bandwidth| nuschl_core::audio::voice_bitrate(bandwidth) != 0),
        push_muted,
        sending: server
            .own_session
            .is_some_and(|id| server.talking.contains(&id)),
    }
    .state()
}

pub fn rail_label(state: State, busy: bool) -> String {
    if state == State::Speaking {
        "Microphone destination · enabled · Speaking".into()
    } else if state == State::Listening && busy {
        "Listening · someone is speaking".into()
    } else if state == State::Offline {
        String::new()
    } else {
        state.label().into()
    }
}

/// The talk ring around a rail button (`radius`: the button's corners).
pub fn rail_ring(this: &Desktop, server: &ServerView, radius: f32) -> Option<AnyElement> {
    let (ring, live, _) = this.voice_motion.values(&server.id);
    (ring > 0.01).then(|| {
        div()
            .absolute()
            .top(px(-4.))
            .left(px(-4.))
            .size(px(56.))
            .rounded(px(radius + 4.))
            .border_2()
            .border_color(mix(TEXT_FAINT, SUCCESS, live))
            .opacity(ring)
            .into_any_element()
    })
}

/// The badge at a rail button's lower left: a microphone on the talk server,
/// headphones where you listen, breathing while someone talks there.
pub fn rail_badge(this: &Desktop, index: usize, server: &ServerView) -> Option<AnyElement> {
    let (_, live, pulse) = this.voice_motion.values(&server.id);
    let state = projected(&this.client, server, this.push_muted);
    let (name, foreground, background) = match state {
        State::MicrophoneEnabled
        | State::Speaking
        | State::Muted
        | State::MutedPending
        | State::ServerMuted
        | State::Suppressed
        | State::NoTransmitBudget
        | State::AwaitingAcknowledgement
        | State::PushMuted => (
            if live > 0.5 { "mic" } else { "mic-off" },
            mix(TEXT_MUTED, WHITE, live),
            mix(ELEVATED, SUCCESS, live),
        ),
        State::Listening | State::ListeningPending => (
            "headphones",
            mix(TEXT_MUTED, WHITE, pulse),
            mix(ELEVATED, SUCCESS, pulse * 0.85),
        ),
        State::Deafened | State::ServerDeafened => {
            ("headphones-off", color(TEXT_MUTED), color(ELEVATED))
        }
        _ => return None,
    };
    Some(
        div()
            .id(("voice-badge", index))
            .absolute()
            .left(px(-4.))
            .bottom(px(-4.))
            .size(px(20.))
            .aspect_square()
            .flex_shrink_0()
            .rounded_full()
            .bg(color(RAIL))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .size(px(16.))
                    .aspect_square()
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(background)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon(name, 12., foreground)),
            )
            .into_any_element(),
    )
}

/// Bind delayed UI actions to the server and connection they name, never a
/// runtime Vec position that removing an earlier bookmark can reuse.
#[derive(Clone)]
pub(crate) struct Target {
    server: String,
    generation: u64,
    own: Option<(u32, u64)>,
    channel: Option<(u32, u64)>,
}

impl Target {
    fn capture(server: &ServerView) -> Self {
        let own = server.own_session.and_then(|id| server.users.get(&id));
        let channel = own.and_then(|user| {
            server
                .channels
                .iter()
                .find(|channel| channel.target == TextTarget::Channel(user.channel))
        });
        Self {
            server: server.id.clone(),
            generation: server.generation,
            own: own.map(|user| (user.session, user.instance)),
            channel: channel.map(|channel| (channel.id, channel.instance)),
        }
    }

    fn resolve(&self, client: &Client) -> Option<usize> {
        self.resolve_in(&client.servers)
    }

    fn resolve_in(&self, servers: &[ServerView]) -> Option<usize> {
        servers
            .iter()
            .position(|server| server.id == self.server)
            .filter(|index| {
                let server = &servers[*index];
                server.generation == self.generation
                    && self.own
                        == server
                            .own_session
                            .and_then(|id| server.users.get(&id))
                            .map(|user| (user.session, user.instance))
            })
    }

    fn resolve_place(&self, client: &Client) -> Option<usize> {
        self.resolve_place_in(&client.servers)
    }

    fn resolve_place_in(&self, servers: &[ServerView]) -> Option<usize> {
        self.resolve_in(servers).filter(|index| {
            let server = &servers[*index];
            server.status == Status::Connected
                && self.channel.is_some_and(|(id, instance)| {
                    server.voice_channel() == Some(id)
                        && server.channels.iter().any(|channel| {
                            channel.target == TextTarget::Channel(id)
                                && channel.instance == instance
                        })
                })
        })
    }

    fn browse(&self, this: &mut Desktop, window: &mut Window, cx: &mut Context<Desktop>) {
        if let Some(index) = self.resolve(&this.client) {
            let server = &this.client.servers[index];
            let channel = (server.status == Status::Connected)
                .then(|| server.voice_channel())
                .flatten();
            match channel {
                Some(channel) => this.browse_channel(index, channel, window, cx),
                None => this.browse_server(index, window, cx),
            }
            cx.notify();
        }
    }
}

pub(crate) struct Place {
    pub(crate) target: Target,
    label: String,
    name: String,
    state: State,
    connected: bool,
    issue: Option<String>,
}

pub(crate) fn places(this: &Desktop) -> Vec<Place> {
    let mut places = Vec::new();
    // Destination rows first, preserving stable IDs without moving live state.
    for destination in [true, false] {
        for server in &this.client.servers {
            if server.status == Status::Disconnected
                || this.client.voice.is_talk(&server.id) != destination
            {
                continue;
            }
            let connected = server.status == Status::Connected;
            let channel = connected
                .then(|| server.voice_channel())
                .flatten()
                .and_then(|id| {
                    server
                        .channels
                        .iter()
                        .find(|channel| channel.target == TextTarget::Channel(id))
                });
            let label = channel.map_or_else(
                || server.name.clone(),
                |channel| format!("{} #{}", server.name, channel.name),
            );
            places.push(Place {
                target: Target {
                    server: server.id.clone(),
                    generation: server.generation,
                    own: server
                        .own_session
                        .and_then(|id| server.users.get(&id))
                        .map(|user| (user.session, user.instance)),
                    channel: channel.map(|channel| (channel.id, channel.instance)),
                },
                label,
                name: server.name.clone(),
                state: projected(&this.client, server, this.push_muted),
                connected,
                issue: server
                    .audio
                    .health
                    .issues()
                    .first()
                    .map(|issue| issue.0.to_owned()),
            });
        }
    }
    places
}

pub(crate) fn target(places: &[Place], server: &str) -> Option<Target> {
    places
        .iter()
        .find(|place| place.target.server == server)
        .map(|place| place.target.clone())
}

fn summary(places: &[Place]) -> String {
    places
        .iter()
        .map(|place| format!("{}: {}", place.label, place.state.label()))
        .collect::<Vec<_>>()
        .join(" · ")
}

fn place_link(place: &Place, cx: &mut Context<Desktop>) -> AnyElement {
    let id = ElementId::from(format!(
        "voice-place-{}-{}-{}",
        place.target.server,
        place.target.generation,
        place.target.own.map_or(0, |(_, instance)| instance)
    ));
    let click = place.target.clone();
    let key = place.target.clone();
    let full = place.label.clone();
    let control = div()
        .id(id.clone())
        .role(Role_::Button)
        .tab_index(0)
        .aria_label(format!("Go to {}", place.label))
        .tooltip(move |_, cx| {
            cx.new(|_| crate::dialogs::Tooltip(full.clone().into()))
                .into()
        })
        .w_full()
        .min_w_0()
        .flex_shrink_0()
        .truncate()
        .cursor_pointer()
        .rounded(px(4.))
        .text_color(color(if place.state.speaking() {
            SUCCESS
        } else {
            TEXT
        }))
        .hover(|style| style.bg(color(HOVER)))
        .on_click(cx.listener(move |this, _, window, cx| click.browse(this, window, cx)))
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                key.browse(this, window, cx);
                cx.stop_propagation();
            }
        }))
        .child(text!(place.label.clone()).with_id((id.clone(), "label")));
    layout::focusable(id, control)
}

/// Every connected/pending voice context, independent of text navigation.
/// Each hang-up names and targets its own connection (voice AND chat).
pub fn card(
    _this: &Desktop,
    places: &[Place],
    window: &mut Window,
    cx: &mut Context<Desktop>,
) -> Option<AnyElement> {
    if places.is_empty() {
        return None;
    }
    let text = summary(places);
    let connected = places.iter().any(|place| place.connected);
    let rows = places
        .iter()
        .map(|place| {
            let id = ElementId::from(format!(
                "voice-route-{}-{}-{}",
                place.target.server,
                place.target.generation,
                place.target.own.map_or(0, |(_, instance)| instance)
            ));
            let target = place.target.clone();
            let status = place.state.label();
            let issue_target = place.target.clone();
            let label = format!("Disconnect from {} (voice and chat)", place.name);
            let action = crate::dialogs::icon_button(
                (id.clone(), "disconnect"),
                "hang-up",
                label,
                TEXT_MUTED,
                None,
                move |this, _, cx| {
                    if let Some(index) = target.resolve(&this.client) {
                        this.disconnect(index);
                        cx.notify();
                    }
                },
                cx,
            );
            layout::row()
                .id(id.clone())
                .role(Role_::Group)
                .aria_label(format!("{}: {}", place.label, status))
                .items_start()
                .flex_shrink_0()
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .flex_basis(px(0.))
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(place_link(place, cx))
                        .child(
                            div()
                                .text_xs()
                                .text_color(color(if place.state.speaking() {
                                    SUCCESS
                                } else {
                                    TEXT_MUTED
                                }))
                                .child(layout::text((id.clone(), "status"), status)),
                        ),
                )
                .child(layout::fixed().child(action))
                .when_some(place.issue.as_ref(), |row, issue| {
                    row.child(layout::fixed().child(crate::dialogs::icon_button(
                        (id.clone(), "audio-issue"),
                        "alert",
                        format!("{}: {issue}; open audio settings", place.name),
                        WARNING,
                        None,
                        move |this, window, cx| {
                            if issue_target.resolve(&this.client).is_some() {
                                issue_target.browse(this, window, cx);
                                this.open_settings();
                                cx.notify();
                            }
                        },
                        cx,
                    )))
                })
                .into_any_element()
        })
        .collect::<Vec<_>>();
    Some(
        div()
            .id("voice-card")
            .role(Role_::Group)
            .aria_label(text)
            .w_full()
            .min_w_0()
            .flex_shrink_0()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(color(BORDER))
            .bg(color(SIDEBAR_FOOTER))
            .flex()
            .flex_col()
            .gap_2()
            .child(
                layout::row()
                    .items_center()
                    .child(layout::fixed().child(icon(
                        "signal",
                        18.,
                        color(if connected { SUCCESS } else { TEXT_MUTED }),
                    )))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .flex_basis(px(0.))
                            .font_weight(FontWeight::BOLD)
                            .text_sm()
                            .child(layout::label(
                                "voice-connection-status",
                                if connected {
                                    "Voice connected"
                                } else {
                                    "Connecting voice"
                                },
                            )),
                    ),
            )
            .child(
                layout::scroll_region(
                    "voice-destinations",
                    "Voice destinations",
                    window.viewport_size().height * 0.20,
                    window,
                    cx,
                )
                .flex()
                .flex_col()
                .gap_2()
                .text_sm()
                .children(rows),
            )
            .into_any_element(),
    )
}

/// An explicit answer is valid only for the named places that were shown.
/// It holds facts, not mutable routing policy; cancellation never sends audio.
pub(crate) struct SecondServerPrompt {
    target: Target,
    target_name: String,
    others: Vec<Target>,
    other_names: Vec<String>,
    destinations: Vec<String>,
}

impl SecondServerPrompt {
    fn capture(client: &Client, ask: SecondServer) -> Option<Self> {
        let server = client.servers.get(ask.server)?;
        let others = ask
            .talking
            .iter()
            .map(|index| client.servers.get(*index))
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            target: Target::capture(server),
            target_name: server.name.clone(),
            other_names: others.iter().map(|server| server.name.clone()).collect(),
            others: others.into_iter().map(Target::capture).collect(),
            destinations: client.voice.talk().to_vec(),
        })
    }

    fn resolve(&self, client: &Client) -> Option<usize> {
        let mut enabled = client
            .servers
            .iter()
            .enumerate()
            .filter(|(index, _)| client.voice_role(*index) == nuschl_client::voice::Role::Talking)
            .map(|(_, server)| server.id.as_str())
            .collect::<Vec<_>>();
        let mut shown = self
            .others
            .iter()
            .map(|target| target.server.as_str())
            .collect::<Vec<_>>();
        enabled.sort_unstable();
        shown.sort_unstable();
        if enabled != shown {
            return None;
        }
        self.resolve_in(&client.servers, client.voice.talk())
    }

    fn resolve_in(&self, servers: &[ServerView], destinations: &[String]) -> Option<usize> {
        if destinations != self.destinations
            || self
                .others
                .iter()
                .any(|target| target.resolve_place_in(servers).is_none())
        {
            return None;
        }
        self.target.resolve_place_in(servers)
    }
}

/// "Speak on two servers?": unmuting here while your voice goes elsewhere.
pub fn second_server_dialog(this: &Desktop, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    let ask = this.second_server.as_ref()?;
    let here = ask.target_name.clone();
    let elsewhere = ask.other_names.join(" and ");
    let detail = format!("Enable your microphone for {here} and {elsewhere}, or only for {here}.");
    let content = div()
        .flex()
        .flex_col()
        .child(
            div()
                .p_6()
                .pb_4()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .text_xl()
                        .font_weight(FontWeight::BOLD)
                        .child(text!("Speak on two servers?").with_id("speak-title")),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(color(TEXT_MUTED))
                        .child(text!(detail).with_id("speak-detail")),
                ),
        )
        .child(
            div()
                .px_6()
                .py_4()
                .bg(color(SIDEBAR))
                .rounded_b(px(12.))
                .flex()
                .gap_3()
                .justify_end()
                .child(styled_button(
                    "speak-cancel",
                    "Cancel",
                    Tone::Ghost,
                    |this, _, cx| {
                        this.second_server = None;
                        cx.notify();
                    },
                    cx,
                ))
                .child(styled_button(
                    "speak-only-here",
                    format!("Only on {here}"),
                    Tone::Secondary,
                    |this, _, cx| {
                        this.answer_second_server(false);
                        cx.notify();
                    },
                    cx,
                ))
                .child(styled_button(
                    "speak-both",
                    "Speak on both",
                    Tone::Primary,
                    |this, _, cx| {
                        this.answer_second_server(true);
                        cx.notify();
                    },
                    cx,
                )),
        )
        .into_any_element();
    Some(modal(
        this,
        "speak-dialog",
        "Speak on two servers?",
        460.,
        content,
        cx,
        |this| this.second_server = None,
    ))
}

impl Desktop {
    /// The footer's microphone or headphones button, on the server in front.
    pub(crate) fn set_mute_target(&mut self, target: &Target, deafen: bool) {
        if let Some(index) = target.resolve(&self.client) {
            let server = &self.client.servers[index];
            let enabling = if deafen {
                server.deafened
            } else {
                server.muted || server.deafened
            };
            if enabling && target.resolve_place(&self.client).is_none() {
                return;
            }
            let question = self.client.set_mute(index, deafen);
            self.ask(question);
        }
    }

    /// The mute and deafen shortcuts: the talk servers.
    pub fn toggle_voice(&mut self, deafen: bool) {
        let ask = self.client.toggle_voice(deafen);
        self.ask(ask);
    }

    fn ask(&mut self, question: Option<SecondServer>) {
        if question.is_some() {
            self.menu = None;
        }
        self.second_server =
            question.and_then(|question| SecondServerPrompt::capture(&self.client, question));
    }

    pub fn answer_second_server(&mut self, both: bool) {
        if let Some(ask) = self.second_server.take() {
            if let Some(index) = ask.resolve(&self.client) {
                self.client.speak_here(index, both);
            }
        }
    }

    pub fn use_voice_here(&mut self, index: usize) {
        self.client.use_voice_here(index);
    }

    /// Keep the rail's rings and badges moving toward the voice state.
    pub fn animate_voice(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (moving, breathing) =
            self.voice_motion
                .animate(&self.client, self.push_muted, cx.reduce_motion());
        if moving {
            window.request_animation_frame();
        } else if breathing && !self.voice_motion.breathing {
            self.voice_motion.breathing = true;
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(BREATH_FRAME).await;
                let _ = this.update(cx, |this, cx| {
                    this.voice_motion.breathing = false;
                    cx.notify();
                });
            })
            .detach();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Place, SecondServerPrompt, ServerView, State, Status, Target, summary};
    use nuschl_client::conversation::ConversationState;
    use nuschl_core::{bookmark::ServerBookmark, model::User};

    #[test]
    fn summary_keeps_each_context_and_never_calls_idle_eligibility_speech() {
        let places = vec![
            Place {
                target: Target {
                    server: "a".into(),
                    generation: 3,
                    own: None,
                    channel: None,
                },
                name: "Alpha".into(),
                label: "Alpha #Lobby".into(),
                state: State::MicrophoneEnabled,
                connected: true,
                issue: None,
            },
            Place {
                target: Target {
                    server: "b".into(),
                    generation: 7,
                    own: None,
                    channel: None,
                },
                name: "Beta".into(),
                label: "Beta #Root".into(),
                state: State::Deafened,
                connected: true,
                issue: None,
            },
        ];
        assert_eq!(
            summary(&places),
            "Alpha #Lobby: Microphone destination · enabled · Beta #Root: Deafened"
        );
    }
    fn server(id: &str) -> ServerView {
        let mut bookmark = ServerBookmark::new(id, "127.0.0.1", 64738, "test").unwrap();
        bookmark.id = id.into();
        let mut server = ServerView::new(bookmark);
        server.status = Status::Connected;
        server.generation = 7;
        server.own_session = Some(1);
        server.users.insert(
            1,
            User {
                session: 1,
                instance: 11,
                channel: 2,
                ..User::default()
            },
        );
        let mut channel = ConversationState::channel(2, Some(0), "Lobby".into(), 0);
        channel.instance = 22;
        server.channels = vec![channel];
        server
    }

    #[test]
    fn targets_survive_index_shift_but_reject_reconnect_and_channel_reuse() {
        let mut servers = vec![server("a"), server("b")];
        let target = Target::capture(&servers[1]);
        assert_eq!(target.resolve_place_in(&servers), Some(1));
        servers.remove(0);
        assert_eq!(target.resolve_place_in(&servers), Some(0));
        servers[0].name = "renamed".into();
        servers[0].channels[0].name = "renamed channel".into();
        assert_eq!(target.resolve_place_in(&servers), Some(0));
        servers[0].channels[0].instance += 1;
        assert_eq!(target.resolve_place_in(&servers), None);
        // Hanging up/navigation is connection-scoped, not a stale channel action.
        assert_eq!(target.resolve_in(&servers), Some(0));
        servers[0].users.get_mut(&1).unwrap().instance += 1;
        assert_eq!(target.resolve_in(&servers), None);
        servers[0] = server("replacement");
        assert_eq!(target.resolve_in(&servers), None);
    }

    #[test]
    fn consent_rejects_a_changed_route_set_or_either_place_lifetime() {
        let mut servers = vec![server("a"), server("b")];
        let routes = vec!["a".into()];
        let prompt = SecondServerPrompt {
            target: Target::capture(&servers[1]),
            target_name: "b".into(),
            others: vec![Target::capture(&servers[0])],
            other_names: vec!["a".into()],
            destinations: routes.clone(),
        };
        assert_eq!(prompt.resolve_in(&servers, &routes), Some(1));
        assert_eq!(prompt.resolve_in(&servers, &["a".into(), "c".into()]), None);
        servers[0].users.get_mut(&1).unwrap().channel = 3;
        assert_eq!(prompt.resolve_in(&servers, &routes), None);
        servers[0] = server("a");
        servers[1].generation += 1;
        assert_eq!(prompt.resolve_in(&servers, &routes), None);
    }
}
