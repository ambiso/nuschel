// SPDX-License-Identifier: EUPL-1.2
//! Voice and audio settings. Changes apply immediately and are saved locally.
use crate::{
    assets::icon,
    dialogs::{Tone, Tooltip, modal, styled_button},
    preview::{EchoTestState, PreviewView},
    state::{Desktop, ServerView},
    theme::*,
};
use gpui_kit::{prelude::*, *};
use nuschel_core::audio::{AudioDevice, EchoMode, EchoSource, TransmitMode};
use std::rc::Rc;

type Action = Rc<dyn Fn(&mut Desktop, &mut Window, &mut Context<Desktop>)>;
type Choose = Rc<dyn Fn(&mut Desktop, Option<String>)>;

fn activation(event: &KeyDownEvent) -> bool {
    event.keystroke.key == "enter" || event.keystroke.key == "space"
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

fn segmented(
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
                .child(div().size(px(16.)).rounded_full().bg(color(0xffffff))),
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
                (None, _) => "No echo of the test sound reached Nuschel (headphones, or a system canceller removed it).".to_string(),
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
                if running { "Testing…" } else { "Test echo" },
                Tone::Secondary,
                move |this, _, cx| {
                    if !running {
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

fn device_list(
    id: &'static str,
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
    div()
        .id(id)
        .role(Role::RadioGroup)
        .max_h(px(150.))
        .overflow_y_scroll()
        .rounded(px(8.))
        .bg(color(APP))
        .border_1()
        .border_color(color(BORDER))
        .p_1()
        .children(rows.into_iter().enumerate().map(|(index, (device, name))| {
            let chosen = device.as_deref() == selected;
            let live = device.is_some() && device.as_deref() == active;
            let choose = choose.clone();
            let value = device.clone();
            div()
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
                })
        }))
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
    let mut voice = vec![segmented(
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
    )];
    if transmit == TransmitMode::VoiceActivity {
        let sensitivity = audio.vad_sensitivity;
        voice.push(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .text_sm()
                        .child(text!("Sensitivity").with_id("sensitivity-label"))
                        .child(
                            div().text_color(color(TEXT_MUTED)).child(
                                text!(format!("{:.0}%", sensitivity * 100.))
                                    .with_id("sensitivity-value"),
                            ),
                        ),
                )
                .child(
                    div()
                        .id("sensitivity")
                        .role(Role::Slider)
                        .aria_label("Voice activity sensitivity")
                        .tab_index(0)
                        .flex()
                        .gap(px(3.))
                        .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                            let step = match event.keystroke.key.as_str() {
                                "left" | "down" => -0.1,
                                "right" | "up" => 0.1,
                                _ => return,
                            };
                            this.update_settings(|settings| settings.audio.vad_sensitivity += step);
                            cx.notify();
                        }))
                        .children((0..10_usize).map(|step| {
                            let value = step as f32 / 9.;
                            div()
                                .id(("sensitivity-step", step))
                                .flex_1()
                                .h(px(10.))
                                .rounded(px(3.))
                                .cursor_pointer()
                                .bg(color(if value <= sensitivity + 0.001 {
                                    ACCENT
                                } else {
                                    BORDER_STRONG
                                }))
                                .hover(|style| style.opacity(0.85))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.update_settings(|settings| {
                                        settings.audio.vad_sensitivity = value
                                    });
                                    cx.notify();
                                }))
                        })),
                )
                .into_any_element(),
        );
    }
    if transmit == TransmitMode::PushToTalk || settings.push_to_talk_key.is_some() {
        let label = if this.recording_ptt {
            "Press a key… (Esc cancels)".to_string()
        } else {
            settings
                .push_to_talk_key
                .clone()
                .map(|key| format!("Push-to-talk key: {key}"))
                .unwrap_or_else(|| "No push-to-talk key".into())
        };
        voice.push(
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(icon("keyboard", 18., color(TEXT_MUTED)))
                .child(
                    div()
                        .flex_1()
                        .text_sm()
                        .child(text!(label).with_id("ptt-key")),
                )
                .child(styled_button(
                    "record-ptt",
                    if this.recording_ptt {
                        "Cancel"
                    } else {
                        "Set key"
                    },
                    Tone::Secondary,
                    |this, _, cx| {
                        this.recording_ptt = !this.recording_ptt;
                        cx.notify();
                    },
                    cx,
                ))
                .when(settings.push_to_talk_key.is_some(), |row| {
                    row.child(styled_button(
                        "clear-ptt",
                        "Clear",
                        Tone::Ghost,
                        |this, _, cx| {
                            this.update_settings(|settings| settings.push_to_talk_key = None);
                            cx.notify();
                        },
                        cx,
                    ))
                })
                .into_any_element(),
        );
        voice.push(
            div()
                .text_xs()
                .text_color(color(TEXT_FAINT))
                .child(text!(this.global_ptt_status.clone().unwrap_or_else(|| "The key works while Nuschel is focused, and system-wide where the desktop grants a global shortcut.".into())).with_id("ptt-note"))
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
        EchoSource::Upstream(name) => format!("Using system echo cancellation: {name}. Nuschel's canceller is off to avoid doubling it."),
        // ERLE is only meaningful once the far end has played something.
        EchoSource::Nuschel => {
            let heard = match &reference {
                Some(output) => format!("It removes everything {output} plays."),
                None => "It removes only Nuschel's own playback: other applications' sound cannot be recorded here.".into(),
            };
            match erle {
                Some(erle) => format!("Nuschel's echo canceller is active · {erle:.0} dB echo reduction. {heard}"),
                None => format!("Nuschel's echo canceller is active. {heard}"),
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
                    .and_then(|(server, _)| this.servers[server].audio.input.clone());
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
        toggle(
            "noise-suppression",
            "Noise suppression",
            match &upstream_noise {
                Some(name) if noise => format!(
                    "Using system noise suppression: {name}. Nuschel's is off to avoid doubling it."
                ),
                _ => "RNNoise removes steady background noise.".into(),
            },
            noise,
            Rc::new(move |this, _, _| {
                this.update_settings(|settings| settings.audio.noise_suppression = !noise)
            }),
            cx,
        ),
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
            .child(segmented(
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
                column.child(toggle(
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
            device_list(
                "input-devices",
                &inputs,
                audio.input_device.as_deref(),
                active_input.as_deref(),
                Rc::new(|this, device| {
                    this.update_settings(|settings| settings.audio.input_device = device)
                }),
                cx,
            ),
            div()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .child(text!("Speakers or headphones").with_id("output-heading"))
                .into_any_element(),
            device_list(
                "output-devices",
                &outputs,
                audio.output_device.as_deref(),
                active_output.as_deref(),
                Rc::new(|this, device| {
                    this.update_settings(|settings| settings.audio.output_device = device)
                }),
                cx,
            ),
        ],
        None => {
            // The worker's detail (each sound system's error) stays in the
            // tooltip; the line itself says what it means.
            let status = preview.and_then(|preview| preview.status.clone());
            let line = match &status {
                Some(status) if status.starts_with("Audio reconnecting") => {
                    "No sound system is reachable (PipeWire, PulseAudio or a sound card). Nuschel keeps trying.".to_string()
                }
                Some(status) => status.clone(),
                None => "Looking for audio devices…".into(),
            };
            vec![div()
                .id("devices-unavailable-row")
                .text_sm()
                .text_color(color(TEXT_MUTED))
                .when_some(status, |row, status| {
                    row.tooltip(move |_, cx| cx.new(|_| Tooltip(status.clone().into())).into())
                })
                .child(text!(line).with_id("devices-unavailable"))
                .into_any_element()]
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
    ];
    let mut general = general;
    let notifications = settings.notifications;
    general.push(toggle(
        "notifications",
        "Desktop notifications",
        if cfg!(target_os = "linux") {
            "Direct messages, your voice channel and mentions of your name, while Nuschel is not in front."
        } else {
            "Not available on this system yet."
        }
        .into(),
        notifications,
        Rc::new(move |this, _, _| {
            this.update_settings(|settings| settings.notifications = !notifications)
        }),
        cx,
    ));
    if crate::update::supported() {
        use crate::update::UpdateState;
        let check = settings.check_updates;
        general.push(toggle(
            "check-updates",
            "Check for updates",
            "Looks for signed releases on GitHub every few hours.".into(),
            check,
            Rc::new(move |this, _, _| {
                this.update_settings(|settings| settings.check_updates = !check)
            }),
            cx,
        ));
        let status = match &this.update {
            UpdateState::Idle => format!("Nuschel {}", env!("CARGO_PKG_VERSION")),
            UpdateState::Checking => "Checking…".into(),
            UpdateState::UpToDate => {
                format!("Nuschel {} is up to date.", env!("CARGO_PKG_VERSION"))
            }
            UpdateState::Available(update) => format!("Nuschel {} is available.", update.version),
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
        toggle("force-tcp", "Force TCP voice", "Send voice through the encrypted control connection instead of UDP. Applies on reconnect.".into(), force_tcp,
            Rc::new(move |this, _, _| this.update_settings(|settings| settings.force_tcp = !force_tcp)), cx),
        toggle("auto-reconnect", "Reconnect automatically", "Retry with backoff after an unexpected disconnect.".into(), reconnect,
            Rc::new(move |this, _, _| this.update_settings(|settings| settings.auto_reconnect = !reconnect)), cx),
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
                .child(section("PROCESSING", processing))
                .child(section("DEVICES", devices))
                .child(section("NETWORK", network))
                .child(section("GENERAL", general))
                .child(section("IDENTITY", identity_section)),
        )
        .into_any_element();
    modal("settings-dialog", "Settings", 560., content, cx, |this| {
        this.close_settings()
    })
}
