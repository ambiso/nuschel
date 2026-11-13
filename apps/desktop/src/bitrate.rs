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
    if let Some(custom) = custom_note(requested) {
        rows.push(line("voice-bitrate-custom".into(), custom, TEXT_MUTED));
    }
    rows.extend(
        settings_lines(&this.client.servers, requested)
            .into_iter()
            .map(|entry| match entry {
                Line::Below(index, text) => {
                    line(("voice-bitrate-server", index).into(), text, WARNING)
                }
                Line::More(text) => line("voice-bitrate-more".into(), text, TEXT_MUTED),
                Line::Others(text) => line("voice-bitrate-others".into(), text, TEXT_MUTED),
            }),
    );
    rows
}

/// "Custom: 80 kb/s" for a configured rate none of the choices shows.
fn custom_note(requested: u32) -> Option<String> {
    (!VOICE_BITRATE_CHOICES.contains(&requested)).then(|| format!("Custom: {}", kbps(requested)))
}

/// One line under the voice bitrate choice.
#[derive(Debug, PartialEq, Eq)]
enum Line {
    /// A server, by index, that sends less than requested.
    Below(usize, String),
    /// How many more are below it than listed.
    More(String),
    /// The servers that send the requested rate.
    Others(String),
}

/// Connected with Mumble voice, so it has a voice bitrate to show.
fn has_voice(server: &ServerView) -> bool {
    server.status == Status::Connected && server.has_mumble_voice()
}

/// The Settings lines for `servers` at a `requested` rate, in server order.
fn settings_lines(servers: &[ServerView], requested: u32) -> Vec<Line> {
    lines(
        servers
            .iter()
            .enumerate()
            .filter(|(_, server)| has_voice(server))
            .filter_map(|(index, server)| {
                Some((
                    index,
                    server.name.as_str(),
                    for_server(servers, index, requested)?,
                ))
            }),
    )
}

/// Up to [`LISTED`] servers below the request by name, then how many more,
/// then one line for those at the request.
fn lines<'a>(servers: impl IntoIterator<Item = (usize, &'a str, VoiceBitrate)>) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut below = 0;
    let mut met = 0;
    let mut first: Option<(&str, u32)> = None;
    for (index, name, bitrate) in servers {
        match why(bitrate.limit) {
            None => {
                met += 1;
                first = first.or(Some((name, bitrate.effective)));
            }
            Some(reason) => {
                below += 1;
                if below <= LISTED {
                    let rate = if bitrate.transmits() {
                        kbps(bitrate.effective)
                    } else {
                        "not sending".into()
                    };
                    lines.push(Line::Below(index, format!("{name}: {rate} ({reason})")));
                }
            }
        }
    }
    if below > LISTED {
        lines.push(Line::More(format!(
            "+{} more in Connection info",
            below - LISTED
        )));
    }
    if let Some((name, rate)) = first {
        let rate = kbps(rate);
        lines.push(Line::Others(match (below, met) {
            (0, 1) => format!("{name}: {rate}"),
            (0, _) => format!("All {met} servers: {rate}"),
            (_, 1) => format!("1 other server: {rate}"),
            _ => format!("{met} other servers: {rate}"),
        }));
    }
    lines
}

/// Why a server sends less than requested, in a few plain words.
fn why(limit: BitrateLimit) -> Option<&'static str> {
    match limit {
        BitrateLimit::Requested => None,
        BitrateLimit::ServerBudget | BitrateLimit::NoServerBudget => Some("server limit"),
        BitrateLimit::SharedMicrophone => Some("another server's limit"),
    }
}

/// The voice bitrate in `servers[index]`'s Connection info: "128 kb/s",
/// "48 of 128 kb/s (server limit)". None unless it is connected with voice.
pub fn connection_value(servers: &[ServerView], index: usize, requested: u32) -> Option<String> {
    if !has_voice(servers.get(index)?) {
        return None;
    }
    let bitrate = for_server(servers, index, requested)?;
    let rate = if bitrate.transmits() {
        bitrate.summary()
    } else {
        "Not sending".into()
    };
    Some(match why(bitrate.limit) {
        Some(reason) => format!("{rate} ({reason})"),
        None => rate,
    })
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
    use super::{
        LABELS, LISTED, Line, connection_value, custom_note, lines, place_line, settings_lines,
    };
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

    fn texts(lines: &[Line]) -> Vec<&str> {
        lines
            .iter()
            .map(|line| match line {
                Line::Below(_, text) | Line::More(text) | Line::Others(text) => text.as_str(),
            })
            .collect()
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
    fn a_custom_rate_is_named_only_when_no_choice_shows_it() {
        for choice in VOICE_BITRATE_CHOICES {
            assert_eq!(custom_note(choice), None);
        }
        assert_eq!(custom_note(80_000).as_deref(), Some("Custom: 80 kb/s"));
    }

    #[test]
    fn settings_list_servers_below_the_request_and_sum_up_the_rest() {
        let limited = VoiceBitrate::resolve(128_000, 72_000);
        let met = VoiceBitrate::resolve(128_000, 558_000);
        // Server order, with each line keyed by the server's own index.
        let shown = lines([
            (0, "Wide", met),
            (2, "Narrow", limited),
            (3, "Tiny", VoiceBitrate::resolve(128_000, 20_000)),
            (5, "Both", met.shared(48_000)),
            (6, "Big", met),
        ]);
        assert_eq!(
            shown,
            [
                Line::Below(2, "Narrow: 48 kb/s (server limit)".into()),
                Line::Below(3, "Tiny: not sending (server limit)".into()),
                Line::Below(5, "Both: 48 kb/s (another server's limit)".into()),
                Line::Others("2 other servers: 128 kb/s".into()),
            ]
        );
        assert_eq!(
            texts(&lines([(1, "Narrow", limited), (4, "Wide", met)])),
            ["Narrow: 48 kb/s (server limit)", "1 other server: 128 kb/s"]
        );
        // Everything at the request: one line, without a list.
        assert_eq!(texts(&lines([(0, "Wide", met)])), ["Wide: 128 kb/s"]);
        assert_eq!(
            texts(&lines([(0, "A", met), (1, "B", met), (2, "C", met)])),
            ["All 3 servers: 128 kb/s"]
        );
        // Only servers below it: no summary line.
        assert_eq!(
            texts(&lines([(0, "Narrow", limited)])),
            ["Narrow: 48 kb/s (server limit)"]
        );
        assert!(lines([]).is_empty());
    }

    #[test]
    fn settings_name_at_most_five_servers_below_the_request() {
        let limited = VoiceBitrate::resolve(128_000, 72_000);
        let met = VoiceBitrate::resolve(128_000, 558_000);
        let names: Vec<String> = (0..9).map(|index| format!("S{index}")).collect();
        let servers = |count: usize| {
            names
                .iter()
                .take(count)
                .enumerate()
                .map(|(index, name)| (index, name.as_str(), limited))
                .chain([(100, "Wide", met)])
                .collect::<Vec<_>>()
        };
        let exactly = lines(servers(LISTED));
        assert_eq!(exactly.len(), LISTED + 1);
        assert!(!exactly.iter().any(|line| matches!(line, Line::More(_))));
        let many = lines(servers(9));
        assert_eq!(LISTED, 5);
        assert_eq!(
            many.iter()
                .filter_map(|line| match line {
                    Line::Below(index, _) => Some(*index),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
        assert_eq!(
            texts(&many[LISTED..]),
            ["+4 more in Connection info", "1 other server: 128 kb/s"]
        );
        // Every line stays short.
        assert!(texts(&many).iter().all(|text| text.chars().count() <= 40));
    }

    #[test]
    fn settings_show_only_connected_servers_with_mumble_voice() {
        let mut offline = server("Offline", 72_000, false);
        offline.status = Status::Disconnected;
        let mut encrypted = server("E2EE", 72_000, false);
        encrypted.capabilities.push("e2ee/1".into());
        let servers = [
            server("Wide", 558_000, true),
            offline,
            server("Narrow", 72_000, false),
            encrypted,
            server("Other", 558_000, false),
        ];
        assert_eq!(
            settings_lines(&servers, 128_000),
            [
                Line::Below(2, "Narrow: 48 kb/s (server limit)".into()),
                Line::Others("2 other servers: 128 kb/s".into()),
            ]
        );
        assert!(settings_lines(&[], 128_000).is_empty());
    }

    #[test]
    fn connection_info_names_the_rate_of_every_server_sending_voice() {
        let mut offline = server("Offline", 558_000, false);
        offline.status = Status::Connecting;
        let mut encrypted = server("E2EE", 558_000, false);
        encrypted.capabilities.push("e2ee/1".into());
        let servers = [
            server("Wide", 558_000, false),
            server("Narrow", 72_000, false),
            server("Tiny", 20_000, false),
            offline,
            encrypted,
        ];
        let values: Vec<_> = (0..6)
            .map(|index| connection_value(&servers, index, 128_000))
            .collect();
        assert_eq!(
            values,
            [
                Some("128 kb/s".into()),
                Some("48 of 128 kb/s (server limit)".into()),
                Some("Not sending (server limit)".into()),
                None,
                None,
                None,
            ]
        );
        // Speaking on both: one encoder at the narrower server's rate.
        let both = [
            server("Wide", 558_000, true),
            server("Narrow", 72_000, true),
        ];
        assert_eq!(
            connection_value(&both, 0, 256_000).as_deref(),
            Some("48 of 256 kb/s (another server's limit)")
        );
        assert_eq!(
            connection_value(&both, 1, 256_000).as_deref(),
            Some("48 of 256 kb/s (server limit)")
        );
    }
}
