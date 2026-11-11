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

/// Whether Nuschel runs its own echo canceller on the selected microphone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EchoMode {
    /// Reuse a recognised upstream canceller, otherwise run Nuschel's.
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
    Nuschel,
    /// A recognised system canceller (e.g. PipeWire or PulseAudio echo-cancel).
    Upstream(String),
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
    use super::*;
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
