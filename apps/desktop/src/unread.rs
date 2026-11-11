// SPDX-License-Identifier: EUPL-1.2
//! Unread conversations outside the visible part of either navigation list.
//! Positions come from layout, including people, direct conversations and
//! collapsed branches; no row-height or window-size assumptions are needed.
use crate::{state::Desktop, theme::*};
use gpui_kit::{prelude::*, *};
use nuschl_client::{model::TextTarget, server::ServerState, tree::TreeRow};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Servers,
    Channels,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub server: String,
    pub channel: u32,
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub slot: usize,
    pub target: Target,
    pub count: usize,
    pub mention: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Hint {
    slot: usize,
    target: Option<Target>,
    count: usize,
    mention: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Edges {
    above: Hint,
    below: Hint,
}

#[derive(Default)]
pub struct Navigation {
    servers: Rc<RefCell<Edges>>,
    channels: Rc<RefCell<Edges>>,
}

impl Navigation {
    pub fn reset_channels(&self) {
        *self.channels.borrow_mut() = Edges::default();
    }

    fn state(&self, lane: Lane) -> Rc<RefCell<Edges>> {
        match lane {
            Lane::Servers => self.servers.clone(),
            Lane::Channels => self.channels.clone(),
        }
    }
}

/// A hidden descendant's unread count belongs on its visible collapsed
/// ancestor. The target stays the actual conversation, so clicking can expand
/// its ancestors and open the right text history without joining voice.
pub fn channels(server: &ServerState) -> Vec<Candidate> {
    let rows = server.tree_rows();
    let mut slots: HashMap<u32, usize> = rows
        .iter()
        .enumerate()
        .filter_map(|(slot, row)| match row {
            TreeRow::Channel(row) => Some((server.channels[row.index].id, slot + 1)),
            _ => None,
        })
        .collect();
    let mut direct = rows.len() + 3; // heading, rows, spacer, direct heading.
    for channel in &server.channels {
        if !matches!(channel.target, TextTarget::Channel(_)) {
            slots.insert(channel.id, direct);
            direct += 1;
        }
    }
    let parents: HashMap<_, _> = server
        .channels
        .iter()
        .filter_map(|channel| {
            matches!(channel.target, TextTarget::Channel(_)).then_some((channel.id, channel.parent))
        })
        .collect();
    let mut owners = HashMap::<u32, Option<usize>>::new();
    let mut candidates = Vec::<Candidate>::new();
    let mut by_slot = HashMap::<usize, usize>::new();
    for channel in server.channels.iter().filter(|channel| channel.unread > 0) {
        let mut id = channel.id;
        let mut trail = HashSet::new();
        let slot = loop {
            if let Some(slot) = slots.get(&id) {
                break Some(*slot);
            }
            if let Some(slot) = owners.get(&id) {
                break *slot;
            }
            if !trail.insert(id) {
                break None;
            }
            let Some(Some(parent)) = parents.get(&id) else {
                break None;
            };
            id = *parent;
        };
        for id in trail {
            owners.insert(id, slot);
        }
        let Some(slot) = slot else {
            continue;
        };
        if let Some(index) = by_slot.get(&slot) {
            let candidate = &mut candidates[*index];
            candidate.count = candidate.count.saturating_add(channel.unread);
            candidate.mention |= channel.unread_mention;
        } else {
            by_slot.insert(slot, candidates.len());
            candidates.push(Candidate {
                slot,
                target: Target {
                    server: server.id.clone(),
                    channel: channel.id,
                },
                count: channel.unread,
                mention: channel.unread_mention,
            });
        }
    }
    candidates.sort_by_key(|candidate| candidate.slot);
    candidates
}

pub fn channel_slot(server: &ServerState, channel: u32) -> Option<usize> {
    let rows = server.tree_rows();
    if let Some(slot) = rows.iter().position(
        |row| matches!(row, TreeRow::Channel(row) if server.channels[row.index].id == channel),
    ) {
        return Some(slot + 1);
    }
    server
        .channels
        .iter()
        .filter(|entry| !matches!(entry.target, TextTarget::Channel(_)))
        .position(|entry| entry.id == channel)
        .map(|slot| rows.len() + 3 + slot)
}

fn edges(candidates: &[Candidate], handle: &ScrollHandle) -> Edges {
    let view = handle.bounds();
    if view.size.height <= px(0.) {
        return Edges::default();
    }
    let offset = handle.offset().y;
    let positions: Vec<_> = candidates
        .iter()
        .filter_map(|candidate| {
            let bounds = handle.bounds_for_item(candidate.slot)?;
            Some((
                candidate,
                f32::from(bounds.top() + offset),
                f32::from(bounds.bottom() + offset),
            ))
        })
        .collect();
    classify(&positions, f32::from(view.top()), f32::from(view.bottom()))
}

fn classify(positions: &[(&Candidate, f32, f32)], top: f32, bottom: f32) -> Edges {
    let mut edges = Edges::default();
    for &(candidate, row_top, row_bottom) in positions {
        let hint = if row_bottom <= top {
            &mut edges.above
        } else if row_top >= bottom {
            &mut edges.below
        } else {
            continue;
        };
        hint.count = hint.count.saturating_add(candidate.count);
        hint.mention |= candidate.mention;
        if hint.target.is_none()
            || (row_bottom <= top && candidate.slot > hint.slot)
            || (row_top >= bottom && candidate.slot < hint.slot)
        {
            hint.slot = candidate.slot;
            hint.target = Some(candidate.target.clone());
        }
    }
    edges
}

/// Place over a laid-out scrolling list. The canvas measures the new layout
/// after children prepaint, and requests another render only if the hints
/// changed (also handles the first layout and window/sidebar resizing).
pub fn overlay(
    this: &Desktop,
    lane: Lane,
    handle: &ScrollHandle,
    candidates: Vec<Candidate>,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let state = this.unread.state(lane);
    let visible = state.borrow().clone();
    let handle = handle.clone();
    let desktop = cx.entity().downgrade();
    div()
        .absolute()
        .inset_0()
        .child(
            canvas(
                move |_, _, cx| {
                    let next = edges(&candidates, &handle);
                    if *state.borrow() != next {
                        *state.borrow_mut() = next;
                        let desktop = desktop.clone();
                        cx.defer(move |cx| {
                            let _ = desktop.update(cx, |_, cx| cx.notify());
                        });
                    }
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .children(button(lane, true, visible.above, cx))
        .children(button(lane, false, visible.below, cx))
        .into_any_element()
}

fn button(lane: Lane, above: bool, hint: Hint, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    let target = hint.target?;
    let target_key = target.clone();
    let (list, direction, arrow) = (
        if lane == Lane::Servers {
            "servers"
        } else {
            "channels"
        },
        if above { "above" } else { "below" },
        if above { "↑" } else { "↓" },
    );
    let label = format!(
        "Unread {list} {direction}: {} messages{}",
        hint.count,
        if hint.mention {
            ", including mentions"
        } else {
            ""
        }
    );
    Some(
        div()
            .id(SharedString::from(format!("unread-{list}-{direction}")))
            .role(Role::Button)
            .tab_index(0)
            .aria_label(label)
            .absolute()
            .left_1()
            .right_1()
            .when(above, |button| button.top_1())
            .when(!above, |button| button.bottom_1())
            .h(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(color(if hint.mention { DANGER } else { ACCENT }))
            .text_color(color(WHITE))
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .cursor_pointer()
            .occlude()
            .hover(|style| style.opacity(0.9))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, window, cx| {
                this.jump_unread(lane, hint.slot, &target, window, cx);
                cx.stop_propagation();
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.jump_unread(lane, hint.slot, &target_key, window, cx);
                    cx.stop_propagation();
                }
            }))
            .child({
                let count = if hint.count > 99 {
                    "99+".to_owned()
                } else {
                    hint.count.to_string()
                };
                if lane == Lane::Servers {
                    format!("{arrow} {count}")
                } else {
                    format!("{arrow} {count} unread")
                }
            })
            .into_any_element(),
    )
}

impl Desktop {
    pub fn jump_unread(
        &mut self,
        lane: Lane,
        slot: usize,
        target: &Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self
            .client
            .servers
            .iter()
            .position(|server| server.id == target.server)
        else {
            return;
        };
        if lane == Lane::Servers {
            self.reveal_server(slot);
            self.browse_server(index, window, cx);
        }
        let server = &mut self.client.servers[index];
        let parents: HashMap<_, _> = server
            .channels
            .iter()
            .map(|channel| (channel.id, channel.parent))
            .collect();
        let mut parent = parents.get(&target.channel).copied().flatten();
        let mut seen = HashSet::new();
        while let Some(id) = parent.filter(|id| seen.insert(*id)) {
            server.expansion.insert(id, true);
            parent = parents.get(&id).copied().flatten();
        }
        if let Some(slot) = channel_slot(server, target.channel) {
            self.cancel_channel_scroll();
            self.channel_scroll.scroll_to_item(slot);
        }
        self.browse_channel(index, target.channel, window, cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{Candidate, Target, channel_slot, channels, classify};
    use nuschl_client::{conversation::ConversationState, server::ServerState};
    use nuschl_core::bookmark::ServerBookmark;

    #[test]
    fn unread_in_a_collapsed_branch_counts_on_its_visible_ancestor() {
        let bookmark = ServerBookmark::new("Test", "localhost", 64738, "andi").unwrap();
        let mut server = ServerState::new(bookmark);
        server.channels.extend([
            ConversationState::channel(1, Some(0), "Branch".into(), 0),
            ConversationState::channel(2, Some(1), "Hidden".into(), 0),
            ConversationState::channel(3, Some(1), "Hidden mention".into(), 1),
        ]);
        server.channels[2].unread = 3;
        server.channels[3].unread = 2;
        server.channels[3].unread_mention = true;
        let folded = channels(&server);
        assert_eq!(folded.len(), 1);
        assert_eq!(
            (folded[0].slot, folded[0].count, folded[0].mention),
            (2, 5, true)
        );
        assert_eq!(folded[0].target.channel, 2);
        assert_eq!(channel_slot(&server, 2), None);
        server.expansion.insert(1, true);
        let unfolded = channels(&server);
        assert_eq!(
            unfolded.iter().map(|item| item.count).collect::<Vec<_>>(),
            [3, 2]
        );
        assert_eq!(channel_slot(&server, 2), Some(unfolded[0].slot));
    }
    #[test]
    fn hints_count_unread_and_mentions_but_jump_to_the_nearest_item() {
        let item = |slot, count, mention| Candidate {
            slot,
            count,
            mention,
            target: Target {
                server: "test".into(),
                channel: slot as u32,
            },
        };
        let items = [
            item(0, 2, true),
            item(1, 3, false),
            item(2, 5, true),
            item(3, 7, false),
            item(4, 11, true),
        ];
        let bounds = [
            (&items[0], -50., -30.),
            (&items[1], -20., 0.),
            (&items[2], -5., 15.),
            (&items[3], 100., 120.),
            (&items[4], 140., 160.),
        ];
        let found = classify(&bounds, 0., 100.);
        assert_eq!(
            (found.above.slot, found.above.count, found.above.mention),
            (1, 5, true)
        );
        assert_eq!(
            (found.below.slot, found.below.count, found.below.mention),
            (3, 18, true)
        );
        // The partially visible item is discoverable already, so neither hint
        // counts it as off-screen.
        assert_eq!(found.above.count + found.below.count, 23);
    }
}
