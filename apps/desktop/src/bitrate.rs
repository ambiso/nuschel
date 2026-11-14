// SPDX-License-Identifier: EUPL-1.2
//! The requested voice bitrate and what each server makes of it
//! (`nuschl_core::bitrate`): a Settings row of its own beside the
//! transmission controls, the effective rate on each voice place, and each
//! server's rate in its Connection info.
use crate::{
    layout,
    settings::{Action, managed, segmented},
    state::{Desktop, ServerView},
    theme::*,
};
use gpui_kit::{prelude::*, *};
use nuschl_client::{bitrate::for_server, server::Status};
use nuschl_core::bitrate::{BitrateLimit, VOICE_BITRATE_CHOICES, VoiceBitrate, kbps};
use std::rc::Rc;

/// The labels of [`VOICE_BITRATE_CHOICES`], in order.
const LABELS: [&str; VOICE_BITRATE_CHOICES.len()] =
    ["64 kb/s", "96 kb/s", "128 kb/s", "192 kb/s", "256 kb/s"];

/// Settings names at most this many servers below the requested rate; every
/// server's Connection info shows its own.
const LISTED: usize = 5;

/// The VOICE QUALITY section: the requested target, the connected servers
/// that send less, and one line for the rest.
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
    let line = |id: ElementId, text: String, tone: Token| {
        div()
            .text_xs()
            .text_color(color(tone))
            .child(layout::text(id, text))
            .into_any_element()
    };
    let mut rows = vec![managed(
        this,
        "voice_bitrate",
        segmented("voice-bitrate", "Voice bitrate", options, cx),
    )];
    if !VOICE_BITRATE_CHOICES.contains(&requested) {
        let custom = format!("Custom: {}", kbps(requested));
        rows.push(line("voice-bitrate-custom".into(), custom, TEXT_MUTED));
    }
    for (position, (server, text)) in settings_lines(&this.client.servers, requested)
        .into_iter()
        .enumerate()
    {
        rows.push(match server {
            Some(index) => line(("voice-bitrate-server", index).into(), text, WARNING),
            None => line(("voice-bitrate-summary", position).into(), text, TEXT_MUTED),
        });
    }
    rows
}

/// Connected with Mumble voice, so it has a voice bitrate to show.
fn has_voice(server: &ServerView) -> bool {
    server.status == Status::Connected && server.has_mumble_voice()
}

/// The Settings lines at a `requested` rate, in server order: up to
/// [`LISTED`] servers below the request (with their index), then how many
/// more, then one line for the servers at the request.
fn settings_lines(servers: &[ServerView], requested: u32) -> Vec<(Option<usize>, String)> {
    let mut lines = Vec::new();
    let (mut below, mut met, mut first) = (0, 0, None);
    for (index, server) in servers.iter().enumerate().filter(|(_, s)| has_voice(s)) {
        let Some(bitrate) = for_server(servers, index, requested) else {
            continue;
        };
        if bitrate.limit == BitrateLimit::Requested {
            met += 1;
            first = first.or(Some(&server.name));
        } else {
            below += 1;
            if below <= LISTED {
                let text = format!("{}: {}", server.name, place_line(&bitrate));
                lines.push((Some(index), text));
            }
        }
    }
    if below > LISTED {
        lines.push((None, format!("+{} more in Connection info", below - LISTED)));
    }
    if let Some(name) = first {
        let rate = kbps(requested);
        lines.push((
            None,
            match (below, met) {
                (0, 1) => format!("{name}: {rate}"),
                (0, _) => format!("All {met} servers: {rate}"),
                (_, 1) => format!("1 other server: {rate}"),
                _ => format!("{met} other servers: {rate}"),
            },
        ));
    }
    lines
}

/// The voice bitrate in `servers[index]`'s Connection info, worded as on the
/// voice card. None unless it is connected with voice.
pub fn connection_value(servers: &[ServerView], index: usize, requested: u32) -> Option<String> {
    if !has_voice(servers.get(index)?) {
        return None;
    }
    Some(place_line(&for_server(servers, index, requested)?))
}

/// One line for a voice place, short enough for the voice card: "128 kb/s",
/// "48 of 128 kb/s · server limit".
pub fn place_line(bitrate: &VoiceBitrate) -> String {
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
    use super::{LABELS, LISTED, connection_value, place_line, settings_lines};
    use nuschl_client::server::{ServerState, Status};
    use nuschl_core::{
        bitrate::{VOICE_BITRATE_CHOICES, VoiceBitrate, kbps},
        bookmark::ServerBookmark,
        model::ServerLimits,
    };

    /// A connected server allowing `budget` per user; `speaking` sends it
    /// the microphone.
    fn server(name: &str, budget: u32, speaking: bool) -> ServerState {
        let mut server =
            ServerState::new(ServerBookmark::new(name, "localhost", 64738, "me").unwrap());
        server.status = Status::Connected;
        server.muted = !speaking;
        server.deafened = false;
        server.limits = Some(ServerLimits {
            max_bandwidth: Some(budget),
            ..Default::default()
        });
        server
    }

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

    #[test]
    fn settings_list_up_to_five_servers_below_the_request_then_the_rest() {
        let narrow = |count: usize| (0..count).map(|n| server(&format!("S{n}"), 72_000, false));
        let wide = |name| server(name, 558_000, false);
        let below = |n: usize| (Some(n), format!("S{n}: 48 of 128 kb/s · server limit"));
        let others = |text: &str| (None, text.to_owned());
        let cases = [
            (vec![], vec![]),
            (vec![wide("Wide")], vec![others("Wide: 128 kb/s")]),
            (
                vec![wide("A"), wide("B"), wide("C")],
                vec![others("All 3 servers: 128 kb/s")],
            ),
            (narrow(1).collect(), vec![below(0)]),
            (
                narrow(2).chain([wide("Wide")]).collect(),
                vec![below(0), below(1), others("1 other server: 128 kb/s")],
            ),
            (
                narrow(LISTED).chain([wide("Wide"), wide("Big")]).collect(),
                (0..LISTED)
                    .map(below)
                    .chain([others("2 other servers: 128 kb/s")])
                    .collect(),
            ),
            (
                narrow(LISTED + 4).chain([wide("Wide")]).collect(),
                (0..LISTED)
                    .map(below)
                    .chain([
                        others("+4 more in Connection info"),
                        others("1 other server: 128 kb/s"),
                    ])
                    .collect(),
            ),
        ];
        for (servers, lines) in cases {
            assert_eq!(settings_lines(&servers, 128_000), lines);
        }
    }

    #[test]
    fn only_connected_servers_with_mumble_voice_have_a_rate() {
        let mut offline = server("Offline", 72_000, false);
        offline.status = Status::Connecting;
        let mut encrypted = server("E2EE", 72_000, false);
        encrypted.capabilities.push("e2ee/1".into());
        let servers = [
            server("Wide", 558_000, false),
            offline,
            server("Tiny", 20_000, false),
            encrypted,
        ];
        assert_eq!(
            settings_lines(&servers, 128_000),
            [
                (Some(2), "Tiny: Not sending · server limit".into()),
                (None, "1 other server: 128 kb/s".into()),
            ]
        );
        let values: Vec<_> = (0..5)
            .map(|index| connection_value(&servers, index, 128_000))
            .collect();
        let tiny = Some("Not sending · server limit".into());
        assert_eq!(values, [Some("128 kb/s".into()), None, tiny, None, None]);
    }
}
