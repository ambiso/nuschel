// SPDX-License-Identifier: EUPL-1.2
//! Sender composer previews. Encoded images only cross the content-worker API.
use crate::{state::Desktop, theme::*};
use gpui_kit::{prelude::*, *};
use nuschl_client::preview::{Context as Destination, Mode, Status};
use nuschl_core::preview::EmbeddedImage;
use std::{sync::Arc, time::Duration};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageTicket {
    destination: Destination,
    connection: u64,
    policy: u64,
    revision: u64,
    image: EmbeddedImage,
}

#[derive(Default)]
pub struct ComposerPreview {
    pub policy: Option<nuschl_session::config::LinkPreviewSettings>,
    pub pasted: bool,
    timer: bool,
    pending: Option<ImageTicket>,
    settled: Option<ImageTicket>,
    pixels: Option<Arc<RenderImage>>,
}

impl ComposerPreview {
    pub fn new(policy: nuschl_session::config::LinkPreviewSettings) -> Self {
        Self {
            policy: Some(policy),
            ..Self::default()
        }
    }
}

impl Desktop {
    pub fn choose_link_preview_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        let Some(loaded) = &self.link_preview.policy else {
            return;
        };
        // A managed route is never copied into the user's sidecar.
        let mut settings = loaded.user.settings.clone();
        settings.mode = mode;
        match self.profile.save_link_previews(&settings) {
            Ok(loaded) => {
                self.client.set_link_preview_settings(
                    loaded.effective.settings.clone(),
                    crate::state::monotonic_ms(),
                );
                self.link_preview.policy = Some(loaded);
                self.sync_link_preview(cx);
            }
            Err(_) => {
                self.error = Some(
                    "Link preview settings could not be saved; the previous policy remains active."
                        .into(),
                )
            }
        }
        cx.notify();
    }
    fn preview_image_ticket(&self) -> Option<ImageTicket> {
        let Status::Ready(card) = self.client.link_preview_status() else {
            return None;
        };
        let (destination, connection, policy, revision) = self.client.link_preview_binding()?;
        Some(ImageTicket {
            destination: destination.clone(),
            connection,
            policy,
            revision,
            image: card.image()?.clone(),
        })
    }

    /// Called on composer/connection changes and bounded service/timer replies.
    pub fn sync_link_preview(&mut self, cx: &mut Context<Self>) {
        let now = crate::state::monotonic_ms();
        let text = self.composer.read(cx).value().to_string();
        let pasted = std::mem::take(&mut self.link_preview.pasted);
        self.client.composing_link_preview(
            &text,
            pasted,
            !self.attachments().is_empty(),
            self.editing.is_some(),
            now,
        );
        self.client.poll_link_preview(now);
        let image = self.preview_image_ticket();
        if self.link_preview.settled.as_ref() != image.as_ref() {
            self.link_preview.pixels = None;
            self.link_preview.settled = None;
        }
        if self.link_preview.pending.is_none()
            && let Some(image) = image
            && self.link_preview.settled.as_ref() != Some(&image)
        {
            self.link_preview.pending = Some(image.clone());
            self.services
                .render_link_preview(image.clone(), image.image.clone());
        }
        if !self.link_preview.timer
            && let Some(deadline) = self.client.link_preview_deadline()
        {
            self.link_preview.timer = true;
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(deadline.saturating_sub(now)))
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.link_preview.timer = false;
                    this.sync_link_preview(cx);
                    cx.notify();
                });
            })
            .detach();
        }
    }

    pub fn link_preview_pixels(
        &mut self,
        ticket: ImageTicket,
        pixels: Option<nuschl_ipc::preview::Pixels>,
        cx: &mut Context<Self>,
    ) {
        if self.link_preview.pending.as_ref() != Some(&ticket) {
            return;
        }
        self.link_preview.pending = None;
        if self.preview_image_ticket().as_ref() == Some(&ticket) {
            self.link_preview.pixels =
                pixels
                    .filter(|pixels| pixels.decode().is_some())
                    .and_then(|pixels| {
                        crate::document::render_image(pixels.width, pixels.height, &pixels.rgba)
                    });
            self.link_preview.settled = Some(ticket);
        }
        self.sync_link_preview(cx);
    }
}

pub fn composer(this: &Desktop, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    let status = this.client.link_preview_status();
    let (title, detail, remove, retry) = match status {
        Status::None | Status::Disabled => return None,
        Status::Fetching => (
            "Preparing link preview…".to_owned(),
            "Your message can be sent without waiting.".to_owned(),
            true,
            false,
        ),
        Status::Ready(card) => (
            card.display_title().to_owned(),
            format!("{} · {}", card.host(), card.description()),
            true,
            false,
        ),
        Status::Unavailable(_) => (
            "Link preview unavailable".to_owned(),
            "Your original text will be sent.".to_owned(),
            true,
            true,
        ),
        Status::Removed => (
            "Link preview removed".to_owned(),
            "Only your text will be sent.".to_owned(),
            false,
            true,
        ),
        Status::TextOnly(_) => (
            "Text only in this conversation".to_owned(),
            "Link previews are unavailable for this message type.".to_owned(),
            false,
            false,
        ),
    };
    let card = div()
        .id("composer-link-preview")
        .role(Role::Group)
        .aria_label("Link preview")
        .flex()
        .items_start()
        .gap_2()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(color(BORDER))
        .when_some(this.link_preview.pixels.clone(), |row, image| {
            row.child(
                img(image)
                    .w(px(96.))
                    .h(px(54.))
                    .object_fit(ObjectFit::Contain),
            )
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .truncate()
                        .child(text!(title).with_id("link-preview-title")),
                )
                .child(
                    div()
                        .text_xs()
                        .truncate()
                        .text_color(color(TEXT_MUTED))
                        .child(text!(detail).with_id("link-preview-detail")),
                ),
        )
        .when(retry, |row| {
            row.child(crate::dialogs::styled_button(
                "retry-link-preview",
                "Preview",
                crate::dialogs::Tone::Ghost,
                |this, _, cx| {
                    this.client.retry_link_preview(crate::state::monotonic_ms());
                    this.sync_link_preview(cx);
                    cx.notify();
                },
                cx,
            ))
        })
        .when(remove, |row| {
            row.child(crate::dialogs::icon_button(
                "remove-link-preview",
                "x",
                "Remove link preview",
                TEXT_MUTED,
                None,
                |this, _, cx| {
                    this.client
                        .remove_link_preview(crate::state::monotonic_ms());
                    this.sync_link_preview(cx);
                    cx.notify();
                },
                cx,
            ))
        });
    Some(card.into_any_element())
}

pub fn settings(this: &Desktop, cx: &mut Context<Desktop>) -> AnyElement {
    let mode = this
        .link_preview
        .policy
        .as_ref()
        .map_or(Mode::Off, |loaded| loaded.user.settings.mode);
    let problem = this
        .link_preview
        .policy
        .as_ref()
        .and_then(|loaded| loaded.effective.problem.as_ref())
        .map(ToString::to_string);
    let effective = match this.client.link_preview_settings().mode {
        Mode::Off => "Off",
        Mode::Direct => "Direct",
        Mode::ProxyRequired => "Proxy required",
    };
    let effective =
        format!("Active policy: {effective}. System configuration may restrict your saved choice.");
    div().flex().flex_col().gap_2()
        .child(crate::settings::segmented("link-preview-mode", "Link previews", [
            ("Off", Mode::Off), ("Direct", Mode::Direct), ("Proxy required", Mode::ProxyRequired),
        ].into_iter().map(|(label, chosen)| {
            (label, mode == chosen, std::rc::Rc::new(move |this: &mut Desktop, _: &mut Window, cx: &mut Context<Desktop>| {
                this.choose_link_preview_mode(chosen, cx);
            }) as crate::settings::Action)
        }).collect(), cx))
        .child(div().text_xs().text_color(color(TEXT_MUTED)).child(text!("Off makes no requests. Direct contacts the website. Proxy required stays disabled until an explicit proxy and proxied DNS are configured. Receivers never fetch previews.").with_id("link-preview-privacy")))
        .child(div().text_xs().text_color(color(TEXT_MUTED)).child(text!(effective).with_id("link-preview-effective-policy")))
        .when_some(problem, |settings, problem| settings.child(div().text_xs().text_color(color(TEXT_MUTED)).child(text!(problem).with_id("link-preview-policy-status"))))
        .into_any_element()
}
