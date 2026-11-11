// SPDX-License-Identifier: EUPL-1.2
//! One explicit browser voice destination, independent of text navigation.
//! This module has no device, codec, network or secret-key access.
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
}

impl Route {
    pub fn view(&self) -> Option<VoiceView> {
        let mut view = self.view.clone()?;
        view.can_transmit = view.joined && !view.muted && !view.deafened && self.capture_ready;
        Some(view)
    }

    pub fn select(&mut self, server: String, generation: u64, channel: u32) {
        self.invalidate();
        self.view = Some(VoiceView {
            server_id: server,
            connection_generation: generation.to_string(),
            voice_generation: self.epoch.to_string(),
            channel_id: channel,
            joined: false,
            muted: true,
            deafened: true,
            can_transmit: false,
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
            || len == 0
            || len > 960
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
        assert_eq!(route.admit(&packet, 960).unwrap(), 9007199254740993);
        assert!(route.admit(&packet, 960).is_err());
        route.policy(true, false, true);
        assert!(route.view().unwrap().muted);
        assert!(route.admit(&packet, 20).is_err());
        assert!(route.receive("one", 5).is_none());
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
