// SPDX-License-Identifier: EUPL-1.2
//! Audio preferences shared by the desktop, settings storage and the media worker.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransmitMode {
    /// Transmit while speech is detected (Mumble's default).
    #[default]
    VoiceActivity,
    PushToTalk,
    Continuous,
}

/// Whether Nuschl runs its own echo canceller on the selected microphone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EchoMode {
    /// Reuse a recognised upstream canceller, otherwise run Nuschl's.
    #[default]
    Auto,
    On,
    Off,
}

/// Who is cancelling echo on the active capture route.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "device", rename_all = "snake_case")]
pub enum EchoSource {
    #[default]
    None,
    Nuschl,
    /// A recognised system canceller (e.g. PipeWire or PulseAudio echo-cancel).
    Upstream(String),
}

/// Requested mono VoIP target; the destination's network budget takes priority.
pub const DEFAULT_VOICE_BITRATE: u32 = 128_000;
pub const MIN_VOICE_BITRATE: u32 = 6_000;
/// Conservative fallback until the server advertises its Mumble network budget.
pub const DEFAULT_SERVER_BANDWIDTH: u32 = 72_000;
/// At 50 packets/s, reserve 60 bytes/packet for Mumble framing/transport.
/// This is conservative for the server's Mumble bandwidth accounting, not an
/// estimate of all traffic on the additional browser WebSocket relay hop.
pub const VOICE_OVERHEAD_BITRATE: u32 = 24_000;

/// Zero means the budget cannot support even the minimum codec rate: listen
/// without transmitting, rather than clamping upwards past the server limit.
pub fn voice_bitrate(max_bandwidth: u32) -> u32 {
    let available = max_bandwidth.saturating_sub(VOICE_OVERHEAD_BITRATE);
    if available < MIN_VOICE_BITRATE {
        0
    } else {
        available.min(DEFAULT_VOICE_BITRATE)
    }
}

/// The voice gate's speech-probability thresholds: a closed gate opens at
/// `open` or above, an open one stays open down to `close`. The media worker's
/// gate and the Settings meter both use this, so the meter's markers cannot
/// drift from what actually opens the gate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GateThresholds {
    pub open: f32,
    pub close: f32,
}

/// Thresholds for a voice activity sensitivity (0 = only clear speech, 1 = very
/// sensitive), clamped to 0..=1 as the gate always has.
pub fn gate_thresholds(sensitivity: f32) -> GateThresholds {
    let sensitivity = sensitivity.clamp(0., 1.);
    let open = 0.9 - 0.6 * sensitivity;
    GateThresholds {
        open,
        close: (open - 0.3).max(0.05),
    }
}

pub const MAX_DEVICE_OVERRIDES: usize = 64;

/// 20 ms of digital silence as one Opus packet (CELT, fullband, mono). It
/// ends a transmission when the user mutes: a terminator must carry nothing
/// the microphone captured.
pub const SILENT_OPUS: [u8; 3] = [0xf8, 0xff, 0xfe];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioSettings {
    pub transmit: TransmitMode,
    /// 0 = only clear speech opens the gate, 1 = very sensitive.
    pub vad_sensitivity: f32,
    pub noise_suppression: bool,
    pub echo_cancellation: EchoMode,
    pub automatic_gain: bool,
    /// Backend device identifiers; `None` follows the system default.
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    /// Keep transmitting briefly after push-to-talk is released.
    pub ptt_release_ms: u32,
    /// Remembered per-microphone echo decisions, keyed by device identifier.
    pub device_overrides: BTreeMap<String, EchoMode>,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            transmit: TransmitMode::VoiceActivity,
            vad_sensitivity: 0.5,
            noise_suppression: true,
            echo_cancellation: EchoMode::Auto,
            automatic_gain: true,
            input_device: None,
            output_device: None,
            ptt_release_ms: 150,
            device_overrides: BTreeMap::new(),
        }
    }
}

impl AudioSettings {
    /// Clamp values from disk or IPC into their supported ranges.
    pub fn sanitized(mut self) -> Self {
        if !self.vad_sensitivity.is_finite() {
            self.vad_sensitivity = 0.5;
        }
        self.vad_sensitivity = self.vad_sensitivity.clamp(0., 1.);
        self.ptt_release_ms = self.ptt_release_ms.min(2000);
        for device in [&mut self.input_device, &mut self.output_device] {
            if device
                .as_ref()
                .is_some_and(|id| id.is_empty() || id.len() > 512)
            {
                *device = None;
            }
        }
        while self.device_overrides.len() > MAX_DEVICE_OVERRIDES {
            self.device_overrides.pop_first();
        }
        self.device_overrides
            .retain(|id, _| !id.is_empty() && id.len() <= 512);
        self
    }

    /// The echo decision for one microphone, honouring a remembered override.
    pub fn echo_for(&self, device: &str) -> EchoMode {
        self.device_overrides
            .get(device)
            .copied()
            .unwrap_or(self.echo_cancellation)
    }

    /// Whether both open the same devices with the same echo decisions. A
    /// different route needs new native streams (a new media worker); anything
    /// else applies to the running capture chain.
    pub fn same_route(&self, other: &Self) -> bool {
        self.input_device == other.input_device
            && self.output_device == other.output_device
            && self.echo_cancellation == other.echo_cancellation
            && self.device_overrides == other.device_overrides
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
}

/// The sound effects the media worker plays. Pitch rises for on, joined and
/// connected, and falls for their opposites.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sound {
    Connected,
    Disconnected,
    /// Someone entered or left your voice channel.
    UserJoined,
    UserLeft,
    Muted,
    Unmuted,
    Deafened,
    Undeafened,
    /// Push-to-talk pressed and released.
    TalkOn,
    TalkOff,
    Message,
    /// A mention of your name or a direct message.
    Mention,
}

impl Sound {
    pub const ALL: [Sound; 12] = [
        Sound::Connected,
        Sound::Disconnected,
        Sound::UserJoined,
        Sound::UserLeft,
        Sound::Muted,
        Sound::Unmuted,
        Sound::Deafened,
        Sound::Undeafened,
        Sound::TalkOn,
        Sound::TalkOff,
        Sound::Message,
        Sound::Mention,
    ];
}

#[cfg(test)]
mod tests {
    #[test]
    fn gate_thresholds_keep_the_gates_arithmetic() {
        for step in 0..=100 {
            let sensitivity = step as f32 / 100.;
            let open = 0.9 - 0.6 * sensitivity;
            let thresholds = gate_thresholds(sensitivity);
            assert_eq!(thresholds.open.to_bits(), open.to_bits());
            assert_eq!(thresholds.close.to_bits(), (open - 0.3).max(0.05).to_bits());
            assert!(thresholds.close < thresholds.open);
        }
        assert_eq!(gate_thresholds(-1.), gate_thresholds(0.));
        assert_eq!(gate_thresholds(2.), gate_thresholds(1.));
        assert_eq!(gate_thresholds(1.).close, 0.05);
    }

    use super::*;
    #[test]
    fn default_voice_target_reserves_overhead_and_never_exceeds_a_server_budget() {
        assert_eq!(DEFAULT_VOICE_BITRATE, 128_000);
        for (budget, target) in [
            (0, 0),
            (24_000, 0),
            (29_999, 0),
            (30_000, 6_000),
            (72_000, 48_000),
            (128_000, 104_000),
            (152_000, 128_000),
            (u32::MAX, 128_000),
        ] {
            assert_eq!(voice_bitrate(budget), target);
            if target != 0 {
                assert!(target + VOICE_OVERHEAD_BITRATE <= budget);
            }
        }
    }

    #[test]
    fn defaults_follow_the_product_requirements() {
        let settings = AudioSettings::default();
        assert_eq!(settings.transmit, TransmitMode::VoiceActivity);
        assert!(settings.noise_suppression && settings.automatic_gain);
        assert_eq!(settings.echo_cancellation, EchoMode::Auto);
    }
    #[test]
    fn sanitizes_untrusted_values_and_honours_overrides() {
        let mut settings = AudioSettings {
            vad_sensitivity: f32::NAN,
            ptt_release_ms: 99_999,
            input_device: Some(String::new()),
            ..Default::default()
        };
        settings
            .device_overrides
            .insert("mic".into(), EchoMode::Off);
        let settings = settings.sanitized();
        assert_eq!(settings.vad_sensitivity, 0.5);
        assert_eq!(settings.ptt_release_ms, 2000);
        assert_eq!(settings.input_device, None);
        assert_eq!(settings.echo_for("mic"), EchoMode::Off);
        assert_eq!(settings.echo_for("other"), EchoMode::Auto);
        let parsed: AudioSettings =
            serde_json::from_str("{\"transmit\":\"push_to_talk\"}").unwrap();
        assert_eq!(parsed.transmit, TransmitMode::PushToTalk);
        assert!(parsed.noise_suppression);
    }
    #[test]
    fn only_devices_and_echo_decisions_change_the_route() {
        let settings = AudioSettings::default();
        let processing = AudioSettings {
            transmit: TransmitMode::PushToTalk,
            vad_sensitivity: 0.9,
            noise_suppression: false,
            automatic_gain: false,
            ptt_release_ms: 500,
            ..AudioSettings::default()
        };
        assert!(settings.same_route(&processing));
        for route in [
            AudioSettings {
                input_device: Some("mic".into()),
                ..AudioSettings::default()
            },
            AudioSettings {
                output_device: Some("speakers".into()),
                ..AudioSettings::default()
            },
            AudioSettings {
                echo_cancellation: EchoMode::Off,
                ..AudioSettings::default()
            },
            AudioSettings {
                device_overrides: [("mic".into(), EchoMode::On)].into(),
                ..AudioSettings::default()
            },
        ] {
            assert!(!settings.same_route(&route), "{route:?}");
        }
    }
}
