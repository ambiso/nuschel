// SPDX-License-Identifier: EUPL-1.2
//! The headless client SDK every Nuschl client shares (docs/clients.md, "The
//! SDK"): timelines, conversations and server state, and the policies that
//! decide them. Plain data only, no runtime and no clock: it builds for
//! `wasm32-unknown-unknown`, and platform adapters execute what it decides.

pub mod bitrate;
pub mod client;
pub mod conversation;
pub mod favicon;
pub mod finder;
pub mod input;
#[cfg(feature = "native")]
mod link;
pub mod names;
#[cfg(feature = "native")]
pub mod native;
mod navigation;
pub mod platform;
pub mod search;
pub mod server;
pub mod sounds;
pub mod timeline;
pub mod tree;
pub mod voice;
pub use client::{Client, Focus, MessageReceipt, Notification, SecondServer, SendError, Update};
pub use navigation::*;
pub use nuschl_core::model;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClientPreferences {
    pub save_local_history: bool,
}

impl Default for ClientPreferences {
    fn default() -> Self {
        Self {
            save_local_history: true,
        }
    }
}

/// Evidence applies to the active capture route, not merely an installed effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectState {
    Active,
    Inactive,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteEffects {
    pub echo_cancellation: EffectState,
    pub noise_suppression: EffectState,
    pub gain_control: EffectState,
    pub playback_reference_matches: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessingProvider {
    Upstream,
    Nuschl,
    /// Unknown upstream processing is never treated as confirmed absence.
    Bypass,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioPlan {
    pub echo_cancellation: ProcessingProvider,
    pub noise_suppression: ProcessingProvider,
    pub gain_control: ProcessingProvider,
    pub reconnect_own_playback_reference: bool,
    pub uncertain: bool,
}

impl AudioPlan {
    /// Pure policy. A platform adapter can only apply changes to Nuschl-owned resources.
    pub fn for_route(effects: RouteEffects) -> Self {
        fn provider(state: EffectState) -> ProcessingProvider {
            match state {
                EffectState::Active => ProcessingProvider::Upstream,
                EffectState::Inactive => ProcessingProvider::Nuschl,
                EffectState::Unknown => ProcessingProvider::Bypass,
            }
        }
        Self {
            echo_cancellation: provider(effects.echo_cancellation),
            noise_suppression: provider(effects.noise_suppression),
            gain_control: provider(effects.gain_control),
            reconnect_own_playback_reference: effects.echo_cancellation == EffectState::Active
                && !effects.playback_reference_matches,
            uncertain: [
                effects.echo_cancellation,
                effects.noise_suppression,
                effects.gain_control,
            ]
            .contains(&EffectState::Unknown),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_history_is_enabled_without_configuration() -> Result<(), serde_json::Error> {
        assert!(ClientPreferences::default().save_local_history);
        assert!(serde_json::from_str::<ClientPreferences>("{}")?.save_local_history);
        Ok(())
    }

    #[test]
    fn a_missing_reference_does_not_add_a_second_canceller() {
        let plan = AudioPlan::for_route(RouteEffects {
            echo_cancellation: EffectState::Active,
            noise_suppression: EffectState::Inactive,
            gain_control: EffectState::Unknown,
            playback_reference_matches: false,
        });
        assert_eq!(plan.echo_cancellation, ProcessingProvider::Upstream);
        assert!(plan.reconnect_own_playback_reference);
        assert_eq!(plan.noise_suppression, ProcessingProvider::Nuschl);
        assert_eq!(plan.gain_control, ProcessingProvider::Bypass);
        assert!(plan.uncertain);
    }
}
