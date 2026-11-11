// SPDX-License-Identifier: EUPL-1.2
//! Adaptive audio controller.
//!
//! Watches the live graph continuously and adjusts only objects Nuschl owns:
//! its own playback buffer and its own transmit gate. System devices, mixer
//! levels and other applications' streams are never touched; problems that only
//! the user or the system can fix (a clipping microphone, echo without
//! cancellation) become suggestions instead.
//!
//! Every decision uses hysteresis: a condition must persist for several
//! observation windows before the controller reacts, and must stay clear for
//! longer before it backs off, so short glitches do not make settings flap.
use crate::dsp::{FRAME, level_db};

/// One observation window.
pub const WINDOW: std::time::Duration = std::time::Duration::from_secs(2);
/// A sample this close to full scale counts as clipped (after downmixing).
const CLIP_LEVEL: f32 = 0.985;
/// A frame with at least this many clipped samples is a clipped frame.
const CLIPPED_SAMPLES_PER_FRAME: u32 = 3;
/// Clipped frames per window that mark the window as clipping.
const CLIPPED_FRAMES_PER_WINDOW: u32 = 4;
const CLIPPING_ENTER_WINDOWS: u32 = 2;
const CLIPPING_EXIT_WINDOWS: u32 = 5;
/// Far-end audio above this level counts as "the other side is talking".
const FAR_ACTIVE_DB: f32 = -50.;
/// Far-end activity is remembered for the acoustic path plus device buffering.
const FAR_HOLD_FRAMES: u32 = 30;
/// Near-end speech probability treated as the local user talking.
const NEAR_VOICE: f32 = 0.8;
const ECHO_ENTER_LIKELIHOOD: f32 = 0.5;
const ECHO_EXIT_LIKELIHOOD: f32 = 0.2;
const ECHO_ENTER_WINDOWS: u32 = 2;
const ECHO_EXIT_WINDOWS: u32 = 4;
/// Playback buffer steps, in 10 ms frames.
pub const PLAYBACK_MIN_FRAMES: usize = 4;
pub const PLAYBACK_MAX_FRAMES: usize = 12;
const PLAYBACK_STEP_FRAMES: usize = 2;
/// Underrunning device callbacks per window that raise the playback buffer.
const UNDERRUNS_PER_WINDOW: u32 = 2;
/// Clean windows before the playback buffer is lowered again (one minute).
const PLAYBACK_RELAX_WINDOWS: u32 = 30;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Health {
    /// The microphone signal reaches full scale before Nuschl sees it.
    pub clipping: bool,
    /// Residual echo is persistent; the transmit gate is guarding against it.
    pub echo_guard: bool,
    /// Share of the last window in which both sides talked at once.
    pub double_talk: f32,
    /// Device underruns in the last window.
    pub underruns: u32,
    /// Nuschl's own playback buffer, in milliseconds.
    pub playback_ms: u32,
}

#[derive(Debug)]
pub struct Controller {
    // Per-window counters.
    frames: u32,
    clipped_frames: u32,
    far_frames: u32,
    double_talk_frames: u32,
    far_hold: u32,
    far_seen: bool,
    // Hysteresis state.
    clipping: bool,
    clipping_run: u32,
    echo_guard: bool,
    echo_run: u32,
    playback_frames: usize,
    playback_clean: u32,
    last: Health,
}

impl Default for Controller {
    fn default() -> Self {
        Self {
            frames: 0,
            clipped_frames: 0,
            far_frames: 0,
            double_talk_frames: 0,
            far_hold: 0,
            far_seen: false,
            clipping: false,
            clipping_run: 0,
            echo_guard: false,
            echo_run: 0,
            playback_frames: PLAYBACK_MIN_FRAMES,
            playback_clean: 0,
            last: Health {
                playback_ms: PLAYBACK_MIN_FRAMES as u32 * 10,
                ..Health::default()
            },
        }
    }
}

impl Controller {
    /// Far-end audio as handed to the device, one 10 ms frame at a time.
    pub fn observe_render(&mut self, frame: &[f32]) {
        if level_db(frame) > FAR_ACTIVE_DB {
            self.far_hold = FAR_HOLD_FRAMES;
            self.far_seen = true;
        }
    }

    /// The raw microphone frame (before any processing) and the near-end
    /// speech probability the pipeline measured for it.
    pub fn observe_capture(&mut self, raw: &[f32; FRAME], voice: f32) {
        let clipped = raw
            .iter()
            .filter(|sample| sample.abs() >= CLIP_LEVEL)
            .count() as u32;
        if clipped >= CLIPPED_SAMPLES_PER_FRAME {
            self.clipped_frames += 1;
        }
        let far = self.far_active();
        if far {
            self.far_frames += 1;
            if voice >= NEAR_VOICE {
                self.double_talk_frames += 1;
            }
        }
        self.far_hold = self.far_hold.saturating_sub(1);
        self.frames += 1;
    }

    /// The other side was audible within the echo path's reach.
    pub fn far_active(&self) -> bool {
        self.far_hold > 0
    }

    /// Whether the transmit gate should demand clearer speech right now:
    /// residual echo is persistent and only the far end is talking.
    pub fn guard_gate(&self) -> bool {
        self.echo_guard && self.far_active()
    }

    pub fn playback_samples(&self) -> usize {
        self.playback_frames * FRAME
    }

    #[cfg(test)]
    pub fn health(&self) -> Health {
        self.last
    }

    /// Close an observation window. `residual_echo` is the canceller's
    /// residual-echo likelihood when Nuschl's canceller runs; `underruns` is
    /// the number of device callbacks that ran dry since the last window.
    /// Returns the new health when anything a user might care about changed.
    pub fn window(&mut self, residual_echo: Option<f32>, underruns: u32) -> Option<Health> {
        // Clipping: only with real capture frames in this window.
        if self.frames > 0 {
            let clipping_now = self.clipped_frames >= CLIPPED_FRAMES_PER_WINDOW;
            if clipping_now == self.clipping {
                self.clipping_run = 0;
            } else {
                self.clipping_run += 1;
                let needed = if clipping_now {
                    CLIPPING_ENTER_WINDOWS
                } else {
                    CLIPPING_EXIT_WINDOWS
                };
                if self.clipping_run >= needed {
                    self.clipping = clipping_now;
                    self.clipping_run = 0;
                }
            }
        }
        // Residual echo: judged only while the far end actually played audio,
        // since the canceller's estimate is meaningless in silence.
        match residual_echo {
            Some(likelihood) if self.far_seen => {
                let towards = if self.echo_guard {
                    likelihood > ECHO_EXIT_LIKELIHOOD
                } else {
                    likelihood > ECHO_ENTER_LIKELIHOOD
                };
                if towards == self.echo_guard {
                    self.echo_run = 0;
                } else {
                    self.echo_run += 1;
                    let needed = if towards {
                        ECHO_ENTER_WINDOWS
                    } else {
                        ECHO_EXIT_WINDOWS
                    };
                    if self.echo_run >= needed {
                        self.echo_guard = towards;
                        self.echo_run = 0;
                    }
                }
            }
            None => {
                self.echo_guard = false;
                self.echo_run = 0;
            }
            Some(_) => {}
        }
        // Underruns: raise Nuschl's own playback buffer quickly, lower it slowly.
        if underruns >= UNDERRUNS_PER_WINDOW {
            self.playback_frames =
                (self.playback_frames + PLAYBACK_STEP_FRAMES).min(PLAYBACK_MAX_FRAMES);
            self.playback_clean = 0;
        } else if underruns == 0 {
            self.playback_clean += 1;
            if self.playback_clean >= PLAYBACK_RELAX_WINDOWS {
                self.playback_frames = self
                    .playback_frames
                    .saturating_sub(PLAYBACK_STEP_FRAMES)
                    .max(PLAYBACK_MIN_FRAMES);
                self.playback_clean = 0;
            }
        }
        let health = Health {
            clipping: self.clipping,
            echo_guard: self.echo_guard,
            double_talk: if self.far_frames == 0 {
                0.
            } else {
                self.double_talk_frames as f32 / self.far_frames as f32
            },
            underruns,
            playback_ms: self.playback_frames as u32 * 10,
        };
        self.frames = 0;
        self.clipped_frames = 0;
        self.far_frames = 0;
        self.double_talk_frames = 0;
        self.far_seen = false;
        let changed = health.clipping != self.last.clipping
            || health.echo_guard != self.last.echo_guard
            || health.playback_ms != self.last.playback_ms;
        self.last = health;
        changed.then_some(health)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// One window of 10 ms capture frames.
    const WINDOW_FRAMES: u32 = 200;

    fn tone(amplitude: f32) -> [f32; FRAME] {
        std::array::from_fn(|index| amplitude * (index as f32 * 0.07).sin())
    }

    fn run_window(
        controller: &mut Controller,
        raw: &[f32; FRAME],
        far: Option<&[f32; FRAME]>,
        voice: f32,
    ) {
        for _ in 0..WINDOW_FRAMES {
            if let Some(far) = far {
                controller.observe_render(far);
            }
            controller.observe_capture(raw, voice);
        }
    }

    #[test]
    fn clipping_needs_two_windows_to_enter_and_five_to_clear() {
        let mut controller = Controller::default();
        let loud = tone(1.4).map(|sample| sample.clamp(-1., 1.));
        run_window(&mut controller, &loud, None, 0.9);
        assert_eq!(
            controller.window(None, 0),
            None,
            "one loud window is not enough"
        );
        run_window(&mut controller, &loud, None, 0.9);
        assert!(controller.window(None, 0).expect("changed").clipping);
        let normal = tone(0.3);
        for _ in 0..4 {
            run_window(&mut controller, &normal, None, 0.9);
            controller.window(None, 0);
            assert!(controller.health().clipping, "cleared too early");
        }
        run_window(&mut controller, &normal, None, 0.9);
        assert!(!controller.window(None, 0).expect("changed").clipping);
    }

    #[test]
    fn a_clipped_window_between_clean_ones_does_not_flap() {
        let mut controller = Controller::default();
        let loud = tone(1.4).map(|sample| sample.clamp(-1., 1.));
        for index in 0..10 {
            let raw = if index % 2 == 0 { loud } else { tone(0.3) };
            run_window(&mut controller, &raw, None, 0.9);
            assert_eq!(controller.window(None, 0), None);
        }
    }

    #[test]
    fn echo_guard_only_reacts_while_the_far_end_plays() {
        let mut controller = Controller::default();
        let quiet = tone(0.01);
        // High likelihood in silence (no far end) is ignored.
        for _ in 0..4 {
            run_window(&mut controller, &quiet, None, 0.1);
            controller.window(Some(0.9), 0);
        }
        assert!(!controller.health().echo_guard);
        let far = tone(0.3);
        run_window(&mut controller, &quiet, Some(&far), 0.1);
        controller.window(Some(0.9), 0);
        assert!(!controller.health().echo_guard);
        run_window(&mut controller, &quiet, Some(&far), 0.1);
        assert!(controller.window(Some(0.9), 0).expect("changed").echo_guard);
        assert!(controller.guard_gate(), "far end active: gate guarded");
        // Values between the thresholds keep the current state.
        for _ in 0..6 {
            run_window(&mut controller, &quiet, Some(&far), 0.1);
            controller.window(Some(0.35), 0);
        }
        assert!(controller.health().echo_guard);
        for _ in 0..4 {
            run_window(&mut controller, &quiet, Some(&far), 0.1);
            controller.window(Some(0.05), 0);
        }
        assert!(!controller.health().echo_guard);
        // Without Nuschl's canceller there is nothing to measure.
        controller.echo_guard = true;
        controller.window(None, 0);
        assert!(!controller.health().echo_guard);
    }

    #[test]
    fn far_end_activity_expires_after_the_echo_path() {
        let mut controller = Controller::default();
        controller.observe_render(&tone(0.3));
        let quiet = tone(0.01);
        for _ in 0..FAR_HOLD_FRAMES {
            assert!(controller.far_active());
            controller.observe_capture(&quiet, 0.);
        }
        assert!(!controller.far_active());
    }

    #[test]
    fn double_talk_share_counts_near_speech_during_far_speech() {
        let mut controller = Controller::default();
        let far = tone(0.3);
        let near = tone(0.2);
        for index in 0..WINDOW_FRAMES {
            controller.observe_render(&far);
            controller.observe_capture(&near, if index % 4 == 0 { 0.95 } else { 0.1 });
        }
        controller.window(Some(0.), 0);
        assert!((controller.health().double_talk - 0.25).abs() < 0.01);
    }

    #[test]
    fn playback_buffer_rises_on_underruns_and_relaxes_slowly() {
        let mut controller = Controller::default();
        assert_eq!(controller.playback_samples(), PLAYBACK_MIN_FRAMES * FRAME);
        assert_eq!(
            controller.window(None, 1),
            None,
            "a single underrun is tolerated"
        );
        let raised = controller.window(None, 3).expect("changed");
        assert_eq!(raised.playback_ms, 60);
        for _ in 0..10 {
            controller.window(None, 5);
        }
        assert_eq!(
            controller.health().playback_ms,
            PLAYBACK_MAX_FRAMES as u32 * 10
        );
        for _ in 0..PLAYBACK_RELAX_WINDOWS - 1 {
            controller.window(None, 0);
        }
        assert_eq!(controller.health().playback_ms, 120, "relaxed too early");
        controller.window(None, 0);
        assert_eq!(controller.health().playback_ms, 100);
        // A lone underrun neither raises nor resets the relaxation clock to zero growth.
        controller.window(None, 1);
        assert_eq!(controller.health().playback_ms, 100);
    }
}
