// SPDX-License-Identifier: EUPL-1.2
//! Ease discrete wheel steps; leave precise touchpad motion to the platform.
use gpui_kit::{Pixels, ScrollDelta, ScrollEvent, ScrollHandle, px};
use std::time::{Duration, Instant};

const DURATION: Duration = Duration::from_millis(150);

struct Motion {
    from: Pixels,
    to: Pixels,
    last: Pixels,
    started: Instant,
}

#[derive(Default)]
pub struct SmoothScroll {
    motion: Option<Motion>,
}

impl SmoothScroll {
    pub fn cancel(&mut self) {
        self.motion = None;
    }

    pub fn active(&self) -> bool {
        self.motion.is_some()
    }

    /// Returns whether this event replaces the container's default scrolling.
    pub fn wheel(
        &mut self,
        handle: &ScrollHandle,
        event: &ScrollEvent,
        line_height: Pixels,
        reduce_motion: bool,
    ) -> bool {
        let ScrollDelta::Lines(delta) = event.delta else {
            self.cancel();
            return false;
        };
        if delta.y == 0. || !delta.y.is_finite() {
            return false;
        }
        let current = handle.offset().y;
        let base = self
            .motion
            .as_ref()
            .filter(|motion| motion.last == current)
            .map_or(current, |motion| motion.to);
        let target = (base + line_height * delta.y).clamp(-handle.max_offset().y, px(0.));
        if reduce_motion {
            self.cancel();
            let mut offset = handle.offset();
            offset.y = target;
            handle.set_offset(offset);
        } else if target != current {
            self.motion = Some(Motion {
                from: current,
                to: target,
                last: current,
                started: Instant::now(),
            });
        } else {
            self.cancel();
        }
        true
    }

    pub fn advance(&mut self, handle: &ScrollHandle) -> bool {
        let Some(motion) = &mut self.motion else {
            return false;
        };
        // Scrollbar drags, history navigation and layout clamping take precedence.
        if handle.offset().y != motion.last {
            self.cancel();
            return false;
        }
        let progress = (motion.started.elapsed().as_secs_f32() / DURATION.as_secs_f32()).min(1.);
        let eased = 1. - (1. - progress).powi(3);
        let mut offset = handle.offset();
        offset.y =
            (motion.from + (motion.to - motion.from) * eased).clamp(-handle.max_offset().y, px(0.));
        motion.last = offset.y;
        handle.set_offset(offset);
        if progress >= 1. {
            self.cancel();
        }
        true
    }
}
