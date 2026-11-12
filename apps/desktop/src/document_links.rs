// SPDX-License-Identifier: EUPL-1.2
//! Links in nonselectable worker documents. Text is shaped by BlockText once;
//! interaction uses that same layout and never creates a timeline message.
use crate::{
    document::Document,
    selection::{self, TextBox},
    state::{Desktop, ServerView, Status},
    theme,
};
use gpui_kit::{prelude::*, *};
use nuschl_client::platform::RenderKey;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{Arc, Weak},
};

/// One shared context per document, without copies of its potentially large HTML.
pub(crate) struct Scope {
    desktop: WeakEntity<Desktop>,
    server: String,
    generation: u64,
    key: RenderKey,
    lease: Weak<()>,
}

impl Scope {
    pub(crate) fn new(
        server: &ServerView,
        key: RenderKey,
        document: &Document,
        cx: &Context<Desktop>,
    ) -> Rc<Self> {
        Rc::new(Self {
            desktop: cx.weak_entity(),
            server: server.id.clone(),
            generation: server.generation,
            key,
            lease: document.lease(),
        })
    }

    fn current(&self, this: &Desktop) -> bool {
        if self.lease.upgrade().is_none() {
            return false;
        }
        let Some((server_index, server)) =
            this.client.servers.iter().enumerate().find(|(_, server)| {
                server.id == self.server && server.generation == self.generation
            })
        else {
            return false;
        };
        if server.status != Status::Connected {
            return false;
        }
        // A cache can outlive its rendered surface. Delayed actions must also
        // belong to the currently shown channel or the still-open profile.
        let surface = match self.key {
            RenderKey::Comment(session) => matches!(
                this.panel,
                Some(crate::panels::Panel::UserInfo { server, session: shown, .. })
                    if server == server_index && shown == session
            ),
            RenderKey::Welcome | RenderKey::Description(_) => {
                !this.dialog_open()
                    && this.selected_indices().is_some_and(|(selected, channel)| {
                        selected == server_index
                            && matches!(
                                server.channels[channel].target,
                                nuschl_session::connection::TextTarget::Channel(_)
                            )
                            && server.channels[channel].id
                                == match self.key {
                                    RenderKey::Description(id) => id,
                                    _ => 0,
                                }
                    })
            }
        };
        if !surface {
            return false;
        }
        let Some(ui) = this.ui.get(&self.server) else {
            return false;
        };
        let cached = match self.key {
            RenderKey::Welcome => ui
                .welcome
                .as_ref()
                .filter(|(source, _)| server.welcome.as_ref() == Some(source)),
            RenderKey::Description(id) => ui.descriptions.get(&id).filter(|(source, _)| {
                server
                    .channels
                    .iter()
                    .find(|channel| channel.id == id)
                    .is_some_and(|channel| channel.description_raw == *source)
            }),
            RenderKey::Comment(id) => ui.comments.get(&id).filter(|(source, _)| {
                server.users.get(&id).and_then(|user| user.comment.as_ref()) == Some(source)
            }),
        };
        cached.is_some_and(|(_, document)| Weak::ptr_eq(&self.lease, &document.lease()))
    }

    fn open(&self, url: &str, window: &mut Window, cx: &mut App) {
        let _ = self.desktop.update(cx, |this, cx| {
            if self.current(this) {
                this.open_link(url, window, cx);
            }
        });
    }

    fn focus(&self, handle: &FocusHandle, window: &mut Window, cx: &mut App) {
        let current = self
            .desktop
            .update(cx, |this, _| self.current(this))
            .unwrap_or(false);
        if current {
            window.focus(handle, cx);
        }
    }

    fn reveal(&self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        // The timeline has its own scroll anchoring rather than the modal's
        // autoscroll scope. A new keyboard focus may deliberately leave bottom.
        let _ = self.desktop.update(cx, |this, _| {
            if !self.current(this) {
                return;
            }
            if matches!(self.key, RenderKey::Comment(_)) {
                window.request_autoscroll(bounds);
                return;
            }
            let handle = &this.scroll;
            let clip = handle.bounds();
            let dy = if bounds.top() < clip.top() {
                bounds.top() - clip.top()
            } else if bounds.bottom() > clip.bottom() {
                bounds.bottom() - clip.bottom()
            } else {
                px(0.)
            };
            let before = handle.offset();
            let after = point(
                before.x,
                (before.y - dy).clamp(-handle.max_offset().y, px(0.)),
            );
            if after != before {
                this.client.focus.following = false;
                this.anchor.reset();
                handle.set_offset(after);
                window.request_animation_frame();
            }
        });
    }
}

#[derive(Default)]
struct Geometry {
    lines: Vec<Arc<WrappedLineLayout>>,
    line_height: Pixels,
    rectangles: Vec<Vec<Bounds<Pixels>>>,
}

/// No strings or glyphs are measured here. Reuse the shaper's wrapped row
/// boundaries and retain relative rectangles while its cached layouts match.
impl Geometry {
    fn update(&mut self, text: &TextBox) {
        let layout = text.layout.text_layout();
        let lines: Vec<_> = layout.line_layouts().into_iter().collect();
        let height = layout.line_height();
        if self.line_height == height
            && self.lines.len() == lines.len()
            && self
                .lines
                .iter()
                .zip(&lines)
                .all(|(a, b)| Arc::ptr_eq(a, b))
        {
            return;
        }
        let links = text.links.as_ref().expect("document links");
        let shown: Vec<_> = links
            .iter()
            .map(|(range, _)| text.layout.shown_range(range))
            .collect();
        self.rectangles = vec![Vec::new(); links.len()];
        let mut line_start = 0;
        let mut top = px(0.);
        let mut first_link = 0;
        for line in &lines {
            let mut row_start = 0;
            let ends = line
                .wrap_boundaries
                .iter()
                .map(|boundary| {
                    line.unwrapped_layout.runs[boundary.run_ix].glyphs[boundary.glyph_ix].index
                })
                .chain([line.len()]);
            for row_end in ends {
                let row_x = line.unwrapped_layout.x_for_index(row_start);
                while first_link < shown.len() && shown[first_link].end <= line_start + row_start {
                    first_link += 1;
                }
                for (index, range) in shown.iter().enumerate().skip(first_link) {
                    if range.start >= line_start + row_end {
                        break;
                    }
                    let start = range.start.max(line_start + row_start);
                    let end = range.end.min(line_start + row_end);
                    if start < end {
                        let left = line.unwrapped_layout.x_for_index(start - line_start) - row_x;
                        let right = line.unwrapped_layout.x_for_index(end - line_start) - row_x;
                        self.rectangles[index].push(Bounds::new(
                            point(left.min(right), top),
                            size((right - left).abs().max(px(1.)), height),
                        ));
                    }
                }
                row_start = row_end;
                top += height;
            }
            line_start += line.len() + 1;
        }
        self.lines = lines;
        self.line_height = height;
        tracing::debug!(
            links = links.len(),
            rows = self.rectangles.iter().map(Vec::len).sum::<usize>(),
            "document link geometry updated"
        );
    }
}

struct State {
    focus: Vec<FocusHandle>,
    labels: Vec<SharedString>,
    reveal: Rc<Cell<Option<usize>>>,
    hover: Rc<Cell<Option<usize>>>,
    pressed: Rc<Cell<Option<(usize, Point<Pixels>)>>>,
    geometry: Rc<RefCell<Geometry>>,
    _subscriptions: Vec<Subscription>,
}

pub(crate) struct Links {
    scope: Rc<Scope>,
    text: Rc<[TextBox]>,
    state: Option<Entity<State>>,
}

impl Links {
    pub(crate) fn new(scope: Rc<Scope>, text: TextBox) -> Self {
        Self {
            scope,
            text: vec![text].into(),
            state: None,
        }
    }

    pub(crate) fn retain(&mut self, window: &mut Window, cx: &mut App) {
        let count = self.text[0].links.as_ref().expect("document links").len();
        let key = ("document-links", self.scope.lease.as_ptr() as usize);
        self.state = Some(window.use_keyed_state(key, cx, |window, cx| {
            let focus: Vec<_> = (0..count)
                .map(|_| cx.focus_handle().tab_stop(true).tab_index(0))
                .collect();
            let subscriptions = focus
                .iter()
                .enumerate()
                .map(|(index, handle)| {
                    cx.on_focus(handle, window, move |state: &mut State, window, _| {
                        state.reveal.set(Some(index));
                        tracing::debug!(index, "document link received focus");
                        // Focus notifications happen after drawing; schedule once.
                        window.on_next_frame(|window, _| window.refresh());
                    })
                })
                .collect();
            State {
                focus,
                labels: self.text[0]
                    .links
                    .as_ref()
                    .expect("document links")
                    .iter()
                    .map(|(range, _)| self.text[0].text[range.clone()].into())
                    .collect(),
                reveal: Rc::default(),
                hover: Rc::default(),
                pressed: Rc::default(),
                geometry: Rc::default(),
                _subscriptions: subscriptions,
            }
        }));
    }

    pub(crate) fn prepaint(&mut self, window: &mut Window, cx: &mut App) -> Frame {
        let (focus, labels, reveal, hover, pressed, geometry) = {
            let state = self.state.as_ref().expect("retained links").read(cx);
            (
                state.focus.clone(),
                state.labels.clone(),
                state.reveal.clone(),
                state.hover.clone(),
                state.pressed.clone(),
                state.geometry.clone(),
            )
        };
        geometry.borrow_mut().update(&self.text[0]);
        let bounds = self.text[0].layout.bounds();
        let geometry = geometry.borrow();
        let links = self.text[0].links.as_ref().expect("document links");
        let mut elements = Vec::with_capacity(links.len());
        let mut rectangles = Vec::with_capacity(links.len());
        for (index, (_, url)) in links.iter().enumerate() {
            let fragments: Vec<_> = geometry.rectangles[index]
                .iter()
                .map(|rect| Bounds::new(rect.origin + bounds.origin, rect.size))
                .collect();
            let Some(first) = fragments.first().copied() else {
                rectangles.push(fragments);
                continue;
            };
            let union = fragments.iter().skip(1).fold(first, |a, b| {
                Bounds::from_corners(
                    a.origin.min(&b.origin),
                    a.bottom_right().max(&b.bottom_right()),
                )
            });
            let handle = focus[index].clone();
            let scope = self.scope.clone();
            let target = url.clone();
            // A one-pixel focus anchor outside the text layout cannot change
            // wrapping. Its accessible bounds and outline use real text rows.
            let control = div().size(px(1.)).track_focus(&handle).on_key_down(
                move |event: &KeyDownEvent, window, cx| {
                    if event.keystroke.key == "enter"
                        && !event.is_held
                        && !event.keystroke.modifiers.modified()
                    {
                        scope.open(&target, window, cx);
                        cx.stop_propagation();
                    } else if event.keystroke.key == "tab" {
                        let modifiers = event.keystroke.modifiers;
                        if !modifiers.control && !modifiers.alt && !modifiers.platform {
                            if modifiers.shift {
                                window.focus_prev(cx);
                            } else {
                                window.focus_next(cx);
                            }
                            // A focused timeline message may still have its own
                            // Tab navigation state. Document links use real focus.
                            cx.stop_propagation();
                        }
                    }
                },
            );
            let mut element = FocusLink {
                id: ("document-link", index).into(),
                control,
                label: labels[index].clone(),
                url: url.clone(),
                bounds: union,
                scale: window.scale_factor(),
                scope: self.scope.clone(),
                handle: handle.clone(),
            }
            .into_any_element();
            element.prepaint_as_root(
                first.origin,
                size(
                    AvailableSpace::Definite(px(1.)),
                    AvailableSpace::Definite(px(1.)),
                ),
                window,
                cx,
            );
            elements.push(element);
            if reveal.get() == Some(index) && handle.is_focused(window) {
                reveal.set(None);
                self.scope.reveal(union, window, cx);
            }
            rectangles.push(fragments);
        }
        Frame {
            scope: self.scope.clone(),
            text: self.text.clone(),
            focus,
            hover,
            pressed,
            rectangles,
            elements,
            hitbox: window.insert_hitbox(bounds, HitboxBehavior::Normal),
        }
    }
}

pub struct Frame {
    scope: Rc<Scope>,
    text: Rc<[TextBox]>,
    focus: Vec<FocusHandle>,
    hover: Rc<Cell<Option<usize>>>,
    pressed: Rc<Cell<Option<(usize, Point<Pixels>)>>>,
    rectangles: Vec<Vec<Bounds<Pixels>>>,
    elements: Vec<AnyElement>,
    hitbox: Hitbox,
}

impl Frame {
    pub(crate) fn paint(&mut self, window: &mut Window, cx: &mut App) {
        for element in &mut self.elements {
            element.paint(window, cx);
        }
        for (index, handle) in self.focus.iter().enumerate() {
            if handle.is_focused(window) {
                for rectangle in &self.rectangles[index] {
                    window.paint_quad(outline(
                        rectangle.dilate(px(1.)),
                        theme::color(theme::ACCENT),
                        BorderStyle::default(),
                    ));
                }
            }
        }
        if selection::link_at(
            &self.text,
            std::slice::from_ref(&self.hitbox),
            window.mouse_position(),
            window,
        )
        .is_some()
        {
            window.set_cursor_style(CursorStyle::PointingHand, &self.hitbox);
        }
        let (text, hitbox, pressed) =
            (self.text.clone(), self.hitbox.clone(), self.pressed.clone());
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, _| {
            if phase == DispatchPhase::Bubble && event.button == MouseButton::Left {
                pressed.set(
                    selection::link_at(
                        &text,
                        std::slice::from_ref(&hitbox),
                        event.position,
                        window,
                    )
                    .map(|(_, index)| (index, event.position)),
                );
            }
        });
        let (text, hitbox, pressed, scope, focus) = (
            self.text.clone(),
            self.hitbox.clone(),
            self.pressed.clone(),
            self.scope.clone(),
            self.focus.clone(),
        );
        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                return;
            }
            let Some((before, at)) = pressed.take() else {
                return;
            };
            let Some((_, index)) =
                selection::link_at(&text, std::slice::from_ref(&hitbox), event.position, window)
            else {
                return;
            };
            if index == before
                && (at.x - event.position.x).abs() <= px(4.)
                && (at.y - event.position.y).abs() <= px(4.)
            {
                scope.focus(&focus[index], window, cx);
                scope.open(
                    &text[0].links.as_ref().expect("document links")[index].1,
                    window,
                    cx,
                );
                cx.stop_propagation();
            }
        });
        let (text, hitbox, hover) = (self.text.clone(), self.hitbox.clone(), self.hover.clone());
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let current =
                selection::link_at(&text, std::slice::from_ref(&hitbox), event.position, window)
                    .map(|(_, index)| index);
            if hover.replace(current) != current {
                window.refresh();
            }
            let _ = cx;
        });
    }
}

struct FocusLink {
    id: ElementId,
    control: Div,
    label: SharedString,
    url: SharedString,
    bounds: Bounds<Pixels>,
    scale: f32,
    scope: Rc<Scope>,
    handle: FocusHandle,
}
impl IntoElement for FocusLink {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl Element for FocusLink {
    type RequestLayoutState = <Div as Element>::RequestLayoutState;
    type PrepaintState = <Div as Element>::PrepaintState;
    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }
    fn a11y_role(&self) -> Option<Role> {
        Some(Role::Link)
    }
    fn write_a11y_info(&self, node: &mut accesskit::Node) {
        self.control.write_a11y_info(node);
        node.set_label(self.label.to_string());
        node.set_url(self.url.to_string());
        node.add_action(AccessibleAction::Click);
        node.add_action(AccessibleAction::Focus);
        let scaled = |value: Pixels| f64::from(f32::from(value) * self.scale);
        node.set_bounds(accesskit::Rect {
            x0: scaled(self.bounds.left()),
            y0: scaled(self.bounds.top()),
            x1: scaled(self.bounds.right()),
            y1: scaled(self.bounds.bottom()),
        });
    }
    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.control.request_layout(id, inspector, window, cx)
    }
    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.control
            .prepaint(id, inspector, bounds, request, window, cx)
    }
    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request: &mut Self::RequestLayoutState,
        state: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.control
            .paint(id, inspector, bounds, request, state, window, cx);
        if let Some(id) = id.filter(|_| window.is_a11y_active()) {
            let (scope, url) = (self.scope.clone(), self.url.clone());
            window.on_a11y_action(
                id.accesskit_node_id(),
                AccessibleAction::Click,
                move |_, window, cx| scope.open(&url, window, cx),
            );
            let (scope, handle) = (self.scope.clone(), self.handle.clone());
            window.on_a11y_action(
                id.accesskit_node_id(),
                AccessibleAction::Focus,
                move |_, window, cx| scope.focus(&handle, window, cx),
            );
        }
    }
}
