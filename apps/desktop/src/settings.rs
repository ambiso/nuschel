// SPDX-License-Identifier: EUPL-1.2
//! Voice and audio settings. Changes apply immediately and are saved locally.
use crate::{
    assets::icon,
    dialogs::{Tone, modal, styled_button},
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

fn toggle(
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

fn meter(server: &ServerView) -> AnyElement {
    let fraction = ((server.audio.level_db + 70.) / 70.).clamp(0., 1.);
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .id("input-meter")
                .role(Role::ProgressIndicator)
                .aria_label(format!("Microphone level {:.0} dB", server.audio.level_db))
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
                        .bg(color(if server.audio.transmitting {
                            SUCCESS
                        } else {
                            TEXT_FAINT
                        })),
                ),
        )
        .child(
            div().text_xs().text_color(color(TEXT_FAINT)).child(
                text!(if server.muted {
                    "Unmute to test your microphone.".to_string()
                } else if server.audio.transmitting {
                    format!("Transmitting · speech {:.0}%", server.audio.voice * 100.)
                } else {
                    format!(
                        "Not transmitting · speech {:.0}%",
                        server.audio.voice * 100.
                    )
                })
                .with_id("meter-label"),
            ),
        )
        .into_any_element()
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
    if let Some(server) = server.filter(|server| server.status == crate::state::Status::Connected) {
        voice.push(meter(server));
    }

    let input = server.and_then(|server| server.audio.input.clone());
    let echo_status = server.map(|server| match &server.audio.echo {
        EchoSource::Upstream(name) => format!("Using system echo cancellation: {name}. Nuschel's canceller is off to avoid doubling it."),
        EchoSource::Nuschel => match server.audio.erle_db {
            Some(erle) => format!("Nuschel's echo canceller is active · {erle:.0} dB echo reduction"),
            None => "Nuschel's echo canceller is active.".into(),
        },
        EchoSource::None if server.muted => "Unmute to see how echo is handled on this microphone.".into(),
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
            "RNNoise removes steady background noise.".into(),
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

    let devices = match server
        .filter(|server| !server.audio.inputs.is_empty() || !server.audio.outputs.is_empty())
    {
        Some(server) => vec![
            div()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .child(text!("Microphone").with_id("input-heading"))
                .into_any_element(),
            device_list(
                "input-devices",
                &server.audio.inputs,
                audio.input_device.as_deref(),
                server.audio.input.as_deref(),
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
                &server.audio.outputs,
                audio.output_device.as_deref(),
                server.audio.output.as_deref(),
                Rc::new(|this, device| {
                    this.update_settings(|settings| settings.audio.output_device = device)
                }),
                cx,
            ),
        ],
        None => vec![
            div()
                .text_sm()
                .text_color(color(TEXT_MUTED))
                .child(
                    text!("Connect to a server to list audio devices.")
                        .with_id("devices-unavailable"),
                )
                .into_any_element(),
        ],
    };

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
                        .child(text!("Voice & audio").with_id("settings-heading")),
                )
                .child(styled_button(
                    "close-settings",
                    "Done",
                    Tone::Primary,
                    |this, _, cx| {
                        this.settings_open = false;
                        this.recording_ptt = false;
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
                .child(section("TRANSMISSION", voice))
                .child(section("PROCESSING", processing))
                .child(section("DEVICES", devices))
                .child(section("NETWORK", network))
                .child(section("IDENTITY", identity_section)),
        )
        .into_any_element();
    modal(
        "settings-dialog",
        "Voice and audio settings",
        560.,
        content,
        cx,
        |this| {
            this.settings_open = false;
            this.recording_ptt = false;
        },
    )
}
