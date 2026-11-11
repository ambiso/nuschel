// SPDX-License-Identifier: EUPL-1.2
//! Your voice across servers (docs/clients.md, "Voice on several servers"):
//! a green microphone ring on the talk server in the rail and a headphone
//! badge on each server you listen to, pulsing while someone talks there;
//! the voice card's "Talking on … · listening to …", whose parts take you
//! there; and the question before your voice goes to a second server.
use crate::{
    assets::icon,
    dialogs::{Tone, mix, modal, styled_button},
    layout,
    motion::Ease,
    state::{Desktop, ServerView, Status},
    theme::*,
};
use gpui_kit::{Role as Role_, prelude::*, *};
use nuschl_client::{Client, SecondServer, model::TextTarget, voice::Role};
use std::{collections::BTreeMap, time::Duration};

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
    /// A timer redraws breathing badges (see `BREATH_FRAME`).
    breathing: bool,
}

struct Rings {
    /// The talk ring's presence, and its colour (green while you are live).
    ring: Ease,
    live: Ease,
    /// The listening badge: breathing while someone talks there.
    pulse: Ease,
}

impl Motion {
    /// Retarget every server's ring and badge: whether rings move (every
    /// frame), and whether badges breathe (at `BREATH_FRAME`).
    pub fn animate(&mut self, client: &Client, reduce: bool) -> (bool, bool) {
        self.servers
            .retain(|id, _| client.servers.iter().any(|server| &server.id == id));
        let (mut moving, mut breathing) = (false, false);
        for (index, server) in client.servers.iter().enumerate() {
            let role = client.voice_role(index);
            let talk = matches!(role, Role::Talking | Role::Muted);
            let live = role == Role::Talking;
            let busy = role == Role::Listening && someone_talks(server);
            let rings = self
                .servers
                .entry(server.id.clone())
                .or_insert_with(|| Rings {
                    ring: Ease::new(talk),
                    live: Ease::new(live),
                    pulse: Ease::taking(false, PULSE),
                });
            rings.ring.set(talk, reduce);
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
            moving |= rings.ring.moving() || rings.live.moving();
            breathing |= rings.pulse.moving();
        }
        (moving, breathing)
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

/// How a server's rail button names its part in your voice.
pub fn rail_label(role: Role, busy: bool) -> &'static str {
    match (role, busy) {
        (Role::Talking, _) => "talking here",
        (Role::Muted, _) => "voice here, muted",
        (Role::Listening, true) => "listening, someone is talking",
        (Role::Listening, false) => "listening",
        (Role::Deafened, _) => "deafened",
        (Role::Offline, _) => "",
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
    let (name, foreground, background) = match this.client.voice_role(index) {
        Role::Talking | Role::Muted => (
            if live > 0.5 { "mic" } else { "mic-off" },
            mix(TEXT_MUTED, WHITE, live),
            mix(ELEVATED, SUCCESS, live),
        ),
        Role::Listening => (
            "headphones",
            mix(TEXT_MUTED, WHITE, pulse),
            mix(ELEVATED, SUCCESS, pulse * 0.85),
        ),
        Role::Deafened | Role::Offline => return None,
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

/// One server in the voice card: where you are on it.
struct Place {
    server: usize,
    channel: Option<u32>,
    label: String,
    role: Role,
}

/// Every connected server's place, the talk servers first.
fn places(this: &Desktop) -> (Vec<Place>, Vec<Place>) {
    let (mut talk, mut listen) = (Vec::new(), Vec::new());
    for (index, server) in this.client.servers.iter().enumerate() {
        if server.status != Status::Connected {
            continue;
        }
        let role = this.client.voice_role(index);
        let channel = server.voice_channel();
        let label = match channel.and_then(|id| {
            server
                .channels
                .iter()
                .find(|channel| channel.target == TextTarget::Channel(id))
        }) {
            Some(channel) if channel.id != 0 => format!("{} #{}", server.name, channel.name),
            _ => server.name.clone(),
        };
        let place = Place {
            server: index,
            channel,
            label,
            role,
        };
        match role {
            Role::Talking | Role::Muted => talk.push(place),
            Role::Deafened if this.client.voice.is_talk(&server.id) => talk.push(place),
            Role::Listening => listen.push(place),
            Role::Deafened | Role::Offline => {}
        }
    }
    (talk, listen)
}

/// "Talking on Alpha #Lobby · listening to Beta #Music", as it is read out.
fn summary(talk: &[Place], listen: &[Place]) -> String {
    let names = |places: &[&Place]| {
        places
            .iter()
            .map(|place| place.label.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut parts = Vec::new();
    for (role, verb) in [
        (Role::Talking, "talking on"),
        (Role::Muted, "muted on"),
        (Role::Deafened, "deafened on"),
    ] {
        let these: Vec<&Place> = talk.iter().filter(|place| place.role == role).collect();
        if !these.is_empty() {
            parts.push(format!("{verb} {}", names(&these)));
        }
    }
    if !listen.is_empty() {
        let these: Vec<&Place> = listen.iter().collect();
        parts.push(format!("listening to {}", names(&these)));
    }
    let mut text = parts.join(" · ");
    if let Some(first) = text.get(..1) {
        let upper = first.to_uppercase();
        text.replace_range(..1, &upper);
    }
    text
}

/// A place in the voice card that takes you to its channel.
fn place_link(
    id: (&'static str, usize),
    place: &Place,
    accent: Token,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let (server, channel) = (place.server, place.channel);
    let go = move |this: &mut Desktop, window: &mut Window, cx: &mut Context<Desktop>| {
        match channel {
            Some(channel) => this.browse_channel(server, channel, window, cx),
            None => this.browse_server(server, window, cx),
        }
        cx.notify();
    };
    let key = go;
    div()
        .id(id)
        .role(Role_::Button)
        .tab_index(0)
        .aria_label(format!("Go to {}", place.label))
        .tooltip({
            let full = place.label.clone();
            move |_, cx| {
                cx.new(|_| crate::dialogs::Tooltip(full.clone().into()))
                    .into()
            }
        })
        .w_full()
        .min_w_0()
        .flex_1()
        .flex_basis(px(0.))
        .truncate()
        .cursor_pointer()
        .rounded(px(4.))
        .text_color(color(accent))
        .hover(|style| style.bg(color(HOVER)))
        .on_click(cx.listener(move |this, _, window, cx| go(this, window, cx)))
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                key(this, window, cx);
                cx.stop_propagation();
            }
        }))
        .child(text!(place.label.clone()).with_id(("voice-place-label", id.1)))
        .into_any_element()
}

/// The voice card above the user panel: where your voice goes, on every
/// server; each place takes you there. `selected` is the server in front,
/// which the hang-up button disconnects.
pub fn card(
    this: &Desktop,
    selected: usize,
    window: &mut Window,
    cx: &mut Context<Desktop>,
) -> Option<AnyElement> {
    let (talk, listen) = places(this);
    if talk.is_empty() && listen.is_empty() {
        return None;
    }
    let text = summary(&talk, &listen);
    let mut rows = Vec::new();
    for (index, place) in talk.iter().chain(listen.iter()).enumerate() {
        let (verb, accent) = match place.role {
            Role::Talking => ("Talking on", SUCCESS),
            Role::Muted => ("Muted on", TEXT),
            Role::Deafened => ("Deafened on", TEXT_MUTED),
            _ => ("Listening to", TEXT_MUTED),
        };
        rows.push(
            div()
                .w_full()
                .min_w_0()
                .flex_shrink_0()
                .flex()
                .flex_col()
                .gap(px(1.))
                .child(
                    div()
                        .text_color(color(TEXT_FAINT))
                        .text_xs()
                        .child(text!(verb).with_id(("voice-route-role", index))),
                )
                .child(layout::row().child(place_link(("voice-place", index), place, accent, cx)))
                .into_any_element(),
        );
    }
    let issue = this
        .client
        .servers
        .iter()
        .find(|server| server.status == Status::Connected)
        .and_then(|server| server.audio.health.issues().first().map(|issue| issue.0));
    let connected = this
        .client
        .servers
        .get(selected)
        .is_some_and(|server| server.status == Status::Connected);
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
                    .child(layout::fixed().child(icon("signal", 18., color(SUCCESS))))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .flex_basis(px(0.))
                            .font_weight(FontWeight::BOLD)
                            .text_sm()
                            .child(layout::label("voice-connection-status", "Voice connected")),
                    )
                    .when(connected, |row| {
                        row.child(layout::fixed().child(crate::dialogs::icon_button(
                            "disconnect",
                            "hang-up",
                            "Disconnect",
                            TEXT_MUTED,
                            None,
                            move |this, _, cx| {
                                this.disconnect(selected);
                                cx.notify();
                            },
                            cx,
                        )))
                    }),
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
            .when_some(issue, |card, issue| {
                card.child(
                    layout::row()
                        .id("audio-issue")
                        .role(Role_::Button)
                        .aria_label(format!("{issue}; open audio settings"))
                        .tab_index(0)
                        .cursor_pointer()
                        .text_xs()
                        .text_color(color(WARNING))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.open_settings();
                            cx.notify();
                        }))
                        .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                this.open_settings();
                                cx.stop_propagation();
                                cx.notify();
                            }
                        }))
                        .child(layout::fixed().child(icon("alert", 12., color(WARNING))))
                        .child(layout::label("audio-issue-label", issue)),
                )
            })
            .into_any_element(),
    )
}

/// "Speak on two servers?": unmuting here while your voice goes elsewhere.
pub fn second_server_dialog(this: &Desktop, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    let ask = this.second_server.as_ref()?;
    let name = |index: usize| {
        this.client
            .servers
            .get(index)
            .map_or_else(String::new, |server| server.name.clone())
    };
    let here = name(ask.server);
    let elsewhere = ask
        .talking
        .iter()
        .map(|index| name(*index))
        .collect::<Vec<_>>()
        .join(" and ");
    let detail = format!(
        "You are talking on {elsewhere}. Your voice can go to {here} as well, or only to {here}."
    );
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
    pub fn set_mute(&mut self, deafen: bool) {
        if let Some((index, _)) = self.selected_indices() {
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
        self.second_server = question;
    }

    pub fn answer_second_server(&mut self, both: bool) {
        if let Some(ask) = self.second_server.take() {
            self.client.speak_here(ask.server, both);
        }
    }

    pub fn use_voice_here(&mut self, index: usize) {
        self.client.use_voice_here(index);
    }

    /// Keep the rail's rings and badges moving toward the voice state.
    pub fn animate_voice(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (moving, breathing) = self.voice_motion.animate(&self.client, cx.reduce_motion());
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
    use super::{Place, Role, summary};

    fn place(label: &str, role: Role) -> Place {
        Place {
            server: 0,
            channel: None,
            label: label.into(),
            role,
        }
    }

    #[test]
    fn the_summary_says_where_you_talk_and_listen() {
        let talk = [place("Alpha #Lobby", Role::Talking)];
        let listen = [
            place("Beta #Music", Role::Listening),
            place("Gamma", Role::Listening),
        ];
        assert_eq!(
            summary(&talk, &listen),
            "Talking on Alpha #Lobby · listening to Beta #Music, Gamma"
        );
        assert_eq!(summary(&[], &listen), "Listening to Beta #Music, Gamma");
        let both = [
            place("Alpha #Lobby", Role::Talking),
            place("Beta", Role::Muted),
        ];
        assert_eq!(
            summary(&both, &[]),
            "Talking on Alpha #Lobby · muted on Beta"
        );
    }
}
