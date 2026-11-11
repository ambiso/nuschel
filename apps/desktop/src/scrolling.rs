// SPDX-License-Identifier: EUPL-1.2
//! Ease discrete wheel steps; leave precise touchpad motion to the platform.
//! Keep a timeline's messages still while history grows above them, and lay
//! out only the part of it near the viewport.
use gpui_kit::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, ScrollDelta, ScrollEvent, ScrollHandle, Styled, Window, canvas, px,
};
use std::{
    cell::{Cell, RefCell},
    ops::Range,
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

/// A timeline entry's height as last laid out; zero until it has been.
pub type Height = Rc<Cell<Pixels>>;

/// The entries of a timeline to lay out: those within a margin of the
/// viewport. The rest keep their room, by the heights they last had.
#[derive(Debug, PartialEq)]
pub struct Shown {
    pub range: Range<usize>,
    pub above: Pixels,
    pub below: Pixels,
}

/// Where the viewport is, for [`shown`].
pub enum Focus {
    /// Following the newest entries.
    Newest,
    /// Its top this far below the first entry.
    Offset(Pixels),
    /// About to bring this entry a third of the way down (see [`Anchor::reveal`]).
    Entry(usize),
}

/// Which entries, of `heights` (zero for those never laid out), lie near a
/// viewport `viewport` tall at `focus`. Laying out every message on every
/// frame would cost each frame (and each step of a window resize, which
/// wraps them all again) time in proportion to history.
pub fn shown(heights: &[Pixels], focus: Focus, viewport: Pixels) -> Shown {
    // Those never laid out count as tall as the average of those that were.
    let known = heights.iter().filter(|height| **height > px(0.));
    let (sum, count) = known.fold((px(0.), 0.), |(sum, count), height| {
        (sum + *height, count + 1.)
    });
    let estimate = if count > 0. { sum / count } else { px(48.) };
    let heights: Vec<Pixels> = heights
        .iter()
        .map(|height| if *height > px(0.) { *height } else { estimate })
        .collect();
    // A screen either side: corrections by the anchor stay within what is laid out.
    let margin = viewport.max(px(400.));
    let total: Pixels = heights.iter().sum();
    let top = match focus {
        Focus::Newest => total - viewport,
        Focus::Offset(top) => top,
        Focus::Entry(index) => heights.iter().take(index).sum::<Pixels>() - viewport / 3.,
    };
    let (from, to) = (top - margin, top + viewport + margin);
    let last = heights.len().saturating_sub(1);
    let (mut start, mut above, mut y) = (None, total, px(0.));
    let mut end = heights.len();
    for (index, height) in heights.iter().enumerate() {
        if start.is_none() && (y + *height > from || index == last) {
            start = Some(index);
            above = y;
        }
        if start.is_some() && y >= to {
            end = index;
            break;
        }
        y += *height;
    }
    let start = start.unwrap_or(heights.len());
    let laid_out: Pixels = heights[start..end].iter().sum();
    Shown {
        range: start..end,
        above,
        below: total - above - laid_out,
    }
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
    /// A message to bring into view, until a frame shows it there, and the
    /// offset this frame set for it.
    reveal: Cell<Option<i64>>,
    revealed: Cell<Option<Pixels>>,
    /// The height of what precedes the entries: the conversation's welcome.
    intro: Height,
}

impl Anchor {
    /// Forget the last frame, for another conversation.
    pub fn reset(&self) {
        self.0.message.set(None);
        self.0.painted.set(None);
        self.0.shifted.set(px(0.));
        self.0.reveal.set(None);
        self.0.intro.set(px(0.));
    }

    /// Scroll message `id` a third of the way down the viewport once it is
    /// laid out (a search result opening it).
    pub fn reveal(&self, id: i64) {
        self.0.reveal.set(Some(id));
    }

    /// The message waiting to be brought into view, which has to be laid out.
    pub fn revealing(&self) -> Option<i64> {
        self.0.reveal.get()
    }

    pub fn intro(&self) -> &Height {
        &self.0.intro
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
    /// A timeline entry, in order, whose laid-out height goes into `height`;
    /// with the `id` of a stored message it can serve as the anchor.
    pub fn row(&self, id: Option<i64>, height: &Height, child: AnyElement) -> AnyElement {
        Row {
            id,
            height: height.clone(),
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
                if let Some(id) = state.reveal.get()
                    && let Some(&(_, layout)) = rows.iter().find(|(row, _)| *row == id)
                {
                    offset.y = (handle.bounds().size.height / 3. - top(window, layout)).min(px(0.));
                    handle.set_offset(offset);
                    state.revealed.set(Some(offset.y));
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
                // Done once the timeline scrolled there (as far as it can):
                // a pending scroll to the bottom may have won this frame.
                if let Some(wanted) = state.revealed.take() {
                    if offset == wanted.max(-handle.max_offset().y) {
                        state.reveal.set(None);
                    } else {
                        window.request_animation_frame();
                    }
                }
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

/// Lays its child out unchanged and notes where, for [`AnchorFrame::keeper`],
/// and how tall, for [`shown`].
struct Row {
    id: Option<i64>,
    height: Height,
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
        if let Some(id) = self.id {
            self.rows.borrow_mut().push((id, layout));
        }
        (layout, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.height.set(bounds.size.height);
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
    use super::{Focus, Shown, follows, near_top, shown};
    use gpui_kit::px;

    #[test]
    fn only_entries_near_the_viewport_are_laid_out() {
        // A hundred entries 50 tall, in a viewport of 400 (so a margin of 400).
        let heights = vec![px(50.); 100];
        // Following: the last 800 of 5000.
        assert_eq!(
            shown(&heights, Focus::Newest, px(400.)),
            Shown {
                range: 84..100,
                above: px(4200.),
                below: px(0.)
            }
        );
        // Scrolled to 2000: from 1600 to 2800.
        assert_eq!(
            shown(&heights, Focus::Offset(px(2000.)), px(400.)),
            Shown {
                range: 32..56,
                above: px(1600.),
                below: px(2200.)
            }
        );
        // At the top, and with less than a screen, everything from the start.
        assert_eq!(
            shown(&heights, Focus::Offset(px(0.)), px(400.)).range,
            0..16
        );
        assert_eq!(shown(&heights[..5], Focus::Newest, px(400.)).range, 0..5);
        // A search result to reveal: it and a screen around it, wherever it is.
        let around = shown(&heights, Focus::Entry(50), px(300.));
        assert!(around.range.contains(&50) && around.range.len() < 30);
        // Past the end, still the last entry rather than nothing.
        assert_eq!(
            shown(&heights, Focus::Offset(px(9000.)), px(400.)).range,
            99..100
        );
        assert_eq!(shown(&[], Focus::Newest, px(400.)).range, 0..0);
        // Entries never laid out (an older page just loaded) count as the average.
        let mut prepended = vec![px(0.); 20];
        prepended.extend([px(100.), px(20.)]);
        let below = shown(&prepended, Focus::Offset(px(0.)), px(400.));
        assert_eq!((below.range, below.below), (0..14, px(480.)));
    }

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
