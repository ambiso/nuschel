// SPDX-License-Identifier: EUPL-1.2
//! Voice and audio settings. Changes apply immediately and are saved locally.
use crate::{
    assets::icon,
    dialogs::{Tone, Tooltip, modal, named_button, styled_button},
    preview::{EchoTestState, PreviewView},
    state::{Desktop, ServerView},
    theme::*,
};
use gpui_kit::{prelude::*, *};
use nuschl_core::audio::{AudioDevice, EchoMode, EchoSource, TransmitMode};
use nuschl_session::config::ShortcutAction;
use std::rc::Rc;

pub type Action = Rc<dyn Fn(&mut Desktop, &mut Window, &mut Context<Desktop>)>;
type Choose = Rc<dyn Fn(&mut Desktop, Option<String>)>;

fn activation(event: &KeyDownEvent) -> bool {
    event.keystroke.key == "enter" || event.keystroke.key == "space"
}

/// The control of a setting the system configuration (managed.json) sets:
/// shown with its value, dimmed and out of the pointer's reach, with a note.
/// Whatever reaches it anyway, `Desktop::update_settings` keeps the value.
pub fn managed(this: &Desktop, key: &str, control: AnyElement) -> AnyElement {
    if !this.managed.contains(key) {
        return control;
    }
    div()
        .id(SharedString::from(format!("managed-{key}")))
        .role(Role::Group)
        .aria_label("Set by your system configuration")
        .relative()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().opacity(0.5).child(control))
        .child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .text_xs()
                .text_color(color(TEXT_MUTED))
                .child(icon("lock", 12., color(TEXT_MUTED)))
                .child(text!("Set by your system configuration").with_id("managed-note")),
        )
        .child(
            div()
                .absolute()
                .inset_0()
                .cursor_not_allowed()
                .block_mouse_except_scroll(),
        )
        .into_any_element()
}

fn section(title: &'static str, children: Vec<AnyElement>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::BOLD)
                .text_color(color(TEXT_FAINT))
                .child(text!(title).with_id(title)),
        )
        .children(children)
        .into_any_element()
}

pub fn segmented(
    id: &'static str,
    label: &'static str,
    options: Vec<(&'static str, bool, Action)>,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    div()
        .id(id)
        .role(Role::RadioGroup)
        .aria_label(label)
        .flex()
        .p(px(3.))
        .gap(px(3.))
        .rounded(px(8.))
        .bg(color(APP))
        .border_1()
        .border_color(color(BORDER))
        .children(
            options
                .into_iter()
                .enumerate()
                .map(|(index, (text, selected, action))| {
                    let key_action = action.clone();
                    div()
                        .id((id, index))
                        .role(Role::RadioButton)
                        .aria_label(text)
                        .aria_selected(selected)
                        .tab_index(0)
                        .flex_1()
                        .py(px(6.))
                        .rounded(px(6.))
                        .flex()
                        .justify_center()
                        .text_sm()
                        .cursor_pointer()
                        .text_color(color(if selected { TEXT } else { TEXT_MUTED }))
                        .when(selected, |option| option.bg(color(SELECTED)))
                        .hover(|style| style.text_color(color(TEXT)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            action(this, window, cx);
                            cx.notify();
                        }))
                        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                            if activation(event) {
                                key_action(this, window, cx);
                                cx.notify();
                            }
                        }))
                        .child(text!(text).with_id((id, index + 100)))
                }),
        )
        .into_any_element()
}

pub fn toggle(
    id: &'static str,
    title: &'static str,
    detail: String,
    on: bool,
    action: Action,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let key_action = action.clone();
    div()
        .id(id)
        .role(Role::Switch)
        .aria_label(title)
        .aria_selected(on)
        .tab_index(0)
        .flex()
        .items_center()
        .gap_3()
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, window, cx| {
            action(this, window, cx);
            cx.notify();
        }))
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
            if activation(event) {
                key_action(this, window, cx);
                cx.notify();
            }
        }))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child(text!(title).with_id((id, 1_usize))),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(color(TEXT_MUTED))
                        .child(text!(detail).with_id((id, 2_usize))),
                ),
        )
        .child(
            div()
                .w(px(40.))
                .h(px(22.))
                .flex_shrink_0()
                .rounded_full()
                .p(px(3.))
                .bg(color(if on { SUCCESS } else { BORDER_STRONG }))
                .flex()
                .when(on, |track| track.justify_end())
                .child(div().size(px(16.)).rounded_full().bg(color(WHITE))),
        )
        .into_any_element()
}

fn meter(preview: &PreviewView) -> AnyElement {
    let fraction = ((preview.level_db + 70.) / 70.).clamp(0., 1.);
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .id("input-meter")
                .role(Role::ProgressIndicator)
                .aria_label(format!("Microphone level {:.0} dB", preview.level_db))
                .h(px(8.))
                .w_full()
                .rounded_full()
                .bg(color(APP))
                .overflow_hidden()
                .child(
                    div()
                        .h_full()
                        .w(relative(fraction))
                        .rounded_full()
                        .bg(color(if preview.transmitting {
                            SUCCESS
                        } else {
                            TEXT_FAINT
                        })),
                ),
        )
        .child(
            div().text_xs().text_color(color(TEXT_FAINT)).child(
                text!(if preview.transmitting {
                    format!(
                        "Would transmit · speech {:.0}% · nothing is sent while testing",
                        preview.voice * 100.
                    )
                } else {
                    format!(
                        "Would not transmit · speech {:.0}% · nothing is sent while testing",
                        preview.voice * 100.
                    )
                })
                .with_id("meter-label"),
            ),
        )
        .into_any_element()
}

/// The slider being dragged. Every slider hears every drag of this type, so
/// each one checks that the drag is its own.
struct SliderDrag(&'static str);

/// A 0-100 % slider in 5 % steps (each step is saved).
fn slider(
    id: &'static str,
    label: &'static str,
    value: f32,
    set: fn(&mut Desktop, f32),
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let step =
        move |this: &mut Desktop, value: f32| set(this, (value.clamp(0., 1.) * 20.).round() / 20.);
    fn at(bounds: Bounds<Pixels>, position: Point<Pixels>) -> f32 {
        (position.x - bounds.left()) / bounds.size.width.max(px(1.))
    }
    // The track's bounds as last painted, for a click that is not a drag.
    let track = Rc::new(std::cell::Cell::new(Bounds::default()));
    let painted = track.clone();
    div()
        .id(id)
        .role(Role::Slider)
        .aria_label(label)
        .tab_index(0)
        .relative()
        .h(px(20.))
        .flex()
        .items_center()
        .cursor_pointer()
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
            let value = match event.keystroke.key.as_str() {
                "left" | "down" => value - 0.05,
                "right" | "up" => value + 0.05,
                "home" => 0.,
                "end" => 1.,
                _ => return,
            };
            step(this, value);
            cx.notify();
        }))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                step(this, at(track.get(), event.position));
                cx.notify();
            }),
        )
        .on_drag(SliderDrag(id), |_, _, _, cx| cx.new(|_| EmptyView))
        .on_drag_move(
            cx.listener(move |this, event: &DragMoveEvent<SliderDrag>, _, cx| {
                if event.drag(cx).0 == id {
                    step(this, at(event.bounds, event.event.position));
                    cx.notify();
                }
            }),
        )
        .child(
            canvas(move |bounds, _, _| painted.set(bounds), |_, _, _, _| {})
                .absolute()
                .size_full(),
        )
        .child(
            div()
                .w_full()
                .h(px(6.))
                .rounded_full()
                .bg(color(BORDER_STRONG))
                .child(
                    div()
                        .h_full()
                        .w(relative(value))
                        .rounded_full()
                        .bg(color(ACCENT)),
                ),
        )
        .child(
            div()
                .absolute()
                .left(relative(value))
                .ml(px(-8.))
                .size(px(16.))
                .rounded_full()
                .bg(color(WHITE))
                .border_2()
                .border_color(color(ACCENT))
                .shadow_sm(),
        )
        .into_any_element()
}

/// A slider under a label that shows its value; `name` is for screen readers.
fn labelled_slider(
    id: &'static str,
    (label, name): (&'static str, &'static str),
    value: f32,
    set: fn(&mut Desktop, f32),
    cx: &mut Context<Desktop>,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .justify_between()
                .text_sm()
                .child(text!(label).with_id((id, 1_usize)))
                .child(
                    div()
                        .text_color(color(TEXT_MUTED))
                        .child(text!(format!("{:.0}%", value * 100.)).with_id((id, 2_usize))),
                ),
        )
        .child(slider(id, name, value, set, cx))
        .into_any_element()
}

/// Microphone and echo checks that need neither a server nor unmuting.
fn audio_check(this: &Desktop, cx: &mut Context<Desktop>) -> Vec<AnyElement> {
    let Some(preview) = this.preview.as_ref() else {
        return Vec::new();
    };
    let (capture, loopback) = (preview.capture, preview.loopback);
    let running = preview.echo_test == EchoTestState::Running;
    let mut rows = vec![
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(icon("mic", 18., color(TEXT_MUTED)))
            .child(
                div().flex_1().min_w_0().text_sm().child(
                    text!(if capture {
                        "Speak to see your level and the voice gate."
                    } else {
                        "Check your microphone without joining a channel."
                    })
                    .with_id("mic-test-intro"),
                ),
            )
            .child(styled_button(
                "mic-test",
                if capture {
                    "Stop test"
                } else {
                    "Test microphone"
                },
                if capture {
                    Tone::Secondary
                } else {
                    Tone::Primary
                },
                move |this, _, cx| {
                    this.set_preview(!capture, false);
                    cx.notify();
                },
                cx,
            ))
            .into_any_element(),
    ];
    if capture {
        rows.push(meter(preview));
        rows.push(toggle(
            "hear-yourself",
            "Hear yourself",
            "Plays back what others would hear, after noise suppression and the voice gate. Use headphones.".into(),
            loopback,
            Rc::new(move |this, _, _| this.set_preview(true, !loopback)),
            cx,
        ));
    }
    let verdict = match &preview.echo_test {
        EchoTestState::Idle => None,
        EchoTestState::Running => Some((
            TEXT_MUTED,
            "Playing a test sound through your speakers for about four seconds…".to_string(),
        )),
        EchoTestState::Failed(message) => Some((WARNING, message.clone())),
        EchoTestState::Done(report) => {
            let leaked = report.transmitted >= 0.05;
            let measured = match (report.echo_db, report.reduction_db) {
                (None, _) => "No echo of the test sound reached Nuschl (headphones, or a system canceller removed it).".to_string(),
                (Some(echo), Some(reduction)) => format!(
                    "The test sound reached the microphone {:.0} dB below its level; processing removed another {reduction:.0} dB.",
                    -echo
                ),
                (Some(echo), None) => format!("The test sound reached the microphone {:.0} dB below its level.", -echo),
            };
            Some(if leaked {
                (
                    WARNING,
                    format!(
                        "{measured} The voice gate opened for {:.0}% of it, so others may hear themselves: use headphones, lower the speaker volume, or set echo cancellation to Always on.",
                        report.transmitted * 100.
                    ),
                )
            } else {
                (
                    SUCCESS,
                    format!(
                        "{measured} Nothing would have been sent, so others will not hear themselves."
                    ),
                )
            })
        }
    };
    rows.push(
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(icon("volume", 18., color(TEXT_MUTED)))
            .child(div().flex_1().min_w_0().text_sm().child(
                text!("Echo test: would others hear themselves?").with_id("echo-test-intro"),
            ))
            .child(styled_button(
                "echo-test",
                if running { "Stop" } else { "Test echo" },
                Tone::Secondary,
                move |this, _, cx| {
                    if running {
                        this.stop_echo_test();
                    } else {
                        this.start_echo_test();
                    }
                    cx.notify();
                },
                cx,
            ))
            .into_any_element(),
    );
    if let Some((tone, text)) = verdict {
        rows.push(
            div()
                .id("echo-test-result")
                .role(Role::Status)
                .text_xs()
                .text_color(color(tone))
                .child(text!(text).with_id("echo-test-text"))
                .into_any_element(),
        );
    }
    rows
}

/// The microphones or speakers to choose from, in a list that scrolls when
/// there are more than four or so.
#[allow(clippy::too_many_arguments)]
fn device_list(
    this: &Desktop,
    id: &'static str,
    label: &'static str,
    devices: &[AudioDevice],
    selected: Option<&str>,
    active: Option<&str>,
    choose: Choose,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let mut rows = vec![(None, "System default".to_string())];
    rows.extend(
        devices
            .iter()
            .map(|device| (Some(device.id.clone()), device.name.clone())),
    );
    let rows = rows.into_iter().enumerate().map(|(index, (device, name))| {
        let chosen = device.as_deref() == selected;
        let live = device.is_some() && device.as_deref() == active;
        let choose = choose.clone();
        let key_choose = choose.clone();
        let value = device.clone();
        let key_value = value.clone();
        let control = div()
            .id((id, index))
            .role(Role::RadioButton)
            .aria_selected(chosen)
            .aria_label(name.clone())
            .tab_index(0)
            .px_2()
            .py(px(6.))
            .rounded(px(6.))
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .text_sm()
            .when(chosen, |row| row.bg(color(SELECTED)))
            .hover(|style| style.bg(color(HOVER)))
            .on_click(cx.listener(move |this, _, _, cx| {
                choose(this, value.clone());
                cx.notify();
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if activation(event) {
                    key_choose(this, key_value.clone());
                    cx.notify();
                    cx.stop_propagation();
                }
            }))
            .child(
                div()
                    .size(px(14.))
                    .rounded_full()
                    .border_2()
                    .border_color(color(if chosen { ACCENT } else { BORDER_STRONG }))
                    .when(chosen, |dot| dot.bg(color(ACCENT))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(text!(name).with_id((id, index + 1000))),
            )
            .when(live, |row| {
                row.child(
                    div()
                        .text_xs()
                        .text_color(color(SUCCESS))
                        .child(text!("in use").with_id((id, index + 2000))),
                )
            });
        crate::layout::focusable((id, index), control)
    });
    div()
        .rounded(px(8.))
        .bg(color(APP))
        .border_1()
        .border_color(color(BORDER))
        .child(
            crate::dialogs::nested_list(this, id, px(148.), div().p_1().children(rows), cx)
                .role(Role::RadioGroup)
                .aria_label(label),
        )
        .into_any_element()
}

pub fn dialog(
    this: &Desktop,
    server: Option<&ServerView>,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let settings = &this.settings;
    let audio = &settings.audio;
    let transmit = audio.transmit;
    let set_transmit = |mode: TransmitMode| -> Action {
        Rc::new(
            move |this: &mut Desktop, _: &mut Window, _: &mut Context<Desktop>| {
                this.update_settings(|settings| settings.audio.transmit = mode)
            },
        )
    };
    let mut voice = vec![managed(
        this,
        "audio.transmit",
        segmented(
            "transmit-mode",
            "Transmission mode",
            vec![
                (
                    "Voice activity",
                    transmit == TransmitMode::VoiceActivity,
                    set_transmit(TransmitMode::VoiceActivity),
                ),
                (
                    "Push to talk",
                    transmit == TransmitMode::PushToTalk,
                    set_transmit(TransmitMode::PushToTalk),
                ),
                (
                    "Continuous",
                    transmit == TransmitMode::Continuous,
                    set_transmit(TransmitMode::Continuous),
                ),
            ],
            cx,
        ),
    )];
    if transmit == TransmitMode::VoiceActivity {
        let slider = labelled_slider(
            "sensitivity",
            ("Sensitivity", "Voice activity sensitivity"),
            audio.vad_sensitivity,
            |this, value| this.update_settings(|settings| settings.audio.vad_sensitivity = value),
            cx,
        );
        voice.push(managed(this, "audio.vad_sensitivity", slider));
    }
    if transmit == TransmitMode::PushToTalk
        && !settings.shortcuts.contains_key(&ShortcutAction::PushToTalk)
    {
        voice.push(
            div()
                .text_xs()
                .text_color(color(TEXT_MUTED))
                .child(
                    text!("Choose a push-to-talk key under Shortcuts below.").with_id("ptt-note"),
                )
                .into_any_element(),
        );
    }
    // The Settings check knows the route even while disconnected or muted.
    let preview = this.preview.as_ref();
    let route_echo = preview
        .and_then(|preview| preview.echo.clone())
        .or_else(|| server.map(|server| server.audio.echo.clone()));
    let upstream_noise = preview
        .filter(|preview| preview.echo.is_some())
        .map(|preview| preview.noise.clone())
        .or_else(|| server.map(|server| server.audio.noise.clone()))
        .flatten();
    let reference = preview
        .filter(|preview| preview.echo.is_some())
        .map(|preview| preview.reference.clone())
        .or_else(|| server.map(|server| server.audio.reference.clone()))
        .flatten();
    let input = preview
        .and_then(|preview| preview.input.clone())
        .or_else(|| server.and_then(|server| server.audio.input.clone()));
    let erle = server
        .and_then(|server| server.audio.erle_db)
        .filter(|erle| *erle >= 1.);
    let echo_status = route_echo.map(|echo| match echo {
        EchoSource::Upstream(name) => format!("Using system echo cancellation: {name}. Nuschl's canceller is off to avoid doubling it."),
        // ERLE is only meaningful once the far end has played something.
        EchoSource::Nuschl => {
            let heard = match &reference {
                Some(output) => format!("It removes everything {output} plays."),
                None => "It removes only Nuschl's own playback: other applications' sound cannot be recorded here.".into(),
            };
            match erle {
                Some(erle) => format!("Nuschl's echo canceller is active · {erle:.0} dB echo reduction. {heard}"),
                None => format!("Nuschl's echo canceller is active. {heard}"),
            }
        }
        EchoSource::None => "Echo cancellation is off for this microphone.".into(),
    });
    let mode = input
        .as_deref()
        .map_or(audio.echo_cancellation, |id| audio.echo_for(id));
    let remembered = input
        .as_ref()
        .is_some_and(|id| audio.device_overrides.contains_key(id));
    let set_echo = |choice: EchoMode| -> Action {
        Rc::new(
            move |this: &mut Desktop, _: &mut Window, _: &mut Context<Desktop>| {
                let input = this
                    .selected_indices()
                    .and_then(|(server, _)| this.client.servers[server].audio.input.clone());
                this.update_settings(|settings| match input {
                    Some(id) if settings.audio.device_overrides.contains_key(&id) => {
                        settings.audio.device_overrides.insert(id, choice);
                    }
                    _ => settings.audio.echo_cancellation = choice,
                })
            },
        )
    };
    let noise = audio.noise_suppression;
    let gain = audio.automatic_gain;
    let processing = vec![
        managed(
            this,
            "audio.noise_suppression",
            toggle(
                "noise-suppression",
                "Noise suppression",
                match &upstream_noise {
                    Some(name) if noise => format!(
                        "Using system noise suppression: {name}. Nuschl's is off to avoid doubling it."
                    ),
                    _ => "RNNoise removes steady background noise.".into(),
                },
                noise,
                Rc::new(move |this, _, _| {
                    this.update_settings(|settings| settings.audio.noise_suppression = !noise)
                }),
                cx,
            ),
        ),
        managed(
            this,
            "audio.automatic_gain",
            toggle(
                "automatic-gain",
                "Automatic gain",
                "Keeps your voice at a steady level, with a limiter.".into(),
                gain,
                Rc::new(move |this, _, _| {
                    this.update_settings(|settings| settings.audio.automatic_gain = !gain)
                }),
                cx,
            ),
        ),
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(text!("Echo cancellation").with_id("echo-heading")),
            )
            .child(managed(
                this,
                if remembered {
                    "audio.device_overrides"
                } else {
                    "audio.echo_cancellation"
                },
                segmented(
                    "echo-mode",
                    "Echo cancellation",
                    vec![
                        (
                            "Automatic",
                            mode == EchoMode::Auto,
                            set_echo(EchoMode::Auto),
                        ),
                        ("Always on", mode == EchoMode::On, set_echo(EchoMode::On)),
                        ("Off", mode == EchoMode::Off, set_echo(EchoMode::Off)),
                    ],
                    cx,
                ),
            ))
            .when_some(echo_status, |column, status| {
                column.child(
                    div()
                        .text_xs()
                        .text_color(color(TEXT_MUTED))
                        .child(text!(status).with_id("echo-status")),
                )
            })
            .children(
                server
                    .map(|server| server.audio.health.issues())
                    .unwrap_or_default()
                    .into_iter()
                    .enumerate()
                    .map(|(index, (label, advice))| {
                        div()
                            .id(("audio-issue", index))
                            .role(Role::Status)
                            .p_3()
                            .rounded(px(8.))
                            .bg(tint(WARNING, 0x1a))
                            .border_1()
                            .border_color(tint(WARNING, 0x55))
                            .flex()
                            .gap_2()
                            .child(icon("alert", 16., color(WARNING)))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(color(WARNING))
                                            .child(text!(label).with_id(("issue-label", index))),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(color(TEXT_MUTED))
                                            .child(text!(advice).with_id(("issue-advice", index))),
                                    ),
                            )
                    }),
            )
            .when_some(input.clone(), |column, id| {
                let detail = if remembered {
                    "This microphone has its own echo setting.".to_string()
                } else {
                    "Remember the echo setting for this microphone only.".to_string()
                };
                column.child(managed(
                    this,
                    "audio.device_overrides",
                    toggle(
                        "echo-override",
                        "Per-microphone setting",
                        detail,
                        remembered,
                        Rc::new(move |this, _, _| {
                            let id = id.clone();
                            this.update_settings(|settings| {
                                if settings.audio.device_overrides.remove(&id).is_none() {
                                    let current = settings.audio.echo_cancellation;
                                    settings.audio.device_overrides.insert(id, current);
                                }
                            })
                        }),
                        cx,
                    ),
                ))
            })
            .into_any_element(),
    ];

    // Device lists come from the Settings check, or from a live connection.
    let listed = preview
        .filter(|preview| !preview.inputs.is_empty() || !preview.outputs.is_empty())
        .map(|preview| {
            (
                preview.inputs.clone(),
                preview.outputs.clone(),
                preview.input.clone(),
                preview.output.clone(),
            )
        })
        .or_else(|| {
            server
                .filter(|server| {
                    !server.audio.inputs.is_empty() || !server.audio.outputs.is_empty()
                })
                .map(|server| {
                    (
                        server.audio.inputs.clone(),
                        server.audio.outputs.clone(),
                        server.audio.input.clone(),
                        server.audio.output.clone(),
                    )
                })
        });
    let devices = match listed {
        Some((inputs, outputs, active_input, active_output)) => vec![
            div()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .child(text!("Microphone").with_id("input-heading"))
                .into_any_element(),
            managed(
                this,
                "audio.input_device",
                device_list(
                    this,
                    "input-devices",
                    "Microphone",
                    &inputs,
                    audio.input_device.as_deref(),
                    active_input.as_deref(),
                    Rc::new(|this, device| {
                        this.update_settings(|settings| settings.audio.input_device = device)
                    }),
                    cx,
                ),
            ),
            div()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .child(text!("Speakers or headphones").with_id("output-heading"))
                .into_any_element(),
            managed(
                this,
                "audio.output_device",
                device_list(
                    this,
                    "output-devices",
                    "Speakers or headphones",
                    &outputs,
                    audio.output_device.as_deref(),
                    active_output.as_deref(),
                    Rc::new(|this, device| {
                        this.update_settings(|settings| settings.audio.output_device = device)
                    }),
                    cx,
                ),
            ),
        ],
        None => {
            // The worker's detail (each sound system's error) stays in the
            // tooltip; the line itself says what it means.
            let status = preview.and_then(|preview| preview.status.clone());
            let line = match &status {
                Some(status) if status.starts_with("Audio reconnecting") => {
                    "No sound system is reachable (PipeWire, PulseAudio or a sound card). Nuschl keeps trying.".to_string()
                }
                Some(status) => status.clone(),
                None => "Looking for audio devices…".into(),
            };
            vec![
                div()
                    .id("devices-unavailable-row")
                    .text_sm()
                    .text_color(color(TEXT_MUTED))
                    .when_some(status, |row, status| {
                        row.tooltip(move |_, cx| cx.new(|_| Tooltip(status.clone().into())).into())
                    })
                    .child(text!(line).with_id("devices-unavailable"))
                    .into_any_element(),
            ]
        }
    };

    let autostart = this.autostart.unwrap_or(false);
    let enter_sends = settings.enter_sends;
    let general = vec![
        toggle(
            "autostart",
            "Start when you log in",
            "Opens minimised. Servers set to connect automatically join muted and deafened.".into(),
            autostart,
            Rc::new(move |this, _, _| this.set_autostart(!autostart)),
            cx,
        ),
        managed(
            this,
            "enter_sends",
            toggle(
                "enter-sends",
                "Enter sends messages",
                if enter_sends {
                    "Shift+Enter starts a new line.".into()
                } else {
                    "Enter starts a new line; Ctrl/Cmd+Enter sends.".into()
                },
                enter_sends,
                Rc::new(move |this, _, _| {
                    this.update_settings(|settings| settings.enter_sends = !enter_sends)
                }),
                cx,
            ),
        ),
    ];
    let mut general = general;
    let server_icons = settings.server_icons;
    general.push(managed(
        this,
        "server_icons",
        toggle(
            "server-icons",
            "Server icons",
            "Shows each server's website icon, fetched over HTTPS from its address.".into(),
            server_icons,
            Rc::new(move |this, _, _| {
                this.update_settings(|settings| settings.server_icons = !server_icons);
                this.refresh_icons();
            }),
            cx,
        ),
    ));
    let notifications = settings.notifications;
    general.push(managed(this, "notifications", toggle(
        "notifications",
        "Desktop notifications",
        if cfg!(target_os = "linux") {
            "Direct messages, your voice channel and mentions of your name, while Nuschl is not in front."
        } else {
            "Not available on this system yet."
        }
        .into(),
        notifications,
        Rc::new(move |this, _, _| {
            this.update_settings(|settings| settings.notifications = !notifications)
        }),
        cx,
    )));
    let sound_effects = settings.sound_effects;
    general.push(managed(this, "sound_effects", toggle(
        "sound-effects",
        "Sound effects",
        "People joining and leaving your channel, mute and deafen, push-to-talk, messages and the connection.".into(),
        sound_effects,
        Rc::new(move |this, _, _| {
            this.update_settings(|settings| settings.sound_effects = !sound_effects)
        }),
        cx,
    )));
    if sound_effects {
        let slider = labelled_slider(
            "sound-volume",
            ("Volume", "Sound effect volume"),
            settings.sound_volume,
            |this, value| this.update_settings(|settings| settings.sound_volume = value),
            cx,
        );
        general.push(managed(this, "sound_volume", slider));
    }
    let send_typing = settings.send_typing;
    let mut privacy = vec![managed(
        this,
        "send_typing",
        toggle(
            "send-typing",
            "Show when you are typing",
            "On Nuschl servers, people who can read the conversation see that you are typing a \
             message. Stock Mumble servers have no typing indicators."
                .into(),
            send_typing,
            Rc::new(move |this, _, _| {
                this.update_settings(|settings| settings.send_typing = !send_typing);
                this.client.composing(false);
            }),
            cx,
        ),
    )];
    let relay_only = settings.relay_only;
    privacy.push(managed(
        this,
        "relay_only",
        toggle(
            "relay-only",
            "Relay-only connections",
            "Use configured servers and relays without sharing your IP address with other users. \
             Skips direct connections and address discovery. Enabling stops active direct-capable file \
             transfers; offer or download them again to use the relay. Legacy Mumble servers cannot \
             enforce address privacy."
                .into(),
            relay_only,
            Rc::new(move |this, _, _| {
                this.update_settings(|settings| settings.relay_only = !relay_only);
            }),
            cx,
        ),
    ));
    let friends = settings.friend_notifications;
    general.push(managed(
        this,
        "friend_notifications",
        toggle(
            "friend-notifications",
            "Notify when friends connect",
            if cfg!(target_os = "linux") {
                "When someone you added as a friend comes online while Nuschl is not in front."
            } else {
                "Not available on this system yet."
            }
            .into(),
            friends,
            Rc::new(move |this, _, _| {
                this.update_settings(|settings| settings.friend_notifications = !friends)
            }),
            cx,
        ),
    ));
    if crate::update::supported() {
        use crate::update::UpdateState;
        let check = settings.check_updates;
        general.push(managed(
            this,
            "check_updates",
            toggle(
                "check-updates",
                "Check for updates",
                "Looks for signed releases on GitHub every few hours.".into(),
                check,
                Rc::new(move |this, _, _| {
                    this.update_settings(|settings| settings.check_updates = !check)
                }),
                cx,
            ),
        ));
        let status = match &this.update {
            UpdateState::Idle => format!("Nuschl {}", env!("CARGO_PKG_VERSION")),
            UpdateState::Checking => "Checking…".into(),
            UpdateState::UpToDate => {
                format!("Nuschl {} is up to date.", env!("CARGO_PKG_VERSION"))
            }
            UpdateState::Available(update) => format!("Nuschl {} is available.", update.version),
            UpdateState::Installing { .. } => "Installing the update…".into(),
            UpdateState::Installed => "Update installed; restart to use it.".into(),
            UpdateState::Failed(error) => format!("Update failed: {error}"),
        };
        general.push(
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .text_color(color(TEXT_MUTED))
                        .child(text!(status).with_id("update-status")),
                )
                .child(styled_button(
                    "check-update-now",
                    "Check now",
                    Tone::Secondary,
                    |this, _, cx| {
                        this.check_update_now();
                        cx.notify();
                    },
                    cx,
                ))
                .into_any_element(),
        );
    }
    let force_tcp = settings.force_tcp;
    let reconnect = settings.auto_reconnect;
    let network = vec![
        managed(this, "force_tcp", toggle("force-tcp", "Force TCP voice", "Send voice through the encrypted control connection instead of UDP. Applies on reconnect.".into(), force_tcp,
            Rc::new(move |this, _, _| this.update_settings(|settings| settings.force_tcp = !force_tcp)), cx)),
        managed(this, "auto_reconnect", toggle("auto-reconnect", "Reconnect automatically", "Retry with backoff after an unexpected disconnect.".into(), reconnect,
            Rc::new(move |this, _, _| this.update_settings(|settings| settings.auto_reconnect = !reconnect)), cx)),
    ];

    let identity = this.profile.identity().ok();
    let identity_section = vec![
        div()
            .flex()
            .items_center()
            .gap_3()
            .child(icon("certificate", 20., color(TEXT_MUTED)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(text!("Client certificate").with_id("identity-heading")))
                    .child(div().text_xs().text_color(color(TEXT_MUTED)).truncate().child(text!(identity.map(|identity| format!("SHA-1 {}", identity.mumble_hash())).unwrap_or_else(|| "Not created yet".into())).with_id("identity-hash"))),
            )
            .child(styled_button("import-certificate", "Import…", Tone::Secondary, |this, window, cx| this.import_certificate(window, cx), cx))
            .child(styled_button("export-certificate", "Export…", Tone::Secondary, |this, window, cx| {
                this.open_prompt(
                    usize::MAX,
                    crate::menus::PromptAction::ExportCertificate,
                    "Protect the exported certificate",
                    "Choose a password for the .p12 file. Anyone with the file and password can log in as you.",
                    String::new(),
                    window,
                    cx,
                );
            }, cx))
            .into_any_element(),
        div()
            .text_xs()
            .text_color(color(TEXT_FAINT))
            .child(text!(this.identity_status.clone().unwrap_or_else(|| "Servers recognise registered users by this certificate. Mumble's Certificate Wizard exports compatible .p12 files.".into())).with_id("identity-status"))
            .into_any_element(),
    ];
    let identity_section = identity_section
        .into_iter()
        .chain(crate::identity::settings_rows(this, cx))
        .collect();
    let content = div()
        .flex()
        .flex_col()
        .child(
            div()
                .p_6()
                .pb_4()
                .flex()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .text_xl()
                        .font_weight(FontWeight::BOLD)
                        .child(text!("Settings").with_id("settings-heading")),
                )
                .child(styled_button(
                    "close-settings",
                    "Done",
                    Tone::Primary,
                    |this, _, cx| {
                        this.close_settings();
                        cx.notify();
                    },
                    cx,
                )),
        )
        .child(
            div()
                .px_6()
                .pb_6()
                .flex()
                .flex_col()
                .gap_6()
                .child(section("MICROPHONE TEST", audio_check(this, cx)))
                .child(section("TRANSMISSION", voice))
                .child(section("SHORTCUTS", shortcut_rows(this, cx)))
                .child(section("PROCESSING", processing))
                .child(section("DEVICES", devices))
                .child(section("NETWORK", network))
                .child(section("APPEARANCE", crate::appearance::settings(this, cx)))
                .child(section("GENERAL", general))
                .child(section("PRIVACY", privacy))
                .child(section("IDENTITY", identity_section)),
        )
        .into_any_element();
    modal(
        this,
        "settings-dialog",
        "Settings",
        560.,
        content,
        cx,
        |this| this.close_settings(),
    )
}

// Shortcuts: one row per action with its key, Record and Clear.

fn shortcut_description(action: ShortcutAction) -> &'static str {
    match action {
        ShortcutAction::PushToTalk => "Hold to transmit.",
        ShortcutAction::ToggleMute => "Same as the microphone button.",
        ShortcutAction::ToggleDeafen => "Same as the headphones button.",
        ShortcutAction::PushToMute => "Hold to silence yourself while transmitting.",
        ShortcutAction::Find => "Jump to any channel, person or conversation. Only in Nuschl.",
        ShortcutAction::Search => "Search the open conversation's messages. Only in Nuschl.",
        ShortcutAction::SearchAll => "Search messages on every server. Only in Nuschl.",
    }
}

fn shortcut_rows(this: &Desktop, cx: &mut Context<Desktop>) -> Vec<AnyElement> {
    use crate::shortcuts::{applies, display, label};
    let shortcuts = &this.shortcuts;
    let transmit = this.settings.audio.transmit;
    let mut rows: Vec<AnyElement> = ShortcutAction::ALL
        .into_iter()
        .enumerate()
        .map(|(index, action)| {
            let name = label(action);
            let binding = crate::shortcuts::binding(&this.settings, action);
            let recording = shortcuts.recording == Some(action);
            let system = binding
                .filter(|_| action.system_wide())
                .and(shortcuts.system.as_ref());
            let detail = if recording {
                "Press the key, with any modifiers. Esc cancels.".to_string()
            } else if binding.is_some() && !applies(action, transmit) {
                match action {
                    ShortcutAction::PushToTalk => "Only in push-to-talk mode.",
                    _ => "Only with voice activity or continuous transmission.",
                }
                .into()
            } else if let Some(system) = system {
                match system.get(&action) {
                    Some(trigger) => format!("System-wide: {trigger}"),
                    None => "Only while Nuschl is focused: the desktop did not grant it.".into(),
                }
            } else {
                shortcut_description(action).into()
            };
            let key = if recording {
                "…".to_string()
            } else {
                binding.map_or_else(|| "Not set".into(), display)
            };
            let row = div()
                .id(("shortcut", index))
                .role(Role::Group)
                .aria_label(format!("{name} shortcut"))
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .child(text!(name).with_id(("shortcut-name", index))),
                        )
                        .child(
                            div()
                                .id(("shortcut-detail", index))
                                .when(recording, |line| line.role(Role::Status))
                                .text_xs()
                                .text_color(color(if recording { ACCENT } else { TEXT_MUTED }))
                                .child(text!(detail).with_id(("shortcut-detail-text", index))),
                        ),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .min_w(px(64.))
                        .px_2()
                        .py(px(3.))
                        .rounded(px(6.))
                        .bg(color(APP))
                        .border_1()
                        .border_color(color(if recording { ACCENT } else { BORDER_STRONG }))
                        .flex()
                        .justify_center()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(color(if binding.is_some() { TEXT } else { TEXT_FAINT }))
                        .child(text!(key).with_id(("shortcut-key", index))),
                )
                .child(named_button(
                    ("record-shortcut", index),
                    if recording { "Cancel" } else { "Record" },
                    if recording {
                        format!("Cancel recording {name} shortcut")
                    } else {
                        format!("Record {name} shortcut")
                    },
                    Tone::Secondary,
                    move |this, _, cx| {
                        this.shortcuts.recording = (!recording).then_some(action);
                        cx.notify();
                    },
                    cx,
                ))
                .child(
                    div()
                        .w(px(64.))
                        .flex_shrink_0()
                        .when(binding.is_some(), |slot| {
                            slot.child(named_button(
                                ("clear-shortcut", index),
                                "Clear",
                                format!("Clear {name} shortcut"),
                                Tone::Ghost,
                                move |this, _, cx| {
                                    this.update_settings(|settings| {
                                        // Voice actions have no default; in-app ones
                                        // would fall back to theirs without the marker.
                                        if action.system_wide() {
                                            settings.shortcuts.remove(&action);
                                        } else {
                                            settings
                                                .shortcuts
                                                .insert(action, crate::shortcuts::UNSET.into());
                                        }
                                    });
                                    cx.notify();
                                },
                                cx,
                            ))
                        }),
                )
                .into_any_element();
            managed(this, &format!("shortcuts.{}", action.id()), row)
        })
        .collect();
    if let Some((status, detail)) = shortcuts.status.clone() {
        rows.push(
            div()
                .id("shortcut-status")
                .role(Role::Status)
                .text_xs()
                .text_color(color(TEXT_FAINT))
                .tooltip(move |_, cx| cx.new(|_| Tooltip(detail.clone().into())).into())
                .child(text!(status).with_id("shortcut-status-text"))
                .into_any_element(),
        );
    }
    rows
}
