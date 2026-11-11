// SPDX-License-Identifier: EUPL-1.2
//! When sound effects play. The media worker synthesises and mixes them; the
//! native driver sends each to the connection's worker and keeps quiet while
//! deafened, except for the deafen sound itself (as stock Mumble does).
use nuschl_core::audio::Sound;

/// Your own mute or deafen changed from `before` to `after` (muted, deafened).
/// Deafening implies muting, so its sound replaces the mute sound.
pub fn self_state(before: (bool, bool), after: (bool, bool)) -> Option<Sound> {
    match (before, after) {
        ((_, false), (_, true)) => Some(Sound::Deafened),
        ((_, true), (_, false)) => Some(Sound::Undeafened),
        ((false, _), (true, _)) => Some(Sound::Muted),
        ((true, _), (false, _)) => Some(Sound::Unmuted),
        _ => None,
    }
}

/// Someone else moved from channel `from` to `to` (`None`: not connected),
/// seen from your voice channel.
pub fn movement(voice: Option<u32>, from: Option<u32>, to: Option<u32>) -> Option<Sound> {
    let voice = voice?;
    match (from == Some(voice), to == Some(voice)) {
        (false, true) => Some(Sound::UserJoined),
        (true, false) => Some(Sound::UserLeft),
        _ => None,
    }
}

/// An incoming message. The conversation in front of you (`shown`) stays
/// quiet unless the message mentions you.
pub fn message(shown: bool, direct: bool, mentioned: bool) -> Option<Sound> {
    if mentioned {
        Some(Sound::Mention)
    } else if shown {
        None
    } else if direct {
        Some(Sound::Mention)
    } else {
        Some(Sound::Message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deafening_sounds_instead_of_muting() {
        assert_eq!(
            self_state((false, false), (true, false)),
            Some(Sound::Muted)
        );
        assert_eq!(
            self_state((true, false), (false, false)),
            Some(Sound::Unmuted)
        );
        assert_eq!(
            self_state((false, false), (true, true)),
            Some(Sound::Deafened)
        );
        assert_eq!(
            self_state((true, true), (true, false)),
            Some(Sound::Undeafened)
        );
        assert_eq!(
            self_state((true, true), (false, false)),
            Some(Sound::Undeafened)
        );
        assert_eq!(self_state((true, false), (true, false)), None);
    }

    #[test]
    fn only_your_voice_channel_counts() {
        assert_eq!(movement(Some(3), Some(1), Some(3)), Some(Sound::UserJoined));
        assert_eq!(movement(Some(3), None, Some(3)), Some(Sound::UserJoined));
        assert_eq!(movement(Some(3), Some(3), Some(1)), Some(Sound::UserLeft));
        assert_eq!(movement(Some(3), Some(3), None), Some(Sound::UserLeft));
        assert_eq!(movement(Some(3), Some(1), Some(2)), None);
        assert_eq!(movement(Some(3), Some(3), Some(3)), None);
        assert_eq!(movement(None, Some(1), Some(3)), None);
    }

    #[test]
    fn the_conversation_in_front_of_you_is_quiet_unless_it_mentions_you() {
        assert_eq!(message(false, false, false), Some(Sound::Message));
        assert_eq!(message(true, false, false), None);
        assert_eq!(message(true, true, false), None);
        assert_eq!(message(false, true, false), Some(Sound::Mention));
        assert_eq!(message(true, false, true), Some(Sound::Mention));
    }
}
