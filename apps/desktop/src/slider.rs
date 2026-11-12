// SPDX-License-Identifier: EUPL-1.2
//! Continuous sliders: the value scales and the keyboard and pointer rules.
//!
//! Volumes keep their stored meaning (linear amplitude, 0 = muted, 1 = as sent,
//! at most [`MAX_GAIN`]); only the slider's position follows a cubic taper,
//! the curve PulseAudio uses for its software volume, so equal slider travel
//! sounds roughly equally loud at every level. The taper is continuous down to
//! silence and exactly invertible for every stored value.

/// The largest volume a person or server can be set to (unchanged).
pub const MAX_GAIN: f32 = 4.;

/// Pointer drags settle on unity when they pass within this many decibels.
const UNITY_DETENT_DB: f32 = 0.5;

/// Keyboard step in slider travel; Page Up/Down move ten of these.
const FINE_STEP: f32 = 0.01;

/// How a slider maps its 0..=1 travel to the value it edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scale {
    /// A linear amplitude gain from 0 to [`MAX_GAIN`] on a cubic taper.
    Gain,
    /// A fraction 0..=1, linear (voice activity sensitivity).
    Fraction,
}

impl Scale {
    /// The largest value.
    pub fn max(self) -> f32 {
        match self {
            Scale::Gain => MAX_GAIN,
            Scale::Fraction => 1.,
        }
    }

    /// Where `value` sits on the slider, 0..=1.
    pub fn position(self, value: f32) -> f32 {
        let value = if value.is_finite() { value.clamp(0., self.max()) } else { 0. };
        match self {
            Scale::Gain => (value / MAX_GAIN).cbrt(),
            Scale::Fraction => value,
        }
    }

    /// The value at slider `position`; the ends are exact.
    pub fn value(self, position: f32) -> f32 {
        let position = if position.is_finite() { position.clamp(0., 1.) } else { 0. };
        if position >= 1. {
            return self.max();
        }
        match self {
            Scale::Gain => MAX_GAIN * position * position * position,
            Scale::Fraction => position,
        }
    }

    /// The value for a pointer at `position`: like [`Scale::value`], but a
    /// gain within half a decibel of unity settles exactly on it.
    pub fn pointer_value(self, position: f32) -> f32 {
        let value = self.value(position);
        if self == Scale::Gain && value > 0. && (20. * value.log10()).abs() < UNITY_DETENT_DB {
            1.
        } else {
            value
        }
    }

    /// `steps` keyboard steps from `value` (positive is louder or more): fine
    /// steps are 1 % of the travel. Unity is a stop, so stepping across it
    /// lands exactly on 1, and both ends are exact.
    pub fn step(self, value: f32, steps: i32) -> f32 {
        let from = self.position(value);
        let to = (from + steps as f32 * FINE_STEP).clamp(0., 1.);
        if self == Scale::Gain {
            let unity = self.position(1.);
            let crosses = (value < 1. && to >= unity) || (value > 1. && to <= unity);
            if crosses {
                return 1.;
            }
        }
        if to <= 0. {
            0.
        } else {
            self.value(to)
        }
    }

    /// What a screen reader and the label say for `value`.
    pub fn text(self, value: f32) -> String {
        match self {
            Scale::Gain if value <= 0. => "Muted".into(),
            Scale::Gain => format!("{:.0} % ({:+.1} dB)", value * 100., 20. * value.log10()),
            Scale::Fraction => format!("{:.0} %", value * 100.),
        }
    }
}

/// What a key does to a slider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// Arrows: one fine step; Page Up/Down: ten.
    Steps(i32),
    /// Home: the smallest value (muted for a volume).
    Min,
    /// End: the largest value.
    Max,
}

impl Key {
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "right" | "up" => Some(Key::Steps(1)),
            "left" | "down" => Some(Key::Steps(-1)),
            "pageup" => Some(Key::Steps(10)),
            "pagedown" => Some(Key::Steps(-10)),
            "home" => Some(Key::Min),
            "end" => Some(Key::Max),
            _ => None,
        }
    }

    /// The value after this key on `scale`.
    pub fn apply(self, scale: Scale, value: f32) -> f32 {
        match self {
            Key::Steps(steps) => scale.step(value, steps),
            Key::Min => 0.,
            Key::Max => scale.max(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gain_taper_is_exact_at_mute_unity_and_maximum() {
        assert_eq!(Scale::Gain.value(0.), 0.);
        assert_eq!(Scale::Gain.value(1.), MAX_GAIN);
        assert_eq!(Scale::Gain.position(0.), 0.);
        assert_eq!(Scale::Gain.position(MAX_GAIN), 1.);
        // Unity is reachable exactly by keyboard from either side and by pointer.
        let below = Scale::Gain.step(0.5, 0);
        let mut value = below;
        while value < 1. {
            value = Scale::Gain.step(value, 1);
        }
        assert_eq!(value, 1.);
        let mut value = 3.;
        while value > 1. {
            value = Scale::Gain.step(value, -1);
        }
        assert_eq!(value, 1.);
        assert_eq!(Scale::Gain.pointer_value(Scale::Gain.position(1.) + 0.001), 1.);
        assert_eq!(Scale::Gain.step(0.2, -1000), 0.);
        assert_eq!(Key::Max.apply(Scale::Gain, 0.2), MAX_GAIN);
        assert_eq!(Key::Min.apply(Scale::Gain, 3.), 0.);
    }

    #[test]
    fn every_stored_gain_maps_back_to_itself() {
        // Old menus stored 0.1 steps; anything from 0 to 4 must round-trip.
        let mut gain = 0.;
        while gain <= MAX_GAIN {
            let back = Scale::Gain.value(Scale::Gain.position(gain));
            assert!((back - gain).abs() <= gain * 1e-5 + 1e-7, "{gain} → {back}");
            gain += 0.003;
        }
        for gain in [1e-6_f32, 0.004, 0.01, 0.1, 0.25, 1., 2.5, 4.] {
            let back = Scale::Gain.value(Scale::Gain.position(gain));
            assert!((back - gain).abs() <= gain * 1e-5, "{gain} → {back}");
        }
    }

    #[test]
    fn the_taper_is_continuous_and_monotonic_down_to_silence() {
        let mut previous = Scale::Gain.value(0.);
        for step in 1..=10_000 {
            let value = Scale::Gain.value(step as f32 / 10_000.);
            assert!(value > previous, "not increasing at {step}");
            // No jump: neighbouring positions differ by well under 0.2 % of full scale.
            assert!(value - previous < MAX_GAIN * 0.002, "jump at {step}");
            previous = value;
        }
        // Low positions reach very quiet gains continuously (no −40 dB floor).
        assert!(Scale::Gain.value(0.05) < 0.001);
    }

    #[test]
    fn fine_steps_move_the_knob_evenly_and_coarse_ones_ten_times_further() {
        let fine = Scale::Gain.position(Scale::Gain.step(0.3, 1)) - Scale::Gain.position(0.3);
        let coarse = Scale::Gain.position(Scale::Gain.step(0.3, 10)) - Scale::Gain.position(0.3);
        assert!((fine - 0.01).abs() < 1e-5);
        assert!((coarse - 0.1).abs() < 1e-5);
        assert!((Scale::Fraction.step(0.5, 1) - 0.51).abs() < 1e-6);
        assert_eq!(Scale::Fraction.step(0.995, 1), 1.);
        assert_eq!(Scale::Fraction.step(0.005, -1), 0.);
    }

    #[test]
    fn readouts_say_mute_percent_and_decibels() {
        assert_eq!(Scale::Gain.text(0.), "Muted");
        assert_eq!(Scale::Gain.text(1.), "100 % (+0.0 dB)");
        assert_eq!(Scale::Gain.text(0.5), "50 % (-6.0 dB)");
        assert_eq!(Scale::Gain.text(4.), "400 % (+12.0 dB)");
        assert_eq!(Scale::Fraction.text(0.42), "42 %");
        for key in ["right", "up", "left", "down", "pageup", "pagedown", "home", "end"] {
            assert!(Key::from_key(key).is_some());
        }
        assert_eq!(Key::from_key("a"), None);
        assert_eq!(Key::from_key("pageup"), Some(Key::Steps(10)));
    }
}
