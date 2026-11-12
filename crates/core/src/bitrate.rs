// SPDX-License-Identifier: EUPL-1.2
//! The voice encoder's target: what the user asked for, what a destination's
//! advertised Mumble budget allows after the framing reserve, and why the
//! effective target is lower when it is. The desktop, the terminal client and
//! the browser show and enforce this one policy. Nothing here raises a rate
//! past a server's cap: a budget too small for voice means listening only.
use crate::audio::{DEFAULT_VOICE_BITRATE, MIN_VOICE_BITRATE, VOICE_OVERHEAD_BITRATE};
use serde::{Deserialize, Serialize};

/// The lowest target a user can choose. Lower rates are used only when a
/// destination's budget forces them.
pub const MIN_REQUESTED_VOICE_BITRATE: u32 = 64_000;
/// Opus payload bytes of one 20 ms frame at most. Stock Mumble bounds a voice
/// datagram (and a tunnelled one) at 1024 bytes including its own header and
/// the relayed sender, so the codec's 1275-byte maximum (510 kb/s) cannot be
/// sent; 960 bytes leaves room for either packet format.
pub const MAX_VOICE_FRAME_BYTES: usize = 960;
/// The highest target a user can choose. Opus lowers a mono encoder's
/// target to 300 kb/s (libopus's `OPUS_SET_BITRATE` ceiling, which opus-pure
/// shares), so a higher request would name a rate never used; 256 kb/s is
/// the last common step below it.
pub const MAX_VOICE_BITRATE: u32 = 256_000;
/// What the settings offer; any value in the supported range is accepted
/// from settings files and managed configuration.
pub const VOICE_BITRATE_CHOICES: [u32; 5] = [64_000, 96_000, 128_000, 192_000, MAX_VOICE_BITRATE];

/// A requested target from settings, IPC or the user, in the supported range.
pub fn requested_voice_bitrate(value: u32) -> u32 {
    value.clamp(MIN_REQUESTED_VOICE_BITRATE, MAX_VOICE_BITRATE)
}

/// The Opus payload one 20 ms frame may use at `bitrate` (bits per second):
/// the encoder's output buffer and every sender's admission use this one
/// bound, so a constrained-VBR frame can never exceed the destination budget.
pub fn max_voice_frame_bytes(bitrate: u32) -> usize {
    ((bitrate / 400) as usize).min(MAX_VOICE_FRAME_BYTES)
}

/// Whether an encoded 20 ms frame of `len` bytes may be sent to a
/// destination whose effective target is `bitrate`. Zero (no voice budget)
/// admits nothing.
pub fn voice_frame_fits(bitrate: u32, len: usize) -> bool {
    bitrate >= MIN_VOICE_BITRATE && len != 0 && len <= max_voice_frame_bytes(bitrate)
}

/// Why the effective target is what it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BitrateLimit {
    /// The requested target is in effect.
    Requested,
    /// The destination's advertised budget, less the framing reserve, is lower.
    ServerBudget,
    /// The budget cannot carry the codec's minimum after the reserve: this
    /// destination is listened to, but the microphone is not sent there.
    NoServerBudget,
    /// Another destination that also receives the microphone allows less:
    /// one encoder serves every destination.
    SharedMicrophone,
}

/// The requested and effective encoder target for one destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct VoiceBitrate {
    /// What the user asked for (bits per second).
    pub requested: u32,
    /// The destination's advertised per-user budget, including transport.
    pub budget: u32,
    /// The encoder target used for it; zero: nothing is transmitted.
    pub effective: u32,
    pub limit: BitrateLimit,
}

impl VoiceBitrate {
    /// The target for one destination: the request, unless the budget less
    /// the conservative Mumble framing/transport reserve is lower. A budget
    /// that leaves less than the codec minimum yields zero, never a clamp
    /// upwards past the server's cap.
    pub fn resolve(requested: u32, max_bandwidth: u32) -> Self {
        let requested = requested_voice_bitrate(requested);
        let available = max_bandwidth.saturating_sub(VOICE_OVERHEAD_BITRATE);
        let (effective, limit) = if available >= requested {
            (requested, BitrateLimit::Requested)
        } else if available < MIN_VOICE_BITRATE {
            (0, BitrateLimit::NoServerBudget)
        } else {
            (available, BitrateLimit::ServerBudget)
        };
        Self {
            requested,
            budget: max_bandwidth,
            effective,
            limit,
        }
    }

    /// The default request against `max_bandwidth`.
    pub fn default_for(max_bandwidth: u32) -> Self {
        Self::resolve(DEFAULT_VOICE_BITRATE, max_bandwidth)
    }

    /// Apply the shared encoder's target while this destination receives
    /// the microphone together with others (`encoder`: their lowest
    /// effective target). It can only lower the rate.
    pub fn shared(self, encoder: u32) -> Self {
        if self.effective != 0 && encoder >= MIN_VOICE_BITRATE && encoder < self.effective {
            Self {
                effective: encoder,
                limit: BitrateLimit::SharedMicrophone,
                ..self
            }
        } else {
            self
        }
    }

    pub fn transmits(&self) -> bool {
        self.effective != 0
    }

    /// A few words on why it is lower than requested, if it is.
    pub fn reason(&self) -> Option<&'static str> {
        match self.limit {
            BitrateLimit::Requested => None,
            BitrateLimit::ServerBudget => Some("server limit"),
            BitrateLimit::NoServerBudget => Some("server allows too little for voice"),
            BitrateLimit::SharedMicrophone => Some("shared with another server"),
        }
    }

    /// "128 kb/s" or "48 of 128 kb/s".
    pub fn summary(&self) -> String {
        if !self.transmits() {
            format!("not sending (requested {})", kbps(self.requested))
        } else if self.effective == self.requested {
            kbps(self.effective)
        } else {
            format!(
                "{} of {}",
                kbps_number(self.effective),
                kbps(self.requested)
            )
        }
    }

    /// Why, in one sentence for a status line or a screen reader.
    pub fn explanation(&self) -> String {
        match self.limit {
            BitrateLimit::Requested => format!("{}, as requested.", kbps(self.effective)),
            BitrateLimit::ServerBudget => format!(
                "{}: this server allows {} per user, {} of which is kept for Mumble packet overhead.",
                kbps(self.effective),
                kbps(self.budget),
                kbps(VOICE_OVERHEAD_BITRATE),
            ),
            BitrateLimit::NoServerBudget => format!(
                "Not sending: this server allows only {} per user, too little for voice after {} of packet overhead.",
                kbps(self.budget),
                kbps(VOICE_OVERHEAD_BITRATE),
            ),
            BitrateLimit::SharedMicrophone => format!(
                "{}: another server you speak on allows less, and one encoder serves both.",
                kbps(self.effective),
            ),
        }
    }
}

/// A rate in bits per second as "128 kb/s" (or "6.5 kb/s").
pub fn kbps(bits: u32) -> String {
    format!("{} kb/s", kbps_number(bits))
}

fn kbps_number(bits: u32) -> String {
    if bits % 1000 == 0 {
        (bits / 1000).to_string()
    } else {
        format!("{:.1}", f64::from(bits) / 1000.)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_are_ordered_supported_and_include_the_default() {
        assert!(
            VOICE_BITRATE_CHOICES
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
        assert!(VOICE_BITRATE_CHOICES.contains(&DEFAULT_VOICE_BITRATE));
        assert_eq!(VOICE_BITRATE_CHOICES[0], MIN_REQUESTED_VOICE_BITRATE);
        assert_eq!(*VOICE_BITRATE_CHOICES.last().unwrap(), MAX_VOICE_BITRATE);
        assert_eq!(MAX_VOICE_BITRATE, 256_000);
        // A choice's frames always fit a Mumble voice datagram.
        assert!(max_voice_frame_bytes(MAX_VOICE_BITRATE) < MAX_VOICE_FRAME_BYTES);
        for choice in VOICE_BITRATE_CHOICES {
            assert_eq!(requested_voice_bitrate(choice), choice);
            // Every choice is a whole number of bytes per 20 ms frame.
            assert_eq!(choice % 400, 0);
        }
    }

    #[test]
    fn requests_are_repaired_into_the_supported_range() {
        for (value, expected) in [
            (0, 64_000),
            (6_000, 64_000),
            (63_999, 64_000),
            (80_000, 80_000),
            (256_000, 256_000),
            (384_000, 256_000),
            (510_000, 256_000),
            (u32::MAX, 256_000),
        ] {
            assert_eq!(requested_voice_bitrate(value), expected);
        }
    }

    #[test]
    fn frame_admission_matches_the_encoder_buffer_and_the_datagram_bound() {
        assert_eq!(max_voice_frame_bytes(128_000), 320);
        assert_eq!(max_voice_frame_bytes(48_000), 120);
        assert_eq!(max_voice_frame_bytes(6_000), 15);
        assert_eq!(max_voice_frame_bytes(384_000), 960);
        // Never past the Mumble datagram bound, whatever is asked.
        assert_eq!(max_voice_frame_bytes(510_000), 960);
        assert_eq!(max_voice_frame_bytes(u32::MAX), 960);
        assert!(voice_frame_fits(48_000, 120));
        assert!(!voice_frame_fits(48_000, 121));
        assert!(!voice_frame_fits(48_000, 0));
        assert!(!voice_frame_fits(0, 1));
        assert!(!voice_frame_fits(5_999, 1));
        assert!(voice_frame_fits(6_000, 15));
        assert!(!voice_frame_fits(510_000, 961));
    }

    #[test]
    fn resolution_never_exceeds_the_cap_less_the_reserve() {
        use BitrateLimit::*;
        // (requested, server budget) -> (effective, reason)
        for (requested, budget, effective, limit) in [
            // The default against stock Mumble 1.3/1.4's 72 kb/s default.
            (128_000, 72_000, 48_000, ServerBudget),
            // Murmur 1.5 and nuschld default to 558 kb/s.
            (128_000, 558_000, 128_000, Requested),
            (256_000, 558_000, 256_000, Requested),
            (256_000, 280_000, 256_000, Requested),
            (256_000, 279_999, 255_999, ServerBudget),
            (64_000, 72_000, 48_000, ServerBudget),
            (64_000, 88_000, 64_000, Requested),
            (96_000, 64_000, 40_000, ServerBudget),
            (256_000, 40_000, 16_000, ServerBudget),
            (128_000, 30_000, 6_000, ServerBudget),
            (128_000, 29_999, 0, NoServerBudget),
            (256_000, 24_000, 0, NoServerBudget),
            (128_000, 0, 0, NoServerBudget),
            // Out-of-range requests are repaired first.
            (510_000, u32::MAX, 256_000, Requested),
            (6_000, u32::MAX, 64_000, Requested),
        ] {
            let resolved = VoiceBitrate::resolve(requested, budget);
            assert_eq!(
                (resolved.effective, resolved.limit),
                (effective, limit),
                "{requested} against {budget}"
            );
            assert_eq!(resolved.budget, budget);
            assert_eq!(resolved.requested, requested_voice_bitrate(requested));
            assert!(resolved.effective <= resolved.requested);
            assert!(resolved.effective <= MAX_VOICE_BITRATE);
            if resolved.transmits() {
                assert!(resolved.effective >= MIN_VOICE_BITRATE);
                assert!(
                    u64::from(resolved.effective) + u64::from(VOICE_OVERHEAD_BITRATE)
                        <= u64::from(budget)
                );
            }
        }
    }

    #[test]
    fn the_default_request_keeps_the_existing_server_budget_policy() {
        for budget in [
            0,
            24_000,
            29_999,
            30_000,
            72_000,
            128_000,
            152_000,
            558_000,
            u32::MAX,
        ] {
            assert_eq!(
                VoiceBitrate::default_for(budget).effective,
                crate::audio::voice_bitrate(budget)
            );
        }
    }

    #[test]
    fn a_shared_encoder_only_lowers_and_names_the_reason() {
        let wide = VoiceBitrate::resolve(192_000, 558_000);
        let shared = wide.shared(48_000);
        assert_eq!(shared.effective, 48_000);
        assert_eq!(shared.limit, BitrateLimit::SharedMicrophone);
        assert_eq!(shared.requested, 192_000);
        // Never upwards, never from zero, never to zero.
        assert_eq!(
            VoiceBitrate::resolve(128_000, 72_000)
                .shared(128_000)
                .effective,
            48_000
        );
        let silent = VoiceBitrate::resolve(128_000, 20_000);
        assert_eq!(silent.shared(48_000), silent);
        assert_eq!(wide.shared(0), wide);
        // A tie keeps this destination's own reason.
        let own = VoiceBitrate::resolve(128_000, 72_000);
        assert_eq!(own.shared(48_000).limit, BitrateLimit::ServerBudget);
    }

    #[test]
    fn summaries_and_explanations_name_requested_effective_and_reason() {
        assert_eq!(
            VoiceBitrate::resolve(128_000, 558_000).summary(),
            "128 kb/s"
        );
        assert_eq!(
            VoiceBitrate::resolve(128_000, 72_000).summary(),
            "48 of 128 kb/s"
        );
        assert_eq!(
            VoiceBitrate::resolve(128_000, 30_500).summary(),
            "6.5 of 128 kb/s"
        );
        assert_eq!(
            VoiceBitrate::resolve(96_000, 20_000).summary(),
            "not sending (requested 96 kb/s)"
        );
        assert_eq!(VoiceBitrate::resolve(128_000, 558_000).reason(), None);
        assert_eq!(
            VoiceBitrate::resolve(128_000, 72_000).reason(),
            Some("server limit")
        );
        assert!(VoiceBitrate::resolve(128_000, 0).reason().is_some());
        assert!(
            VoiceBitrate::resolve(192_000, 558_000)
                .shared(64_000)
                .reason()
                .is_some_and(|reason| reason.contains("another server"))
        );
        let limited = VoiceBitrate::resolve(128_000, 72_000).explanation();
        assert!(
            limited.contains("72 kb/s") && limited.contains("24 kb/s"),
            "{limited}"
        );
        assert!(
            VoiceBitrate::resolve(128_000, 20_000)
                .explanation()
                .starts_with("Not sending")
        );
        assert!(
            VoiceBitrate::resolve(192_000, 558_000)
                .shared(64_000)
                .explanation()
                .contains("another server")
        );
        assert_eq!(
            serde_json::to_value(BitrateLimit::NoServerBudget).unwrap(),
            "no_server_budget"
        );
    }
}
