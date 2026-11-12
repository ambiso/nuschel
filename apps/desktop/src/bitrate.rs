// SPDX-License-Identifier: EUPL-1.2
//! The requested voice bitrate and what each server makes of it
//! (`nuschl_core::bitrate`): a Settings row of its own beside the
//! transmission controls, and the effective rate on each voice place.
use crate::{
    layout,
    settings::{Action, managed, segmented},
    state::Desktop,
    theme::*,
};
use gpui_kit::{prelude::*, *};
use nuschl_client::server::Status;
use nuschl_core::bitrate::{VOICE_BITRATE_CHOICES, VoiceBitrate, kbps};
use std::rc::Rc;

/// The labels of [`VOICE_BITRATE_CHOICES`], in order.
const LABELS: [&str; VOICE_BITRATE_CHOICES.len()] =
    ["64 kb/s", "96 kb/s", "128 kb/s", "192 kb/s", "256 kb/s"];

/// The VOICE QUALITY section: the requested target, and for every
/// connected server what it allows and why.
pub fn settings_rows(this: &Desktop, cx: &mut Context<Desktop>) -> Vec<AnyElement> {
    let requested = this.settings.voice_bitrate;
    let choose = |bitrate: u32| -> Action {
        Rc::new(
            move |this: &mut Desktop, _: &mut Window, _: &mut Context<Desktop>| {
                this.update_settings(|settings| settings.voice_bitrate = bitrate);
                // An explicit choice also replaces a voice-quality.json that
                // could not be read, even with the value shown in its place.
                if !this.managed.contains("voice_bitrate")
                    && let Err(error) = this
                        .profile
                        .save_voice_bitrate(this.settings.voice_bitrate, &this.managed)
                {
                    this.error = Some(format!("Could not save the voice bitrate: {error}"));
                }
            },
        )
    };
    let options = VOICE_BITRATE_CHOICES
        .iter()
        .zip(LABELS)
        .map(|(&bitrate, label)| (label, bitrate == requested, choose(bitrate)))
        .collect();
    let custom = !VOICE_BITRATE_CHOICES.contains(&requested);
    let mut rows = vec![
        managed(
            this,
            "voice_bitrate",
            segmented("voice-bitrate", "Voice bitrate", options, cx),
        ),
        div()
            .text_xs()
            .text_color(color(TEXT_MUTED))
            .child(layout::text(
                "voice-bitrate-note",
                format!(
                    "Requested: {}{}. Mono Opus for every server; a server's \
                     bandwidth limit can lower it, never raise it.",
                    kbps(requested),
                    if custom { " (custom)" } else { "" }
                ),
            ))
            .into_any_element(),
    ];
    for (index, server) in this.client.servers.iter().enumerate() {
        if server.status != Status::Connected || !server.has_mumble_voice() {
            continue;
        }
        let Some(bitrate) = this.client.voice_bitrate(index, requested) else {
            continue;
        };
        rows.push(
            div()
                .text_xs()
                .text_color(color(
                    if bitrate.limit == nuschl_core::bitrate::BitrateLimit::Requested {
                        TEXT_MUTED
                    } else {
                        WARNING
                    },
                ))
                .child(layout::text(
                    ("voice-bitrate-server", index),
                    format!("{}: {}", server.name, bitrate.explanation()),
                ))
                .into_any_element(),
        );
    }
    rows
}

/// One line for a voice place, short enough for the voice card: "128 kb/s",
/// "48 of 128 kb/s · server limit". Settings has the whole explanation.
pub fn place_line(bitrate: &VoiceBitrate) -> String {
    use nuschl_core::bitrate::BitrateLimit;
    let rate = || bitrate.summary();
    match bitrate.limit {
        BitrateLimit::Requested => rate(),
        BitrateLimit::ServerBudget => format!("{} · server limit", rate()),
        BitrateLimit::SharedMicrophone => format!("{} · shared encoder", rate()),
        BitrateLimit::NoServerBudget => "Not sending · server limit".into(),
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: gpui's prelude has an attribute named `test` too.
    use super::{LABELS, place_line};
    use nuschl_core::bitrate::{VOICE_BITRATE_CHOICES, VoiceBitrate, kbps};

    #[test]
    fn labels_name_each_choice() {
        for (bitrate, label) in VOICE_BITRATE_CHOICES.iter().zip(LABELS) {
            assert_eq!(kbps(*bitrate), label);
        }
    }

    #[test]
    fn place_lines_name_requested_effective_and_reason() {
        assert_eq!(
            place_line(&VoiceBitrate::resolve(128_000, 558_000)),
            "128 kb/s"
        );
        assert_eq!(
            place_line(&VoiceBitrate::resolve(128_000, 72_000)),
            "48 of 128 kb/s · server limit"
        );
        assert_eq!(
            place_line(&VoiceBitrate::resolve(256_000, 558_000).shared(48_000)),
            "48 of 256 kb/s · shared encoder"
        );
        assert_eq!(
            place_line(&VoiceBitrate::resolve(256_000, 20_000)),
            "Not sending · server limit"
        );
        // The longest line still fits the voice card's 208 px at 12 px text.
        for line in [
            place_line(&VoiceBitrate::resolve(256_000, 30_500)),
            place_line(&VoiceBitrate::resolve(192_000, 179_600)),
            place_line(&VoiceBitrate::resolve(192_000, 558_000).shared(6_500)),
        ] {
            assert!(line.chars().count() <= 32, "{line}");
        }
    }
}
