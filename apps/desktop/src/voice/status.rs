// SPDX-License-Identifier: EUPL-1.2
//! A presentation of current voice facts, not another routing state machine.
//! In particular, microphone eligibility is never evidence of sent audio.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Connection {
    Offline,
    Connecting,
    Reconnecting,
    Connected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    Negotiating,
    Available,
    Unsupported,
    EndToEnd,
}

#[derive(Clone, Copy, Debug)]
pub struct Facts {
    pub connection: Connection,
    pub availability: Availability,
    pub destination: bool,
    pub muted: bool,
    pub deafened: bool,
    pub server_muted: bool,
    pub server_deafened: bool,
    pub suppressed: bool,
    pub acknowledged: bool,
    pub transmit_budget: bool,
    pub push_muted: bool,
    pub sending: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Offline,
    Connecting,
    Reconnecting,
    Negotiating,
    Unsupported,
    EndToEnd,
    Deafened,
    ServerDeafened,
    Listening,
    Muted,
    ServerMuted,
    Suppressed,
    NoTransmitBudget,
    AwaitingAcknowledgement,
    PushMuted,
    MicrophoneEnabled,
    Speaking,
}

impl Facts {
    pub fn state(self) -> State {
        match self.connection {
            Connection::Offline => return State::Offline,
            Connection::Connecting => return State::Connecting,
            Connection::Reconnecting => return State::Reconnecting,
            Connection::Connected => {}
        }
        match self.availability {
            Availability::Negotiating => return State::Negotiating,
            Availability::Unsupported => return State::Unsupported,
            Availability::EndToEnd => return State::EndToEnd,
            Availability::Available => {}
        }
        if self.deafened {
            State::Deafened
        } else if self.server_deafened {
            State::ServerDeafened
        } else if !self.destination {
            State::Listening
        } else if self.muted {
            State::Muted
        } else if self.server_muted {
            State::ServerMuted
        } else if self.suppressed {
            State::Suppressed
        } else if !self.transmit_budget {
            State::NoTransmitBudget
        } else if !self.acknowledged {
            State::AwaitingAcknowledgement
        } else if self.push_muted {
            State::PushMuted
        } else if self.sending {
            State::Speaking
        } else {
            State::MicrophoneEnabled
        }
    }
}

impl State {
    /// Short labels describe server-scoped intent/observations. "Enabled"
    /// deliberately does not promise device readiness or continuous speech.
    pub fn label(self) -> &'static str {
        match self {
            Self::Offline => "Disconnected",
            Self::Connecting => "Connecting",
            Self::Reconnecting => "Reconnecting",
            Self::Negotiating => "Negotiating voice",
            Self::Unsupported => "Voice unavailable: Opus unsupported",
            Self::EndToEnd => "Voice unavailable: encrypted session",
            Self::Deafened => "Deafened",
            Self::ServerDeafened => "Deafened by server",
            Self::Listening => "Listening",
            Self::Muted => "Microphone destination · muted",
            Self::ServerMuted => "Microphone destination · server muted",
            Self::Suppressed => "Microphone destination · suppressed",
            Self::NoTransmitBudget => "Microphone unavailable: server bandwidth",
            Self::AwaitingAcknowledgement => "Microphone destination · awaiting server",
            Self::PushMuted => "Microphone destination · push to mute",
            Self::MicrophoneEnabled => "Microphone destination · enabled",
            Self::Speaking => "Speaking",
        }
    }

    pub fn speaking(self) -> bool {
        self == Self::Speaking
    }

    pub fn microphone_enabled(self) -> bool {
        matches!(self, Self::MicrophoneEnabled | Self::Speaking)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live() -> Facts {
        Facts {
            connection: Connection::Connected,
            availability: Availability::Available,
            destination: true,
            muted: false,
            deafened: false,
            server_muted: false,
            server_deafened: false,
            suppressed: false,
            acknowledged: true,
            transmit_budget: true,
            push_muted: false,
            sending: false,
        }
    }

    #[test]
    fn microphone_intent_is_not_speech() {
        assert_eq!(live().state(), State::MicrophoneEnabled);
        assert!(!live().state().speaking());
        assert_eq!(
            Facts {
                sending: true,
                ..live()
            }
            .state(),
            State::Speaking
        );
    }

    #[test]
    fn privacy_and_server_blocks_override_stale_sending() {
        let active = Facts {
            sending: true,
            ..live()
        };
        for (facts, expected) in [
            (
                Facts {
                    muted: true,
                    ..active
                },
                State::Muted,
            ),
            (
                Facts {
                    deafened: true,
                    ..active
                },
                State::Deafened,
            ),
            (
                Facts {
                    server_muted: true,
                    ..active
                },
                State::ServerMuted,
            ),
            (
                Facts {
                    server_deafened: true,
                    ..active
                },
                State::ServerDeafened,
            ),
            (
                Facts {
                    suppressed: true,
                    ..active
                },
                State::Suppressed,
            ),
            (
                Facts {
                    transmit_budget: false,
                    ..active
                },
                State::NoTransmitBudget,
            ),
            (
                Facts {
                    acknowledged: false,
                    ..active
                },
                State::AwaitingAcknowledgement,
            ),
            (
                Facts {
                    push_muted: true,
                    ..active
                },
                State::PushMuted,
            ),
            (
                Facts {
                    destination: false,
                    ..active
                },
                State::Listening,
            ),
        ] {
            assert_eq!(facts.state(), expected);
            assert!(!facts.state().microphone_enabled());
            assert!(!facts.state().speaking());
        }
    }

    #[test]
    fn reconnect_and_codec_negotiation_override_old_channel_and_activity() {
        let active = Facts {
            sending: true,
            ..live()
        };
        for (connection, expected) in [
            (Connection::Offline, State::Offline),
            (Connection::Connecting, State::Connecting),
            (Connection::Reconnecting, State::Reconnecting),
        ] {
            assert_eq!(
                Facts {
                    connection,
                    ..active
                }
                .state(),
                expected
            );
        }
        for (availability, expected) in [
            (Availability::Negotiating, State::Negotiating),
            (Availability::Unsupported, State::Unsupported),
            (Availability::EndToEnd, State::EndToEnd),
        ] {
            assert_eq!(
                Facts {
                    availability,
                    ..active
                }
                .state(),
                expected
            );
        }
    }

    #[test]
    fn secondary_deafened_context_remains_visible_and_never_enabled() {
        assert_eq!(
            Facts {
                destination: false,
                deafened: true,
                ..live()
            }
            .state(),
            State::Deafened
        );
        assert_eq!(
            Facts {
                destination: false,
                transmit_budget: false,
                ..live()
            }
            .state(),
            State::Listening
        );
    }
}
