// SPDX-License-Identifier: EUPL-1.2
//! Typing indicators on Nuschl servers (`typing/1`, docs/extension.md §7).
//!
//! While the composer changes, Nuschl says so at most every three seconds
//! (the server forwards at most one a conversation in that time and drops the
//! rest). Nothing more is sent once the message is sent, the composer is
//! emptied or editing stops: the protocol has no "stopped typing", so others
//! drop the indicator when that person's message arrives, or six seconds
//! after the last one. Only conversations the server named and we may write
//! in get one; the server forwards it only to those who may read there.
//! Settings → Privacy turns sending off; others' indicators still show.
use crate::state::Desktop;
use gpui_kit::*;
use std::time::Duration;

impl Desktop {
    /// The composer changed: say we are typing, now and then (the client
    /// decides where and how often).
    pub fn composer_changed(&mut self, cx: &mut Context<Self>) {
        let empty = self.composer.read(cx).value().trim().is_empty();
        self.client.set_time(crate::state::monotonic_ms());
        self.client.composing(!empty && self.editing.is_none());
        self.sync_link_preview(cx);
    }

    /// Typing indicators and temporary notices fade even on an idle connection.
    pub fn expire_typing(&mut self, cx: &mut Context<Self>) {
        if self.typing_timer {
            return;
        }
        let any = |this: &Self| {
            this.client.servers.iter().any(|server| {
                server.has_temporary_notice()
                    || server
                        .channels
                        .iter()
                        .any(|channel| !channel.typing.is_empty())
            })
        };
        if !any(self) {
            return;
        }
        self.typing_timer = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let more = this.update(cx, |this, cx| {
                    let expired_notice = this.client.set_time(crate::state::monotonic_ms());
                    let (changed, more) = this.client.expire_typing();
                    if changed || expired_notice {
                        cx.notify();
                    }
                    let more = more
                        || this
                            .client
                            .servers
                            .iter()
                            .any(|server| server.has_temporary_notice());
                    this.typing_timer = more;
                    more
                });
                if !matches!(more, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }
}

/// A polite live region for screen readers around `child`: a status whose
/// changes (who is typing) they announce without interrupting. GPUI's
/// elements cannot set AccessKit's `live` property themselves.
pub struct LiveStatus {
    pub id: ElementId,
    pub label: Option<SharedString>,
    pub child: AnyElement,
}

impl IntoElement for LiveStatus {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for LiveStatus {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn a11y_role(&self) -> Option<Role> {
        Some(Role::Status)
    }

    fn write_a11y_info(&self, node: &mut accesskit::Node) {
        if let Some(label) = &self.label {
            node.set_label(label.to_string());
        }
        node.set_live(accesskit::Live::Polite);
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
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
