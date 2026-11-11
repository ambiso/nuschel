// SPDX-License-Identifier: EUPL-1.2
//! Ease discrete wheel steps; leave precise touchpad motion to the platform.
//! Keep a timeline's messages still while history grows above them.
use gpui_kit::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, ScrollDelta, ScrollEvent, ScrollHandle, Styled, Window, canvas, px,
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

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

    /// Carry a running animation along when the content moved under it.
    pub fn shift(&mut self, delta: Pixels) {
        if let Some(motion) = &mut self.motion {
            motion.from += delta;
            motion.to += delta;
            motion.last += delta;
        }
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

/// Within this distance of the bottom, scrolling down follows new messages again.
const FOLLOW_SLACK: Pixels = px(32.);

/// Whether the timeline keeps following its newest message after the reader
/// scrolled from `painted` to `now`; `max` is how far it could scroll.
pub fn follows(following: bool, painted: Pixels, now: Pixels, max: Pixels) -> bool {
    if now > painted {
        false
    } else if now < painted {
        following || max + now <= FOLLOW_SLACK
    } else {
        following
    }
}

/// Whether less than a screen of loaded history is left above the viewport,
/// so the previous page should be on its way.
pub fn near_top(offset: Pixels, viewport: Pixels) -> bool {
    -offset < viewport.max(px(200.))
}

/// Scroll anchoring, as browsers do it: the first message in view keeps its
/// place on screen while pages are prepended, or rows above it change height.
/// Positions come from the finished layout before the timeline scrolls, so
/// the correction lands in the same frame and nothing visibly jumps.
#[derive(Clone, Default)]
pub struct Anchor(Rc<AnchorState>);

#[derive(Default)]
struct AnchorState {
    /// The first message in view, and its distance below the viewport top in
    /// the last frame's layout (the scroll offset left out).
    message: Cell<Option<(i64, Pixels)>>,
    /// The offset the last frame was painted with, and how far it could
    /// scroll: any other offset now is the reader's own scrolling.
    painted: Cell<Option<(Pixels, Pixels)>>,
    /// Corrections made since the smooth scroller last looked.
    shifted: Cell<Pixels>,
}

impl Anchor {
    /// Forget the last frame, for another conversation.
    pub fn reset(&self) {
        self.0.message.set(None);
        self.0.painted.set(None);
        self.0.shifted.set(px(0.));
    }

    pub fn painted(&self) -> Option<(Pixels, Pixels)> {
        self.0.painted.get()
    }

    pub fn take_shift(&self) -> Pixels {
        self.0.shifted.replace(px(0.))
    }

    /// Collects the rows of the frame being built.
    pub fn frame(&self) -> AnchorFrame {
        AnchorFrame {
            state: self.0.clone(),
            rows: Default::default(),
        }
    }
}

pub struct AnchorFrame {
    state: Rc<AnchorState>,
    rows: Rc<RefCell<Vec<(i64, LayoutId)>>>,
}

impl AnchorFrame {
    /// A stored message row, in timeline order, that can serve as the anchor.
    pub fn row(&self, id: i64, child: AnyElement) -> AnyElement {
        Row {
            id,
            rows: self.rows.clone(),
            child,
        }
        .into_any_element()
    }

    /// Goes at the viewport's top edge, before the scrolling element: it
    /// prepaints after layout but before the timeline applies its offset.
    /// `hold` keeps the anchor in place (otherwise the caller follows the
    /// bottom); `older` runs once less than a screen is left above.
    pub fn keeper(
        &self,
        handle: &ScrollHandle,
        hold: bool,
        older: Option<impl FnOnce(&mut App) + 'static>,
    ) -> impl IntoElement {
        let (state, rows, handle) = (self.state.clone(), self.rows.clone(), handle.clone());
        let painted = (self.state.clone(), handle.clone());
        canvas(
            move |bounds, window, _| {
                let rows = rows.borrow();
                let top =
                    |window: &mut Window, layout| window.layout_bounds(layout).top() - bounds.top();
                let mut offset = handle.offset();
                if hold
                    && let Some((id, before)) = state.message.get()
                    && let Some(&(_, layout)) = rows.iter().find(|(row, _)| *row == id)
                {
                    let moved = top(window, layout) - before;
                    if moved != px(0.) {
                        tracing::debug!(moved = f32::from(moved), "timeline anchored");
                        offset.y -= moved;
                        handle.set_offset(offset);
                        state.shifted.set(state.shifted.get() - moved);
                    }
                }
                let first = rows.partition_point(|&(_, layout)| {
                    window.layout_bounds(layout).bottom() - bounds.top() + offset.y <= px(0.)
                });
                state.message.set(
                    rows.get(first)
                        .map(|&(id, layout)| (id, top(window, layout))),
                );
            },
            move |_, _, window, _| {
                let (state, handle) = painted;
                let offset = handle.offset().y;
                state.painted.set(Some((offset, handle.max_offset().y)));
                if let Some(older) = older
                    && near_top(offset, handle.bounds().size.height)
                {
                    window.on_next_frame(move |_, cx| older(cx));
                }
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_0()
    }
}

/// Lays its child out unchanged and notes where, for [`AnchorFrame::keeper`].
struct Row {
    id: i64,
    rows: Rc<RefCell<Vec<(i64, LayoutId)>>>,
    child: AnyElement,
}

impl IntoElement for Row {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for Row {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let layout = self.child.request_layout(window, cx);
        self.rows.borrow_mut().push((self.id, layout));
        (layout, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::{follows, near_top};
    use gpui_kit::px;

    #[test]
    fn scrolling_up_stops_following_and_reaching_the_bottom_resumes_it() {
        let max = px(1000.);
        // Following at the bottom; nothing moved, or new content pinned it.
        assert!(follows(true, -max, -max, max));
        // A wheel step up, even a small one, lets the reader look around.
        assert!(!follows(true, -max, -max + px(5.), max));
        // Scrolling down without reaching the bottom keeps the position.
        assert!(!follows(false, px(-200.), px(-600.), max));
        // Close enough to the bottom: follow again.
        assert!(follows(false, px(-600.), -max + px(20.), max));
        // Held still, the state stays as it was.
        assert!(!follows(false, px(-600.), px(-600.), max));
    }

    #[test]
    fn older_history_loads_ahead_of_the_top() {
        let viewport = px(600.);
        assert!(near_top(px(0.), viewport));
        assert!(near_top(px(-599.), viewport));
        assert!(!near_top(px(-601.), viewport));
        // A tiny window still keeps some history ready above.
        assert!(near_top(px(-150.), px(50.)));
    }
}
