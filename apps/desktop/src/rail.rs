// SPDX-License-Identifier: EUPL-1.2
//! Server rail interaction: a reversible ID-order preview, critically damped
//! slot movement, and one atomic catalog commit at drop. Connections never move.
use crate::{layout, state::Desktop, theme::*, voice::RAIL_DECORATION_OUTSET};
use gpui_kit::{prelude::*, *};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

pub const TILE: f32 = 48.;
const SETTLE: Duration = Duration::from_millis(220);
fn slot_height() -> f32 {
    TILE + 2. * (RAIL_DECORATION_OUTSET + 2.)
}

#[derive(Clone)]
pub struct DragItem {
    pub server: String,
}

pub struct Ghost {
    name: String,
    image: Option<Arc<RenderImage>>,
    background: Hsla,
    radius: f32,
}
impl Render for Ghost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        layout::decorated_slot(px(RAIL_DECORATION_OUTSET))
            .size(px(slot_height()))
            .child(
                div()
                    .size(px(TILE))
                    .rounded(px(self.radius))
                    .bg(self.background)
                    .shadow_lg()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(color(WHITE))
                    .font_weight(FontWeight::BOLD)
                    .map(|tile| match &self.image {
                        Some(image) => {
                            tile.child(img(image.clone()).size(px(TILE)).rounded(px(self.radius)))
                        }
                        None => tile.child(crate::theme::initials(&self.name)),
                    }),
            )
    }
}

struct Offset {
    from: f32,
    began: Instant,
}
impl Offset {
    fn value(&self) -> f32 {
        let t = self.began.elapsed().as_secs_f32() / SETTLE.as_secs_f32();
        if t >= 1. {
            0.
        } else {
            self.from * (1. + 8. * t) * (-8. * t).exp()
        }
    }
    fn moving(&self) -> bool {
        self.began.elapsed() < SETTLE
    }
}
struct Drag {
    source: String,
    baseline: Vec<String>,
    ids: Vec<String>,
    indices: Vec<usize>,
    pointer: Option<Point<Pixels>>,
}
#[derive(Default)]
pub struct Rail {
    drag: Option<Drag>,
    offsets: BTreeMap<String, Offset>,
    frame: Option<Instant>,
}
impl Rail {
    pub fn dragged(&self) -> Option<&str> {
        self.drag.as_ref().map(|drag| drag.source.as_str())
    }
    pub fn order<'a>(&'a self, canonical: &'a [usize]) -> &'a [usize] {
        self.drag
            .as_ref()
            .map_or(canonical, |drag| drag.indices.as_slice())
    }
    pub fn offset(&self, id: &str) -> f32 {
        self.offsets.get(id).map_or(0., Offset::value)
    }
    fn begin(&mut self, id: &str, ids: &[String], indices: &[usize]) {
        self.drag = Some(Drag {
            source: id.into(),
            baseline: ids.to_vec(),
            ids: ids.to_vec(),
            indices: indices.to_vec(),
            pointer: None,
        });
        self.frame = Some(Instant::now());
    }
    fn move_to(&mut self, target: usize, reduce: bool) -> bool {
        let Some(drag) = &mut self.drag else {
            return false;
        };
        let Some(from) = drag.ids.iter().position(|id| id == &drag.source) else {
            return false;
        };
        let target = target.min(drag.ids.len().saturating_sub(1));
        if from == target {
            return false;
        }
        let old = drag.ids.clone();
        let id = drag.ids.remove(from);
        drag.ids.insert(target, id);
        let index = drag.indices.remove(from);
        drag.indices.insert(target, index);
        self.retarget(&old, reduce);
        true
    }
    fn retarget(&mut self, old: &[String], reduce: bool) {
        let Some(drag) = &self.drag else { return };
        for (new, id) in drag.ids.iter().enumerate() {
            let Some(prior) = old.iter().position(|old| old == id) else {
                continue;
            };
            let value = self.offsets.get(id).map_or(0., Offset::value)
                + (prior as f32 - new as f32) * slot_height();
            if reduce || value.abs() < 0.05 {
                self.offsets.remove(id);
            } else {
                self.offsets.insert(
                    id.clone(),
                    Offset {
                        from: value,
                        began: Instant::now(),
                    },
                );
            }
        }
    }
    fn point(&mut self, point: Point<Pixels>, handle: &ScrollHandle, reduce: bool) -> bool {
        let Some(drag) = &mut self.drag else {
            return false;
        };
        drag.pointer = Some(point);
        let bounds = handle.bounds();
        if !bounds.contains(&point) {
            return false;
        }
        let local = f32::from(point.y - bounds.top() - handle.offset().y);
        self.move_to((local / slot_height()).max(0.).floor() as usize, reduce)
    }
    fn cancel(&mut self, canonical: &[String], reduce: bool) {
        if let Some(drag) = &mut self.drag {
            let old = drag.ids.clone();
            drag.ids = canonical.to_vec();
            self.retarget(&old, reduce);
        }
        self.drag = None;
        self.frame = None;
    }
    fn take(&mut self) -> Option<Vec<String>> {
        self.frame = None;
        self.drag.take().map(|drag| drag.ids)
    }
    fn advance(&mut self, handle: &ScrollHandle, reduce: bool) -> bool {
        self.offsets.retain(|_, offset| !reduce && offset.moving());
        let now = Instant::now();
        let dt = self.frame.replace(now).map_or(0., |prior| {
            now.duration_since(prior).as_secs_f32().min(0.05)
        });
        let mut scrolling = false;
        if let Some(point) = self.drag.as_ref().and_then(|drag| drag.pointer) {
            let bounds = handle.bounds();
            if bounds.contains(&point) {
                let edge = px(28.);
                let speed = if point.y < bounds.top() + edge {
                    280. * f32::from((bounds.top() + edge - point.y) / edge)
                } else if point.y > bounds.bottom() - edge {
                    -280. * f32::from((point.y - bounds.bottom() + edge) / edge)
                } else {
                    0.
                };
                let prior = handle.offset();
                let mut offset = prior;
                offset.y = (offset.y + px(speed * dt)).clamp(-handle.max_offset().y, px(0.));
                if offset != prior {
                    handle.set_offset(offset);
                    self.point(point, handle, reduce);
                    scrolling = true;
                }
            }
        }
        scrolling || !self.offsets.is_empty()
    }
}

impl Desktop {
    pub fn begin_server_drag(&mut self, item: &DragItem, cx: &mut Context<Self>) -> Entity<Ghost> {
        self.cancel_server_scroll();
        self.rail.begin(
            &item.server,
            self.client.server_order(),
            self.client.ordered_server_indices(),
        );
        let index = self.client.server_index(&item.server);
        let (name, image, background, radius) = index.map_or_else(
            || ("Server".into(), None, color(ACCENT).into(), 16.),
            |index| {
                let server = &self.client.servers[index];
                (
                    server.name.clone(),
                    self.ui
                        .get(&server.id)
                        .and_then(|ui| ui.icon.clone())
                        .filter(|_| self.settings.server_icons),
                    crate::theme::avatar(&server.name),
                    self.voice_motion.radius(&server.id, false),
                )
            },
        );
        cx.notify();
        cx.new(|_| Ghost {
            name,
            image,
            background,
            radius,
        })
    }
    pub fn move_server_drag(&mut self, event: &DragMoveEvent<DragItem>, cx: &mut Context<Self>) {
        if self.rail.dragged() != Some(event.drag(cx).server.as_str()) {
            return;
        }
        if self.rail.point(
            event.event.position,
            &self.server_scroll,
            cx.reduce_motion(),
        ) {
            cx.notify();
        }
        // A stationary edge gesture must continue scrolling; only the actual
        // changing offset schedules subsequent frames in animate_server_rail.
        cx.notify();
    }
    pub fn cancel_server_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rail
            .cancel(self.client.server_order(), cx.reduce_motion());
        cx.stop_active_drag(window);
        cx.notify();
    }
    pub fn drop_server_drag(
        &mut self,
        item: &DragItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.rail.dragged() != Some(item.server.as_str()) {
            return;
        }
        if let Some(ids) = self.rail.take() {
            self.commit_server_order(&ids, window, cx);
        }
    }
    pub fn move_saved_server(
        &mut self,
        id: &str,
        delta: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.rail.dragged().is_some() {
            self.cancel_server_drag(window, cx);
        }
        let mut order = self.client.server_order().to_vec();
        let Some(from) = order.iter().position(|server| server == id) else {
            return;
        };
        let to = from
            .saturating_add_signed(delta)
            .min(order.len().saturating_sub(1));
        if from == to {
            return;
        }
        let value = order.remove(from);
        order.insert(to, value);
        self.commit_server_order(&order, window, cx);
        if let Some(position) = self
            .client
            .server_order()
            .iter()
            .position(|server| server == id)
        {
            self.reveal_server(position + 1);
        }
    }
    fn commit_server_order(&mut self, ids: &[String], _: &mut Window, cx: &mut Context<Self>) {
        let prior = self.client.server_order().to_vec();
        match self.client.set_server_order(ids) {
            Ok(true) => {
                let bookmarks = self.client.ordered_bookmarks();
                if let Err(error) = self.write_servers(&bookmarks) {
                    let _ = self.client.set_server_order(&prior);
                    self.error = Some(format!("Server order was not saved: {error}"));
                }
            }
            Ok(false) => {}
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }
    pub fn animate_server_rail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let stale = self
            .rail
            .drag
            .as_ref()
            .is_some_and(|drag| drag.baseline != self.client.server_order());
        if self.rail.dragged().is_some() && (stale || !cx.has_active_drag()) {
            self.rail
                .cancel(self.client.server_order(), cx.reduce_motion());
            if stale {
                cx.stop_active_drag(window);
            }
        }
        self.voice_motion.dragged(self.rail.dragged());
        if self.rail.advance(&self.server_scroll, cx.reduce_motion()) {
            window.request_animation_frame();
        }
    }
}

pub fn drag_view(owner: &WeakEntity<Desktop>, item: &DragItem, cx: &mut App) -> Entity<Ghost> {
    owner
        .update(cx, |this, cx| this.begin_server_drag(item, cx))
        .unwrap_or_else(|_| {
            cx.new(|_| Ghost {
                name: "Server".into(),
                image: None,
                background: color(ACCENT).into(),
                radius: 16.,
            })
        })
}

#[cfg(test)]
mod tests {
    use super::{Rail, SETTLE, slot_height};
    use gpui_kit::ScrollHandle;
    use std::time::{Duration, Instant};
    fn ids() -> Vec<String> {
        ["a", "b", "c", "d"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }
    #[test]
    fn drag_is_a_reversible_preview_of_stable_ids_and_runtime_indices() {
        let canonical = ids();
        let runtime = [3, 0, 2, 1];
        let mut rail = Rail::default();
        rail.begin("a", &canonical, &runtime);
        assert!(rail.move_to(2, false));
        assert_eq!(rail.drag.as_ref().unwrap().ids, ["b", "c", "a", "d"]);
        assert_eq!(rail.order(&runtime), [0, 2, 3, 1]);
        assert_eq!(canonical, ids());
        rail.cancel(&canonical, false);
        assert_eq!(rail.order(&runtime), runtime);
        assert!(rail.dragged().is_none());
        assert!(rail.take().is_none());
    }
    #[test]
    fn a_drop_produces_one_candidate_and_reduced_motion_keeps_no_tweens() {
        let mut rail = Rail::default();
        rail.begin("b", &ids(), &[0, 1, 2, 3]);
        assert!(rail.move_to(3, true));
        assert!(rail.offsets.is_empty());
        assert_eq!(rail.take().unwrap(), ["a", "c", "d", "b"]);
        assert!(rail.take().is_none());
    }
    #[test]
    fn rapid_preview_reversals_continue_from_the_current_visual_position() {
        let mut rail = Rail::default();
        rail.begin("a", &ids(), &[0, 1, 2, 3]);
        rail.move_to(2, false);
        rail.offsets.get_mut("b").unwrap().began = Instant::now() - Duration::from_millis(70);
        let old = rail.offset("b") + 0. * slot_height();
        rail.move_to(0, false);
        let new = rail.offset("b") + 1. * slot_height();
        assert!((old - new).abs() < 0.5, "{old} -> {new}");
    }
    #[test]
    fn settled_motion_does_not_request_continuous_frames() {
        let mut rail = Rail::default();
        rail.begin("a", &ids(), &[0, 1, 2, 3]);
        rail.move_to(3, false);
        rail.take();
        for offset in rail.offsets.values_mut() {
            offset.began = Instant::now() - SETTLE;
        }
        assert!(!rail.advance(&ScrollHandle::new(), false));
        assert!(rail.offsets.is_empty());
    }
}
