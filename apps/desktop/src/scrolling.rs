// SPDX-License-Identifier: EUPL-1.2
//! Ease discrete wheel steps; leave precise touchpad motion to the platform.
//! In a dialog with lists that scroll on their own, move one of them per
//! run of wheel steps. Keep a timeline's messages still while history grows
//! above them, lay out only the part of it near the viewport, and move the
//! keyboard through it.
use gpui_kit::{
    AccessibleAction, AnyElement, App, Bounds, Element, ElementId, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, Pixels, Role, ScrollDelta, ScrollEvent,
    ScrollHandle, SharedString, Style, Styled, Window, accesskit, canvas, px,
};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
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

/// How soon a wheel step must follow the previous one to belong to the same
/// run: a spin of the wheel, or a swipe on a touchpad with its momentum.
const RUN: Duration = Duration::from_millis(500);

/// Within this much of its end a container counts as there (easing ends a
/// fraction of a pixel off).
const EDGE: Pixels = px(0.5);

/// What a wheel step in a dialog moves: the dialog, or a list that scrolls
/// inside it (Settings' device lists and font picker), by its element id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scroller {
    Dialog,
    List(&'static str),
}

/// The container that a run of wheel steps moves, one per run, as browsers
/// latch a wheel transaction: the list under the pointer if the run's first
/// step can move it, the dialog otherwise. A step that moved both carried
/// the list away from a parked pointer and slid the next list under it, so
/// the entries passing under the pointer came twice and out of order.
#[derive(Default)]
pub struct WheelRun {
    last: Option<(Scroller, Instant)>,
}

impl WheelRun {
    /// The container a step at `now` moves; `list` is the list under the
    /// pointer, if any, and whether the step can move it.
    pub fn route(&mut self, list: Option<(&'static str, bool)>, now: Instant) -> Scroller {
        let running = self
            .last
            .filter(|(_, at)| now.saturating_duration_since(*at) < RUN)
            .map(|(scroller, _)| scroller);
        let scroller = match (running, list) {
            // Lists sliding under a parked pointer leave the dialog moving.
            (Some(Scroller::Dialog), _) => Scroller::Dialog,
            // At its end, a list keeps the rest of the run.
            (Some(Scroller::List(held)), Some((under, _))) if held == under => Scroller::List(held),
            (_, Some((under, true))) => Scroller::List(under),
            _ => Scroller::Dialog,
        };
        self.last = Some((scroller, now));
        scroller
    }
}

/// Whether a step of `delta` (positive towards the top) can move a
/// container scrolled to `offset` (zero or less) of at most `max`.
pub fn can_move(offset: Pixels, max: Pixels, delta: Pixels) -> bool {
    if delta > px(0.) {
        offset < -EDGE
    } else if delta < px(0.) {
        offset > EDGE - max
    } else {
        false
    }
}

/// A scroll event's motion in pixels for a container that scrolls
/// vertically (positive towards the top); a horizontal one moves it too, as
/// it moves GPUI's own.
pub fn vertical(event: &ScrollEvent, line_height: Pixels) -> Pixels {
    let delta = event.delta.pixel_delta(line_height);
    if delta.y == px(0.) { delta.x } else { delta.y }
}

/// Move `handle` by a scroll event as GPUI would, with wheel steps eased.
pub fn scroll_by(
    motion: &mut SmoothScroll,
    handle: &ScrollHandle,
    event: &ScrollEvent,
    line_height: Pixels,
    reduce_motion: bool,
) {
    if motion.wheel(handle, event, line_height, reduce_motion) {
        return;
    }
    motion.cancel();
    let mut offset = handle.offset();
    offset.y = (offset.y + vertical(event, line_height)).clamp(-handle.max_offset().y, px(0.));
    handle.set_offset(offset);
}

/// Lists that scroll inside the open dialog, which scrolls as well: their
/// positions while it stays open, which container the current run of wheel
/// steps moves, and the easing of the list it moves.
///
/// The dialog's content and each list's content take every wheel step
/// before a scroll container sees it, so GPUI's own scrolling, which moves
/// every container under the pointer, never runs inside a dialog.
#[derive(Default)]
pub struct Nested {
    lists: RefCell<HashMap<&'static str, ScrollHandle>>,
    versions: RefCell<HashMap<&'static str, u64>>,
    run: WheelRun,
    motion: SmoothScroll,
    /// The list `motion` eases.
    moving: Option<&'static str>,
}

impl Nested {
    /// Share a caller-owned virtual list's existing viewport. Registering each
    /// render replaces the same bounded key; it never creates a second scroller.
    pub fn register(&mut self, id: &'static str, version: u64, handle: ScrollHandle) {
        let previous = self.versions.borrow_mut().insert(id, version);
        if previous.is_some_and(|previous| previous != version)
            || (previous.is_none() && self.moving == Some(id))
        {
            self.cancel();
        }
        self.lists.borrow_mut().insert(id, handle);
    }

    /// Direct manipulation takes over before a thumb has moved its offset.
    pub fn cancel(&mut self) {
        self.cancel_motion();
        self.run = WheelRun::default();
    }

    pub fn cancel_motion(&mut self) {
        self.motion.cancel();
        self.moving = None;
    }

    /// The list's scroll position, the same from frame to frame.
    pub fn handle(&self, id: &'static str) -> ScrollHandle {
        self.lists.borrow_mut().entry(id).or_default().clone()
    }

    /// The list starts at the top when it shows next.
    pub fn forget(&self, id: &'static str) {
        self.lists.borrow_mut().remove(id);
        self.versions.borrow_mut().remove(id);
    }

    /// Every list starts at the top (another dialog opened).
    pub fn forget_all(&self) {
        self.lists.borrow_mut().clear();
        self.versions.borrow_mut().clear();
    }

    /// What a step moving `delta` moves, over the list `list` or elsewhere
    /// in the dialog.
    pub fn route(&mut self, list: Option<&'static str>, delta: Pixels, now: Instant) -> Scroller {
        let under = list.map(|id| {
            let handle = self.handle(id);
            (
                id,
                can_move(handle.offset().y, handle.max_offset().y, delta),
            )
        });
        self.run.route(under, now)
    }

    /// Move the list `id` by a step.
    pub fn scroll(
        &mut self,
        id: &'static str,
        event: &ScrollEvent,
        line_height: Pixels,
        reduce_motion: bool,
    ) {
        if self.moving != Some(id) {
            self.motion.cancel();
            self.moving = Some(id);
        }
        let handle = self.handle(id);
        scroll_by(&mut self.motion, &handle, event, line_height, reduce_motion);
    }

    pub fn active(&self) -> bool {
        self.motion.active()
    }

    /// The next frame of a list's easing; whether it moved.
    pub fn advance(&mut self) -> bool {
        let Some(id) = self.moving else {
            return false;
        };
        let handle = self.handle(id);
        self.motion.advance(&handle)
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

/// A timeline entry from frame to frame, laid out or not: where the keyboard
/// is, and what a screen reader asks to see.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// The conversation's welcome, or older history on its way.
    Intro,
    /// A message, by its serial.
    Message(u64),
    /// An activity line, by its serial.
    Activity(u64),
    /// A run of activity lines, by its first line's serial.
    Run(u64),
}

impl Key {
    /// The same whether the entry is laid out or stands in for itself off
    /// screen, so screen readers keep their place as it comes into view.
    pub fn element_id(self) -> ElementId {
        match self {
            Key::Intro => "entry-intro".into(),
            Key::Message(serial) => ("entry-message", serial).into(),
            Key::Activity(serial) => ("entry-activity", serial).into(),
            Key::Run(serial) => ("entry-run", serial).into(),
        }
    }
}

/// What a screen reader asks of a timeline entry: to scroll it into view,
/// or to move the keyboard to it.
pub type EntryAction = Rc<dyn Fn(Key, AccessibleAction, &mut Window, &mut App)>;

/// A timeline entry that is not laid out, as screen readers still find it: a
/// list item named `label`, with the element id the laid-out entry has, and
/// an empty box for its layout. Its bounds are a pixel `at` the viewport's
/// edge it lies beyond (in window coordinates; `scale` is the window's), not
/// where it would be: that would move every one of them on each frame of a
/// scroll or resize, and each changed node costs the platform's
/// accessibility bridge time and an event. (Without bounds, AT-SPI would
/// offer no way to scroll to or focus it.)
pub struct Unseen {
    pub key: Key,
    pub label: SharedString,
    pub at: Bounds<Pixels>,
    pub scale: f32,
    pub action: EntryAction,
}

impl IntoElement for Unseen {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for Unseen {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.key.element_id())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn a11y_role(&self) -> Option<Role> {
        Some(Role::ListItem)
    }

    fn write_a11y_info(&self, node: &mut accesskit::Node) {
        node.set_label(self.label.to_string());
        let scaled = |pixels: Pixels| f64::from(f32::from(pixels) * self.scale);
        node.set_bounds(accesskit::Rect {
            x0: scaled(self.at.left()),
            y0: scaled(self.at.top()),
            x1: scaled(self.at.right()),
            y1: scaled(self.at.bottom()),
        });
        node.add_action(AccessibleAction::ScrollIntoView);
        node.add_action(AccessibleAction::Focus);
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (window.request_layout(Style::default(), None, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut Window,
        _: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        _: &mut App,
    ) {
        let Some(id) = id.filter(|_| window.is_a11y_active()) else {
            return;
        };
        for action in [AccessibleAction::ScrollIntoView, AccessibleAction::Focus] {
            let (key, handler) = (self.key, self.action.clone());
            window.on_a11y_action(id.accesskit_node_id(), action, move |_, window, cx| {
                handler(key, action, window, cx)
            });
        }
    }
}

/// Heights, with those never laid out counted as tall as the average of
/// those that were.
pub fn filled(heights: &[Pixels]) -> Vec<Pixels> {
    let known = heights.iter().filter(|height| **height > px(0.));
    let (sum, count) = known.fold((px(0.), 0.), |(sum, count), height| {
        (sum + *height, count + 1.)
    });
    let estimate = if count > 0. { sum / count } else { px(48.) };
    heights
        .iter()
        .map(|height| if *height > px(0.) { *height } else { estimate })
        .collect()
}

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
    let heights = filled(heights);
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

/// The entry at `top`, of entries `heights` tall: the first whose bottom is
/// below it.
pub fn at(heights: &[Pixels], top: Pixels) -> usize {
    let mut bottom = px(0.);
    for (index, height) in filled(heights).into_iter().enumerate() {
        bottom += height;
        if bottom > top {
            return index;
        }
    }
    heights.len().saturating_sub(1)
}

/// Where the keyboard goes from entry `from` on `key` (up, down, page up and
/// down, home, end), among entries `heights` tall; a page is most of a
/// viewport `viewport` tall. `None` for other keys.
pub fn step(heights: &[Pixels], from: usize, key: &str, viewport: Pixels) -> Option<usize> {
    let last = heights.len().checked_sub(1)?;
    let from = from.min(last);
    let heights = filled(heights);
    let page = |forward: bool| {
        let (mut to, mut moved) = (from, px(0.));
        while moved < viewport * 0.8 && if forward { to < last } else { to > 0 } {
            to = if forward { to + 1 } else { to - 1 };
            moved += heights[to];
        }
        to
    };
    Some(match key {
        "up" => from.saturating_sub(1),
        "down" => (from + 1).min(last),
        "pageup" => page(false),
        "pagedown" => page(true),
        "home" => 0,
        "end" => last,
        _ => return None,
    })
}

/// The offset at which content from `top` to `bottom` shows whole in a
/// viewport `height` tall, scrolling as little as possible from `offset`;
/// at its top if it is taller than the viewport.
pub fn nearest(offset: Pixels, top: Pixels, bottom: Pixels, height: Pixels) -> Pixels {
    if top + offset < px(0.) || bottom - top > height {
        -top
    } else if bottom + offset > height {
        height - bottom
    } else {
        offset
    }
    .min(px(0.))
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
    /// What to bring into view, until a frame shows it there, and the offset
    /// this frame set for it.
    reveal: Cell<Option<Reveal>>,
    revealed: Cell<Option<Pixels>>,
    /// The entry the keyboard moved to.
    cursor: Cell<Option<Key>>,
    /// The height of what precedes the entries: the conversation's welcome.
    intro: Height,
}

/// What [`Anchor`] brings into view.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Reveal {
    /// A stored message, a third of the way down (a search result opening it).
    Message(i64),
    /// An entry, scrolling no further than needed to show it whole.
    Entry(Key),
}

impl Anchor {
    /// Forget the last frame, for another conversation.
    pub fn reset(&self) {
        self.0.message.set(None);
        self.0.painted.set(None);
        self.0.shifted.set(px(0.));
        self.0.reveal.set(None);
        self.0.cursor.set(None);
        self.0.intro.set(px(0.));
    }

    /// Scroll message `id` a third of the way down the viewport once it is
    /// laid out (a search result opening it).
    pub fn reveal(&self, id: i64) {
        self.0.reveal.set(Some(Reveal::Message(id)));
    }

    /// Scroll entry `key` just into view once it is laid out (the keyboard
    /// or a screen reader going there).
    pub fn show(&self, key: Key) {
        self.0.reveal.set(Some(Reveal::Entry(key)));
    }

    /// What waits to be brought into view, which has to be laid out.
    pub fn revealing(&self) -> Option<Reveal> {
        self.0.reveal.get()
    }

    /// The entry the keyboard moved to, if it did.
    pub fn cursor(&self) -> Option<Key> {
        self.0.cursor.get()
    }

    pub fn set_cursor(&self, key: Key) {
        self.0.cursor.set(Some(key));
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

/// A laid-out entry: its key, the id of a stored message (which can serve as
/// the anchor) and its layout.
type Rows = Rc<RefCell<Vec<(Key, Option<i64>, LayoutId)>>>;

pub struct AnchorFrame {
    state: Rc<AnchorState>,
    rows: Rows,
}

impl AnchorFrame {
    /// A timeline entry, in order, whose laid-out height goes into `height`;
    /// with the `id` of a stored message it can serve as the anchor.
    pub fn row(&self, key: Key, id: Option<i64>, height: &Height, child: AnyElement) -> AnyElement {
        Row {
            key,
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
                    && let Some(&(_, _, layout)) = rows.iter().find(|row| row.1 == Some(id))
                {
                    let moved = top(window, layout) - before;
                    if moved != px(0.) {
                        tracing::debug!(moved = f32::from(moved), "timeline anchored");
                        offset.y -= moved;
                        handle.set_offset(offset);
                        state.shifted.set(state.shifted.get() - moved);
                    }
                }
                if let Some(reveal) = state.reveal.get()
                    && let Some(&(_, _, layout)) = rows.iter().find(|(key, id, _)| match reveal {
                        Reveal::Message(wanted) => *id == Some(wanted),
                        Reveal::Entry(wanted) => *key == wanted,
                    })
                {
                    let height = handle.bounds().size.height;
                    let start = top(window, layout);
                    let end = start + window.layout_bounds(layout).size.height;
                    offset.y = match reveal {
                        Reveal::Message(_) => (height / 3. - start).min(px(0.)),
                        Reveal::Entry(_) => nearest(offset.y, start, end, height),
                    };
                    handle.set_offset(offset);
                    state.revealed.set(Some(offset.y));
                }
                let first = rows.partition_point(|&(_, _, layout)| {
                    window.layout_bounds(layout).bottom() - bounds.top() + offset.y <= px(0.)
                });
                state.message.set(
                    rows[first..]
                        .iter()
                        .find_map(|&(_, id, layout)| Some((id?, top(window, layout)))),
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
    key: Key,
    id: Option<i64>,
    height: Height,
    rows: Rows,
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
        self.rows.borrow_mut().push((self.key, self.id, layout));
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
    use super::{
        Focus, Motion, Nested, RUN, Scroller, Shown, WheelRun, at, can_move, follows, near_top,
        nearest, shown, step,
    };
    use gpui_kit::{ScrollHandle, point, px};
    use std::time::{Duration, Instant};

    fn easing_list(nested: &mut Nested, id: &'static str) {
        nested.moving = Some(id);
        nested.motion.motion = Some(Motion {
            from: px(0.),
            to: px(-80.),
            last: px(0.),
            started: Instant::now(),
        });
    }

    #[test]
    fn a_thumb_grab_cancels_before_its_first_offset_change_and_releases_the_wheel_run() {
        let mut nested = Nested::default();
        nested.register("rules", 1, ScrollHandle::new());
        nested.run.route(Some(("rules", true)), Instant::now());
        easing_list(&mut nested, "rules");
        // A press has not moved the scroll offset yet. Waiting for an offset
        // mismatch in advance() would leave the previous wheel animation live.
        nested.cancel();
        assert!(!nested.advance());
        assert!(!nested.active());
        assert_eq!(
            nested.route(None, px(-1.), Instant::now()),
            Scroller::Dialog
        );
    }

    #[test]
    fn replacing_a_virtual_list_generation_cannot_apply_old_easing_to_its_new_handle() {
        let mut nested = Nested::default();
        let old = ScrollHandle::new();
        nested.register("rules", 1, old.clone());
        easing_list(&mut nested, "rules");
        let replacement = ScrollHandle::new();
        nested.register("rules", 2, replacement.clone());
        assert!(!nested.advance());
        assert_eq!(replacement.offset().y, px(0.));
        old.set_offset(point(px(0.), px(-40.)));
        assert_eq!(nested.handle("rules").offset().y, px(0.));
        nested.handle("rules").set_offset(point(px(0.), px(-10.)));
        assert_eq!(replacement.offset().y, px(-10.));
        assert_eq!(old.offset().y, px(-40.));
    }

    #[test]
    fn registering_the_same_virtual_viewport_preserves_a_running_wheel_animation() {
        let mut nested = Nested::default();
        let handle = ScrollHandle::new();
        nested.register("rules", 1, handle.clone());
        easing_list(&mut nested, "rules");
        nested.register("rules", 1, handle);
        assert!(nested.active());
        // Stopping motion while transferring to the dialog does not release
        // the edge latch: only an explicit grab or a new transaction does.
        nested.run.route(Some(("rules", true)), Instant::now());
        nested.cancel_motion();
        assert_eq!(
            nested.route(Some("rules"), px(-1.), Instant::now()),
            Scroller::List("rules")
        );
    }

    #[test]
    fn a_run_of_wheel_steps_moves_one_container() {
        // Settings, with the pointer parked on the microphones: the report
        // was that a spin there showed some entries twice or out of order.
        let (microphones, speakers) = ("input-devices", "output-devices");
        let mut run = WheelRun::default();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        // A spin moves the list alone, and past its end it stays with the
        // list: the dialog does not carry the speakers under the pointer.
        assert_eq!(
            run.route(Some((microphones, true)), at(0)),
            Scroller::List(microphones)
        );
        assert_eq!(
            run.route(Some((microphones, true)), at(40)),
            Scroller::List(microphones)
        );
        assert_eq!(
            run.route(Some((microphones, false)), at(80)),
            Scroller::List(microphones)
        );
        assert_eq!(
            run.route(Some((microphones, false)), at(400)),
            Scroller::List(microphones)
        );
        // The next spin, at the list's end, moves the dialog; the speakers
        // sliding under the pointer leave it moving until the spin ends.
        let pause = 400 + RUN.as_millis() as u64;
        assert_eq!(
            run.route(Some((microphones, false)), at(pause)),
            Scroller::Dialog
        );
        assert_eq!(
            run.route(Some((speakers, true)), at(pause + 50)),
            Scroller::Dialog
        );
        assert_eq!(
            run.route(Some((speakers, true)), at(pause + 100)),
            Scroller::Dialog
        );
        // After a pause, a spin over the speakers moves them.
        let later = pause + 100 + RUN.as_millis() as u64;
        assert_eq!(
            run.route(Some((speakers, true)), at(later)),
            Scroller::List(speakers)
        );
        // Moving the pointer onto the dialog's text moves the dialog at once,
        // and onto the other list, that list.
        assert_eq!(run.route(None, at(later + 50)), Scroller::Dialog);
        assert_eq!(run.route(None, at(later + 5000)), Scroller::Dialog);
        assert_eq!(
            run.route(Some((microphones, true)), at(later + 9000)),
            Scroller::List(microphones)
        );
        assert_eq!(
            run.route(Some((speakers, true)), at(later + 9050)),
            Scroller::List(speakers)
        );
        // A list too short to scroll never takes the wheel.
        assert_eq!(
            run.route(Some(("short", false)), at(later + 20000)),
            Scroller::Dialog
        );
    }

    #[test]
    fn a_list_can_move_until_it_reaches_an_end() {
        let max = px(300.);
        let (up, down) = (px(60.), px(-60.));
        assert!(!can_move(px(0.), max, up));
        assert!(can_move(px(0.), max, down));
        assert!(can_move(px(-150.), max, up));
        assert!(can_move(px(-150.), max, down));
        assert!(can_move(-max, max, up));
        assert!(!can_move(-max, max, down));
        // Easing ends a fraction of a pixel off.
        assert!(!can_move(-max + px(0.0001), max, down));
        assert!(!can_move(px(-0.0001), max, up));
        // Nothing to scroll, or no motion.
        assert!(!can_move(px(0.), px(0.), up));
        assert!(!can_move(px(0.), px(0.), down));
        assert!(!can_move(px(-150.), max, px(0.)));
    }

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
    fn the_keyboard_moves_by_entries_and_pages() {
        // A hundred entries 50 tall, in a viewport of 400: a page is 320.
        let heights = vec![px(50.); 100];
        let go = |from, key| step(&heights, from, key, px(400.));
        assert_eq!((go(10, "up"), go(10, "down")), (Some(9), Some(11)));
        // Seven entries make a page; the ends stop it.
        assert_eq!((go(10, "pagedown"), go(17, "pageup")), (Some(17), Some(10)));
        assert_eq!((go(3, "pageup"), go(97, "pagedown")), (Some(0), Some(99)));
        assert_eq!((go(0, "up"), go(99, "down")), (Some(0), Some(99)));
        assert_eq!((go(50, "home"), go(50, "end")), (Some(0), Some(99)));
        assert_eq!(go(10, "left"), None);
        assert_eq!(step(&[], 0, "down", px(400.)), None);
        // One entry taller than a page is a page.
        let tall = [px(50.), px(900.), px(50.)];
        assert_eq!(step(&tall, 2, "pageup", px(400.)), Some(1));
        // Where the viewport's top is: entries never laid out count as the average.
        assert_eq!((at(&heights, px(0.)), at(&heights, px(125.))), (0, 2));
        assert_eq!(at(&[px(0.), px(0.), px(100.)], px(150.)), 1);
        assert_eq!(at(&heights, px(9000.)), 99);
    }

    #[test]
    fn revealing_an_entry_scrolls_no_further_than_needed() {
        // A viewport 400 tall, scrolled 1000 down.
        let offset = px(-1000.);
        // In view: stays.
        assert_eq!(nearest(offset, px(1100.), px(1200.), px(400.)), offset);
        // Above: its top at the top; below: its bottom at the bottom.
        assert_eq!(nearest(offset, px(900.), px(950.), px(400.)), px(-900.));
        assert_eq!(nearest(offset, px(1380.), px(1450.), px(400.)), px(-1050.));
        // Taller than the viewport: its top.
        assert_eq!(nearest(offset, px(1200.), px(1900.), px(400.)), px(-1200.));
        // Never past the top of the timeline.
        assert_eq!(nearest(px(-10.), px(-5.), px(20.), px(400.)), px(0.));
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
