// SPDX-License-Identifier: EUPL-1.2
//! One explicit browser voice destination, independent of text navigation.
//! This module has no device, codec, network or secret-key access.
use nuschl_core::bitrate::{BitrateLimit, VoiceBitrate, voice_frame_fits};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VoiceView {
    pub server_id: String,
    pub connection_generation: String,
    pub voice_generation: String,
    pub channel_id: u32,
    pub joined: bool,
    pub muted: bool,
    pub deafened: bool,
    pub can_transmit: bool,
    /// The effective encoder target (bits per second); zero: no capture.
    pub bitrate: u32,
    /// What the user asked for, the server's advertised budget, and why
    /// `bitrate` is lower, if it is (nuschl_core::bitrate).
    pub requested_bitrate: u32,
    pub server_bandwidth: u32,
    pub bitrate_limit: BitrateLimit,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Context {
    pub server_id: String,
    pub connection_generation: String,
    pub voice_generation: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ready {
    #[serde(flatten)]
    pub context: Context,
    pub capture: bool,
    pub playback: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outgoing {
    #[serde(flatten)]
    pub context: Context,
    pub sequence: String,
    pub end: bool,
}

#[derive(Serialize)]
pub struct Incoming {
    #[serde(flatten)]
    pub context: VoiceView,
    pub speaker: u32,
    pub sequence: String,
    pub opus: Vec<u8>,
    pub end: bool,
    pub volume: f32,
}

#[derive(Default)]
pub struct Route {
    epoch: u64,
    view: Option<VoiceView>,
    capture_ready: bool,
    playback_ready: bool,
    last_sequence: Option<u64>,
    /// The user's requested target; `None`: the default.
    requested: Option<u32>,
}

impl Route {
    pub fn view(&self) -> Option<VoiceView> {
        let mut view = self.view.clone()?;
        view.can_transmit =
            view.joined && !view.muted && !view.deafened && view.bitrate != 0 && self.capture_ready;
        Some(view)
    }

    /// The user's requested target, in the supported range.
    pub fn requested(&self) -> u32 {
        self.requested
            .unwrap_or(nuschl_core::audio::DEFAULT_VOICE_BITRATE)
    }

    pub fn set_requested(&mut self, requested: u32) {
        self.requested = Some(nuschl_core::bitrate::requested_voice_bitrate(requested));
    }

    pub fn select(&mut self, server: String, generation: u64, channel: u32) {
        self.invalidate();
        let target = VoiceBitrate::resolve(
            self.requested(),
            nuschl_core::audio::DEFAULT_SERVER_BANDWIDTH,
        );
        self.view = Some(VoiceView {
            server_id: server,
            connection_generation: generation.to_string(),
            voice_generation: self.epoch.to_string(),
            channel_id: channel,
            joined: false,
            muted: true,
            deafened: true,
            can_transmit: false,
            bitrate: target.effective,
            requested_bitrate: target.requested,
            server_bandwidth: target.budget,
            bitrate_limit: target.limit,
        });
    }

    fn invalidate(&mut self) {
        self.epoch = self
            .epoch
            .checked_add(1)
            .expect("voice generation exhausted");
        self.capture_ready = false;
        self.playback_ready = false;
        self.last_sequence = None;
    }

    pub fn clear(&mut self) {
        if self.view.is_some() {
            self.invalidate();
            self.view = None;
        }
    }

    /// Policy changes revoke all previously acknowledged media work immediately.
    pub fn policy(&mut self, joined: bool, muted: bool, deafened: bool) {
        let muted = muted || deafened || !joined;
        let deafened = deafened || !joined;
        if self
            .view
            .as_ref()
            .is_some_and(|v| (v.joined, v.muted, v.deafened) != (joined, muted, deafened))
        {
            self.invalidate();
            if let Some(v) = &mut self.view {
                v.voice_generation = self.epoch.to_string();
                v.joined = joined;
                v.muted = muted;
                v.deafened = deafened;
            }
        }
    }

    /// A new route joins only once its Join's self-state is confirmed (or its
    /// retries ran out), so that echo cannot replace the first joined view;
    /// a joined route stays joined while later changes settle.
    pub fn observe(&mut self, present: bool, settling: bool, muted: bool, deafened: bool) {
        let joined = present && (!settling || self.view.as_ref().is_some_and(|v| v.joined));
        self.policy(joined, muted, deafened);
    }

    /// Revoke queued work/readiness when the effective target changes (a
    /// request or the destination's budget). Playback and explicit capture
    /// consent are otherwise independent. A request the budget still caps
    /// at the same rate changes only what is shown.
    pub fn set_target(&mut self, target: VoiceBitrate) {
        if self
            .view
            .as_ref()
            .is_some_and(|v| v.bitrate != target.effective)
        {
            self.invalidate();
            if let Some(view) = &mut self.view {
                view.bitrate = target.effective;
                view.voice_generation = self.epoch.to_string();
            }
        }
        if let Some(view) = &mut self.view {
            view.requested_bitrate = target.requested;
            view.server_bandwidth = target.budget;
            view.bitrate_limit = target.limit;
        }
    }

    pub fn matches(&self, context: &Context) -> bool {
        self.view.as_ref().is_some_and(|v| {
            v.server_id == context.server_id
                && v.connection_generation == context.connection_generation
                && v.voice_generation == context.voice_generation
        })
    }

    pub fn ready(&mut self, ready: Ready) -> Result<(), String> {
        if !self.matches(&ready.context) {
            return Err("Stale audio route.".into());
        }
        let v = self.view.as_ref().expect("matched");
        self.capture_ready = ready.capture && v.joined && !v.muted && !v.deafened;
        self.playback_ready = ready.playback && v.joined && !v.deafened;
        Ok(())
    }

    pub fn admit(&mut self, packet: &Outgoing, len: usize) -> Result<u64, String> {
        if !self.matches(&packet.context)
            || !self.view().is_some_and(|v| v.can_transmit)
            || !voice_frame_fits(self.view.as_ref().map_or(0, |v| v.bitrate), len)
        {
            return Err("Voice packet is not authorized for the active route.".into());
        }
        let sequence = decimal(&packet.sequence).ok_or("Invalid voice sequence.")?;
        if self.last_sequence.is_some_and(|old| sequence <= old) {
            return Err("Old voice packet.".into());
        }
        self.last_sequence = Some(sequence);
        Ok(sequence)
    }

    pub fn receive(&self, server: &str, generation: u64) -> Option<VoiceView> {
        self.view().filter(|v| {
            self.playback_ready
                && v.joined
                && !v.deafened
                && v.server_id == server
                && v.connection_generation == generation.to_string()
        })
    }
}

fn decimal(value: &str) -> Option<u64> {
    if value.is_empty()
        || value.len() > 20
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    value.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context(route: &Route) -> Context {
        let v = route.view().unwrap();
        Context {
            server_id: v.server_id,
            connection_generation: v.connection_generation,
            voice_generation: v.voice_generation,
        }
    }
    #[test]
    fn capture_requires_explicit_route_join_policy_and_readiness() {
        let mut route = Route::default();
        route.select("one".into(), 5, 2);
        let stale = context(&route);
        route
            .ready(Ready {
                context: context(&route),
                capture: true,
                playback: true,
            })
            .unwrap();
        assert!(!route.view().unwrap().can_transmit);
        route.policy(true, false, false);
        assert!(!route.view().unwrap().can_transmit);
        assert!(
            route
                .ready(Ready {
                    context: stale,
                    capture: true,
                    playback: true
                })
                .is_err()
        );
        route
            .ready(Ready {
                context: context(&route),
                capture: true,
                playback: true,
            })
            .unwrap();
        let packet = Outgoing {
            context: context(&route),
            sequence: "9007199254740993".into(),
            end: false,
        };
        assert_eq!(route.admit(&packet, 120).unwrap(), 9007199254740993);
        assert!(route.admit(&packet, 120).is_err());
        route.policy(true, false, true);
        assert!(route.view().unwrap().muted);
        assert!(route.admit(&packet, 20).is_err());
        assert!(route.receive("one", 5).is_none());
    }
    #[test]
    fn live_budget_changes_revoke_queued_frames_without_affecting_playback_policy() {
        let mut route = Route::default();
        route.select("one".into(), 1, 2);
        route.policy(true, false, false);
        route.set_target(VoiceBitrate::resolve(128_000, 558_000));
        route
            .ready(Ready {
                context: context(&route),
                capture: true,
                playback: true,
            })
            .unwrap();
        let old = Outgoing {
            context: context(&route),
            sequence: "0".into(),
            end: false,
        };
        assert!(route.admit(&old, 320).is_ok());
        assert!(
            route
                .admit(
                    &Outgoing {
                        context: context(&route),
                        sequence: "2".into(),
                        end: false
                    },
                    321
                )
                .is_err()
        );
        route.set_target(VoiceBitrate::resolve(128_000, 48_000));
        assert!(route.admit(&old, 40).is_err());
        assert!(!route.view().unwrap().can_transmit);
        route
            .ready(Ready {
                context: context(&route),
                capture: true,
                playback: true,
            })
            .unwrap();
        assert!(
            route
                .admit(
                    &Outgoing {
                        context: context(&route),
                        sequence: "2".into(),
                        end: false
                    },
                    61
                )
                .is_err()
        );
        assert!(
            route
                .admit(
                    &Outgoing {
                        context: context(&route),
                        sequence: "2".into(),
                        end: false
                    },
                    60
                )
                .is_ok()
        );
        route.set_target(VoiceBitrate::resolve(128_000, 29_999));
        route
            .ready(Ready {
                context: context(&route),
                capture: true,
                playback: true,
            })
            .unwrap();
        assert!(!route.view().unwrap().can_transmit);
        assert!(route.receive("one", 1).is_some());
    }

    #[test]
    fn requested_targets_revoke_only_when_the_effective_rate_changes() {
        let ready = |route: &mut Route| {
            route
                .ready(Ready {
                    context: context(route),
                    capture: true,
                    playback: true,
                })
                .unwrap()
        };
        let mut route = Route::default();
        assert_eq!(route.requested(), 128_000);
        route.set_requested(256_000);
        route.select("one".into(), 1, 2);
        route.policy(true, false, false);
        // Until the server's budget is known: the conservative fallback.
        let view = route.view().unwrap();
        assert_eq!(
            (view.requested_bitrate, view.bitrate, view.bitrate_limit),
            (256_000, 48_000, BitrateLimit::ServerBudget)
        );
        route.set_target(VoiceBitrate::resolve(route.requested(), 558_000));
        ready(&mut route);
        let before = context(&route);
        assert!(
            route
                .admit(
                    &Outgoing {
                        context: context(&route),
                        sequence: "0".into(),
                        end: false
                    },
                    640
                )
                .is_ok()
        );
        // A lower request changes the encoder: earlier work is revoked and
        // a frame over the new allowance is refused, never replayed.
        route.set_requested(64_000);
        route.set_target(VoiceBitrate::resolve(route.requested(), 558_000));
        assert!(!route.view().unwrap().can_transmit);
        let stale = Outgoing {
            context: before,
            sequence: "2".into(),
            end: false,
        };
        assert!(route.admit(&stale, 100).is_err());
        ready(&mut route);
        let fresh = |sequence: &str, route: &Route| Outgoing {
            context: context(route),
            sequence: sequence.into(),
            end: false,
        };
        assert!(route.admit(&fresh("4", &route), 161).is_err());
        assert!(route.admit(&fresh("6", &route), 160).is_ok());
        // Against a 72k server the budget caps both 64k and 256k at 48k:
        // the request is shown, but nothing is revoked.
        route.set_target(VoiceBitrate::resolve(64_000, 72_000));
        ready(&mut route);
        let generation = route.view().unwrap().voice_generation;
        route.set_requested(256_000);
        route.set_target(VoiceBitrate::resolve(route.requested(), 72_000));
        let view = route.view().unwrap();
        assert_eq!(view.voice_generation, generation);
        assert!(view.can_transmit);
        assert_eq!(
            (
                view.requested_bitrate,
                view.bitrate,
                view.server_bandwidth,
                view.bitrate_limit
            ),
            (256_000, 48_000, 72_000, BitrateLimit::ServerBudget)
        );
        // A budget too small for voice: listen, never send.
        route.set_target(VoiceBitrate::resolve(256_000, 20_000));
        ready(&mut route);
        assert!(!route.view().unwrap().can_transmit);
        assert!(route.receive("one", 1).is_some());
        assert_eq!(
            route.view().unwrap().bitrate_limit,
            BitrateLimit::NoServerBudget
        );
    }

    #[test]
    fn replacement_and_policy_changes_revoke_both_directions() {
        let mut route = Route::default();
        route.select("one".into(), 1, 4);
        route.policy(true, false, false);
        route
            .ready(Ready {
                context: context(&route),
                capture: true,
                playback: true,
            })
            .unwrap();
        assert!(route.receive("one", 1).is_some());
        assert!(route.receive("two", 1).is_none());
        assert!(route.receive("one", 2).is_none());
        let old = context(&route);
        route.select("two".into(), 1, 4);
        assert!(!route.view().unwrap().can_transmit);
        assert!(
            route
                .ready(Ready {
                    context: old,
                    capture: true,
                    playback: true
                })
                .is_err()
        );
        route.policy(true, false, false);
        let old = context(&route);
        route.policy(true, true, false);
        assert!(
            route
                .ready(Ready {
                    context: old,
                    capture: true,
                    playback: true
                })
                .is_err()
        );
        route.clear();
        assert!(route.view().is_none());
    }
    #[test]
    fn a_new_route_is_joined_only_after_its_own_self_state_is_confirmed() {
        let mut route = Route::default();
        route.select("one".into(), 1, 2);
        let selected = route.view().unwrap().voice_generation;
        // Present in the channel, but Join's self-state request is unconfirmed.
        route.observe(true, true, true, true);
        let view = route.view().unwrap();
        assert!(!view.joined);
        assert_eq!(
            view.voice_generation, selected,
            "holding back is no new view"
        );
        route.observe(true, false, true, false);
        let joined = route.view().unwrap();
        assert!(joined.joined && joined.muted && !joined.deafened && !joined.can_transmit);
        assert_ne!(joined.voice_generation, selected);
        // A later explicit change keeps the route joined; only the media
        // gates follow it.
        route.observe(true, true, true, false);
        assert_eq!(route.view().unwrap(), joined);
        // Leaving the channel always unjoins.
        route.observe(false, false, true, false);
        assert!(!route.view().unwrap().joined);
        // A fresh selection is confirmed again.
        route.select("one".into(), 1, 2);
        route.observe(true, true, true, true);
        assert!(!route.view().unwrap().joined);
    }
    #[test]
    fn context_and_sequences_are_strict_and_bounded() {
        for bad in ["", "01", "+1", "-1", "1e2", "18446744073709551616"] {
            assert_eq!(decimal(bad), None);
        }
        assert_eq!(decimal("18446744073709551615"), Some(u64::MAX));
        let ready: Ready = serde_json::from_str(r#"{"server_id":"x","connection_generation":"1","voice_generation":"2","capture":true,"playback":false}"#).unwrap();
        assert!(ready.capture);
        assert!(!ready.playback);
        let packet: Outgoing = serde_json::from_str(r#"{"server_id":"x","connection_generation":"1","voice_generation":"2","sequence":"3","end":false}"#).unwrap();
        assert_eq!(packet.sequence, "3");
        assert!(serde_json::from_str::<Context>(r#"{"server_id":"x","connection_generation":"1","voice_generation":"1","extra":true}"#).is_err());
    }
}
