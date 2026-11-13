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
                    let control = div()
                        .id((id, index))
                        .role(Role::RadioButton)
                        .aria_label(text)
                        .aria_selected(selected)
                        .tab_index(0)
                        .flex_1()
                        .min_w_0()
                        .whitespace_normal()
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
                                cx.stop_propagation();
                            }
                        }))
                        .child(text!(text).with_id((id, index + 100)));
                    crate::layout::focusable((id, index), control)
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
    let control = div()
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
                cx.stop_propagation();
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
        );
    crate::layout::focusable(id, control)
}

/// The level bar's range, dBFS.
const METER_MIN_DB: f32 = -70.;

/// A bar from 0 to 1 with `fill` (none: no reading) and thin `marks`, each
/// strong or faint, standing out above and below it.
fn meter_bar(fill: Option<(f32, Token)>, marks: &[(f32, Token)]) -> Div {
    div()
        .relative()
        .h(px(8.))
        .my(px(3.))
        .w_full()
        .child(
            div()
                .absolute()
                .inset_0()
                .rounded_full()
                .bg(color(APP))
                .border_1()
                .border_color(color(BORDER)),
        )
        .when_some(fill, |bar, (fraction, tone)| {
            bar.child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .w(relative(fraction.clamp(0., 1.)))
                    .rounded_full()
                    .bg(color(tone)),
            )
        })
        .children(marks.iter().map(|&(at, tone)| {
            div()
                .absolute()
                .top(px(-3.))
                .h(px(14.))
                .left(relative(at.clamp(0., 1.)))
                .ml(px(-1.))
                .w(px(2.))
                .rounded(px(1.))
                .bg(color(tone))
        }))
}

/// One meter: its name and reading above the bar, what the marks mean below.
fn meter_row(
    id: &'static str,
    name: &'static str,
    reading: String,
    bar: Div,
    legend: String,
    numeric: Option<(f64, f64, f64)>,
) -> AnyElement {
    let description = format!("{reading}. {legend}");
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .justify_between()
                .gap_3()
                .text_sm()
                .child(text!(name).with_id((id, 1_usize)))
                .child(
                    div()
                        .text_color(color(TEXT_MUTED))
                        .child(text!(reading).with_id((id, 2_usize))),
                ),
        )
        .child(
            bar.id(id)
                .role(Role::ProgressIndicator)
                .aria_label(name)
                .aria_description(description)
                .when_some(numeric, |bar, (value, min, max)| {
                    bar.aria_numeric_value(value)
                        .aria_min_numeric_value(min)
                        .aria_max_numeric_value(max)
                }),
        )
        .child(
            div()
                .text_xs()
                .text_color(color(TEXT_FAINT))
                .child(text!(legend).with_id((id, 3_usize))),
        )
        .into_any_element()
}

/// Whether you are in a call (connected with voice) but muted or deafened in
/// every one: the microphone test records all the same, which the meter says.
fn muted_in_calls(this: &Desktop) -> bool {
    use nuschl_client::voice::Role;
    let roles: Vec<Role> = (0..this.client.servers.len())
        .map(|index| this.client.voice_role(index))
        .collect();
    roles.iter().any(|role| *role != Role::Offline) && !roles.contains(&Role::Talking)
}

/// What the local microphone test measures, above Sensitivity: the level of
/// the processed input (the RMS, in dBFS, of what echo cancellation, noise
/// suppression and gain leave, as the worker's `level_db`) against the gate's
/// floor, the speech probability against the thresholds `sensitivity` gives,
/// and whether the voice gate is open. Nothing is sent: these are not a
/// call's transmissions. "—" when there is no current reading.
fn meter(
    preview: &PreviewView,
    sensitivity: f32,
    transmit: TransmitMode,
    muted_in_calls: bool,
) -> AnyElement {
    let reading = preview.reading;
    let voice_activity = transmit == TransmitMode::VoiceActivity;
    let thresholds = nuschl_core::audio::gate_thresholds(sensitivity);
    let floor = nuschl_core::audio::GATE_FLOOR_DB;
    let level_at = |db: f32| (db - METER_MIN_DB) / -METER_MIN_DB;
    let level = meter_row(
        "input-level",
        "Processed input level",
        reading.map_or_else(
            || "—".into(),
            |reading| {
                if reading.level_db < METER_MIN_DB {
                    format!("below {METER_MIN_DB:.0} dBFS")
                } else {
                    format!("{:.0} dBFS", reading.level_db)
                }
            },
        ),
        meter_bar(
            reading.map(|reading| {
                let tone = if reading.level_db > floor {
                    ACCENT
                } else {
                    TEXT_FAINT
                };
                (level_at(reading.level_db), tone)
            }),
            &[(level_at(floor), TEXT)],
        ),
        format!(
            "After echo cancellation, noise suppression and gain. Marked: {floor:.0} dBFS, below which the voice gate stays closed."
        ),
        reading.map(|reading| {
            (
                f64::from(reading.level_db.clamp(METER_MIN_DB, 0.)),
                f64::from(METER_MIN_DB),
                0.,
            )
        }),
    );
    let probability = meter_row(
        "speech-probability",
        "Speech probability",
        reading.map_or_else(
            || "—".into(),
            |reading| format!("{:.0} %", reading.voice * 100.),
        ),
        meter_bar(
            reading.map(|reading| {
                let tone = if reading.voice >= thresholds.open {
                    SUCCESS
                } else {
                    TEXT_FAINT
                };
                (reading.voice, tone)
            }),
            &if voice_activity {
                vec![(thresholds.open, TEXT), (thresholds.close, TEXT_MUTED)]
            } else {
                Vec::new()
            },
        ),
        if voice_activity {
            format!(
                "Marked from your sensitivity: the gate opens above {:.0} % and, once open, stays open down to {:.0} %. In calls, echo protection can raise these. Not a loudness.",
                thresholds.open * 100.,
                thresholds.close * 100.
            )
        } else {
            "How confident speech detection is that this is speech. Not a loudness.".into()
        },
        reading.map(|reading| (f64::from(reading.voice) * 100., 0., 100.)),
    );
    let (state, tone) = match (transmit, reading) {
        (TransmitMode::PushToTalk, _) => (
            "Push to talk: you transmit while you hold its key".to_string(),
            TEXT_FAINT,
        ),
        (TransmitMode::Continuous, _) => (
            "Continuous: no voice gate, everything you say is sent in calls".to_string(),
            TEXT_FAINT,
        ),
        (_, None) => ("—".to_string(), TEXT_FAINT),
        (_, Some(reading)) if reading.transmitting => ("Voice gate open".to_string(), SUCCESS),
        (_, Some(_)) => ("Voice gate closed".to_string(), TEXT_FAINT),
    };
    div()
        .id("microphone-activity")
        .role(Role::Group)
        .aria_label("Microphone test")
        .flex()
        .flex_col()
        .gap_3()
        .p_3()
        .rounded(px(8.))
        .border_1()
        .border_color(color(BORDER))
        // An explicitly started test is capture of its own: muting a call
        // does not stop it, and the meter says so rather than imply it.
        .when(muted_in_calls, |meter| {
            meter.child(
                div()
                    .id("test-while-muted")
                    .role(Role::Status)
                    .aria_label("Muted in calls · this test still uses your microphone")
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_xs()
                    .text_color(color(WARNING))
                    .child(icon("mic-off", 14., color(WARNING)))
                    .child(
                        text!("Muted in calls · this test still uses your microphone")
                            .with_id("test-while-muted-text"),
                    ),
            )
        })
        .child(level)
        .child(probability)
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_sm()
                .child(div().size(px(8.)).rounded_full().bg(color(tone)))
                .child(
                    div()
                        .id("gate-state")
                        .role(Role::Status)
                        .aria_label(state.clone())
                        .child(text!(state).with_id("gate-state-text")),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(color(TEXT_FAINT))
                        .child(text!("· a local test, nothing is sent").with_id("gate-state-note")),
                ),
        )
        .into_any_element()
}

/// A slider in Settings (see crate::slider): heard while it moves, saved
/// when it is let go or half a second after the last key.
fn slider(
    this: &Desktop,
    id: &'static str,
    name: &'static str,
    setting: crate::slider::Setting,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let read_only = this.managed.contains(&setting.managed_key());
    crate::slider::slider(
        crate::slider::Spec {
            id: id.into(),
            name: name.into(),
            value: setting.get(&this.settings),
            target: crate::slider::Target {
                owner: crate::slider::Owner::Settings(this.settings_invocation),
                setting,
            },
            read_only,
        },
        cx,
    )
}

/// A slider under a label that shows its value; `name` is for screen readers.
fn labelled_slider(
    this: &Desktop,
    id: &'static str,
    (label, name): (&'static str, &'static str),
    setting: crate::slider::Setting,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let readout = setting.readout(setting.get(&this.settings));
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .justify_between()
                .gap_3()
                .text_sm()
                .child(text!(label).with_id((id, 1_usize)))
                .child(
                    div()
                        .text_color(color(TEXT_MUTED))
                        .child(text!(readout).with_id((id, 2_usize))),
                ),
        )
        .child(slider(this, id, name, setting, cx))
        .into_any_element()
}

/// What a finished echo test found. The gate's share only means echo would
/// be sent when echo was detected: without it, the gate opened on other
/// sound at the microphone (speech, noise, a test tone), which is reported
/// as such and says nothing about echo.
fn echo_verdict(report: &crate::preview::EchoReport) -> (Token, String) {
    let gate_opened = report.transmitted >= 0.05;
    let opened = report.transmitted * 100.;
    match (report.echo_db, report.reduction_db) {
        (None, _) if gate_opened => (
            TEXT_MUTED,
            format!(
                "No echo of the test sound stood out at the microphone. The voice gate opened for {opened:.0}% of the test on other sound it picked up, such as speech or background noise."
            ),
        ),
        (echo, reduction) => {
            let measured = match (echo, reduction) {
                (None, _) => "No echo of the test sound reached Nuschl (headphones, or a system canceller removed it).".to_string(),
                (Some(echo), Some(reduction)) => format!(
                    "The test sound reached the microphone {:.0} dB below its level; processing removed another {reduction:.0} dB.",
                    -echo
                ),
                (Some(echo), None) => format!("The test sound reached the microphone {:.0} dB below its level.", -echo),
            };
            if gate_opened {
                (
                    WARNING,
                    format!(
                        "{measured} The voice gate opened for {opened:.0}% of it, so others may hear themselves: use headphones, lower the speaker volume, or set echo cancellation to Always on."
                    ),
                )
            } else {
                (
                    SUCCESS,
                    format!(
                        "{measured} Nothing would have been sent, so others will not hear themselves."
                    ),
                )
            }
        }
    }
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
                        "Speak, and watch your level and the voice gate under Transmission."
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
            "Playing the test sound through your speakers for about 7 seconds…".to_string(),
        )),
        EchoTestState::Failed(message) => Some((WARNING, message.clone())),
        EchoTestState::Done(report) => Some(echo_verdict(report)),
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
    // Shown before anything plays, and kept in place while it does. "About
    // 7 seconds" is checked against the worker's timeline by
    // `settings_says_about_seven_seconds` in apps/media-worker/src/probe.rs.
    rows.push(
        div()
            .flex()
            .items_start()
            .gap_2()
            .text_xs()
            .text_color(color(TEXT_MUTED))
            .child(
                div()
                    .w(px(18.))
                    .flex_shrink_0()
                    .flex()
                    .justify_center()
                    .child(icon("alert", 14., color(WARNING))),
            )
            .child(
                div().flex_1().min_w_0().child(
                    text!("Turn your volume down first: the test plays a speech-like sound for about 7 seconds. Use your speakers to find out whether the microphone picks them up; with headphones it can only confirm that no sound leaks back.")
                        .with_id("echo-test-warning"),
                ),
            )
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
    // Only while the microphone test runs: Settings alone opens no microphone.
    if let Some(preview) = this.preview.as_ref().filter(|preview| preview.capture) {
        voice.push(meter(
            preview,
            audio.vad_sensitivity,
            transmit,
            muted_in_calls(this),
        ));
    }
    if transmit == TransmitMode::VoiceActivity {
        let slider = labelled_slider(
            this,
            "sensitivity",
            ("Sensitivity", "Voice activity sensitivity"),
            crate::slider::Setting::Sensitivity,
            cx,
        );
        // Which way it goes: Mumble's similar slider is a threshold, where
        // higher is stricter; this one is the other way round.
        let direction = div()
            .flex()
            .justify_between()
            .gap_3()
            .text_xs()
            .text_color(color(TEXT_FAINT))
            .child(text!("0 %: stricter").with_id(("sensitivity", 3_usize)))
            .child(text!("100 %: more sensitive").with_id(("sensitivity", 4_usize)));
        let slider = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(slider)
            .child(direction)
            .into_any_element();
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
            this,
            "sound-volume",
            ("Volume", "Sound effect volume"),
            crate::slider::Setting::SoundVolume,
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
                .child(section(
                    "VOICE QUALITY",
                    crate::bitrate::settings_rows(this, cx),
                ))
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

#[cfg(test)]
mod echo_verdict_tests {
    // Not `super::*`: GPUI's prelude brings its own `test` attribute.
    use super::echo_verdict;
    use crate::{
        preview::EchoReport,
        theme::{SUCCESS, TEXT_MUTED, Token, WARNING},
    };

    fn verdict(
        echo_db: Option<f32>,
        reduction_db: Option<f32>,
        transmitted: f32,
    ) -> (Token, String) {
        echo_verdict(&EchoReport {
            echo_db,
            reduction_db,
            transmitted,
        })
    }

    #[test]
    fn a_gate_opened_by_other_sound_is_not_called_echo() {
        // A constant test tone (or speech) at the microphone, no echo.
        let (tone, text) = verdict(None, None, 1.);
        assert_eq!(tone, TEXT_MUTED, "{text}");
        assert!(
            text.contains("opened for 100% of the test on other sound"),
            "{text}"
        );
        assert!(!text.contains("hear themselves"), "{text}");
        assert!(!text.contains("Always on"), "{text}");
    }

    #[test]
    fn detected_echo_that_opens_the_gate_still_warns() {
        let (tone, text) = verdict(Some(-31.), Some(2.), 0.8);
        assert_eq!(tone, WARNING, "{text}");
        assert!(
            text.contains("31 dB below") && text.contains("others may hear themselves"),
            "{text}"
        );
    }

    #[test]
    fn a_closed_gate_sends_nothing_with_or_without_echo() {
        for echo_db in [None, Some(-31.)] {
            let (tone, text) = verdict(echo_db, echo_db.map(|_| 40.), 0.);
            assert_eq!(tone, SUCCESS, "{text}");
            assert!(
                text.ends_with("Nothing would have been sent, so others will not hear themselves.")
            );
        }
        let (_, text) = verdict(None, None, 0.);
        assert!(
            text.starts_with("No echo of the test sound reached Nuschl"),
            "{text}"
        );
    }
}
