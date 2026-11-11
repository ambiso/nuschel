// SPDX-License-Identifier: EUPL-1.2
//! Toggle animations that never jump: the value eases toward its target, and
//! a new target mid-way starts from wherever the value is, taking only as
//! long as the remaining distance (rapid clicking reverses smoothly).
use std::time::{Duration, Instant};

const FULL: Duration = Duration::from_millis(180);

pub struct Ease {
    from: f32,
    to: f32,
    started: Instant,
    duration: Duration,
}

impl Ease {
    pub fn new(on: bool) -> Self {
        let value = f32::from(u8::from(on));
        Self {
            from: value,
            to: value,
            started: Instant::now(),
            duration: Duration::ZERO,
        }
    }

    fn progress(&self) -> f32 {
        if self.duration.is_zero() {
            return 1.;
        }
        (self.started.elapsed().as_secs_f32() / self.duration.as_secs_f32()).min(1.)
    }

    /// From 0 (off) to 1 (on).
    pub fn value(&self) -> f32 {
        let t = self.progress();
        self.from + (self.to - self.from) * t * t * (3. - 2. * t)
    }

    pub fn set(&mut self, on: bool, reduce_motion: bool) {
        let to = f32::from(u8::from(on));
        if to == self.to {
            return;
        }
        let from = self.value();
        *self = Self {
            from,
            to,
            started: Instant::now(),
            duration: if reduce_motion {
                Duration::ZERO
            } else {
                FULL.mul_f32((to - from).abs())
            },
        };
    }

    pub fn moving(&self) -> bool {
        self.progress() < 1.
    }
}

#[cfg(test)]
mod tests {
    use super::Ease;

    #[test]
    fn reversing_mid_way_continues_from_the_current_value() {
        let mut ease = Ease::new(false);
        ease.set(true, false);
        std::thread::sleep(std::time::Duration::from_millis(60));
        let before = ease.value();
        ease.set(false, false);
        let after = ease.value();
        assert!(before > 0. && before < 1.);
        assert!((after - before).abs() < 0.05, "{before} → {after}");
        // The way back takes only as long as the distance covered.
        assert!(ease.duration < super::FULL);
        assert!(ease.moving());
    }

    #[test]
    fn reduced_motion_jumps_straight_to_the_target() {
        let mut ease = Ease::new(false);
        ease.set(true, true);
        assert_eq!(ease.value(), 1.);
        assert!(!ease.moving());
    }
}
