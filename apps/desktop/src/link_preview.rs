// SPDX-License-Identifier: EUPL-1.2
//! Sender composer previews. Encoded images only cross the content-worker API.
use crate::{state::Desktop, theme::*};
use gpui_kit::{prelude::*, *};
use nuschl_client::preview::{Context as Destination, Mode, Settings, Status};
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
    writes: PolicyWrites,
}

#[derive(Clone)]
pub struct PolicyAction {
    pub revision: u64,
    pub settings: Settings,
}

/// One physical save, one replaceable latest intent. GUI cancellation never
/// releases the active save: only its completed background operation does.
#[derive(Default)]
struct PolicyWrites {
    revision: u64,
    active: Option<PolicyAction>,
    pending: Option<PolicyAction>,
    exhausted: bool,
}

impl PolicyWrites {
    fn choose(&mut self, settings: Settings) -> Result<Option<PolicyAction>, ()> {
        let Some(revision) = self.revision.checked_add(1).filter(|_| !self.exhausted) else {
            self.exhausted = true;
            self.pending = None;
            return Err(());
        };
        self.revision = revision;
        self.pending = Some(PolicyAction { revision, settings });
        Ok(self.start_latest())
    }

    fn start_latest(&mut self) -> Option<PolicyAction> {
        if self.active.is_some() {
            return None;
        }
        let next = self.pending.take()?;
        self.active = Some(next.clone());
        Some(next)
    }

    fn finished(&mut self, revision: u64) -> Option<(bool, Option<PolicyAction>)> {
        if self.active.as_ref().map(|active| active.revision) != Some(revision) {
            return None;
        }
        self.active = None;
        let current = !self.exhausted && revision == self.revision;
        Some((current, self.start_latest()))
    }

    fn desired(&self) -> Option<&Settings> {
        self.pending
            .as_ref()
            .or(self.active.as_ref())
            .map(|action| &action.settings)
    }
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
        // Revocation precedes disk work and even revision-exhaustion errors.
        self.client
            .set_link_preview_settings(Settings::default(), crate::state::monotonic_ms());
        self.link_preview.pixels = None;
        self.link_preview.settled = None;
        if !cfg!(target_os = "linux") {
            cx.notify();
            return;
        }
        let Some(loaded) = &self.link_preview.policy else {
            cx.notify();
            return;
        };
        // A managed route is never copied into the user's sidecar.
        let mut settings = loaded.user.settings.clone();
        settings.mode = mode;
        match self.link_preview.writes.choose(settings) {
            Ok(Some(action)) => self
                .services
                .save_link_preview_policy(self.profile.clone(), action),
            Ok(None) => {}
            Err(()) => {
                self.error = Some(
                    "Link preview settings cannot be changed in this session; previews remain off."
                        .into(),
                )
            }
        }
        cx.notify();
    }

    pub fn link_preview_policy_saved(
        &mut self,
        revision: u64,
        result: Result<nuschl_session::config::LinkPreviewSettings, String>,
        cx: &mut Context<Self>,
    ) {
        let Some((current, next)) = self.link_preview.writes.finished(revision) else {
            return;
        };
        if let Some(next) = next {
            self.services
                .save_link_preview_policy(self.profile.clone(), next);
        }
        if current {
            match result {
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
                        "Link preview settings could not be saved; previews remain off.".into(),
                    )
                }
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
        if !cfg!(target_os = "linux") {
            self.client
                .set_link_preview_settings(Settings::default(), now);
        }
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
    let saved_mode = this
        .link_preview
        .policy
        .as_ref()
        .map_or(Mode::Off, |loaded| loaded.user.settings.mode);
    let mode = this
        .link_preview
        .writes
        .desired()
        .map_or(saved_mode, |settings| settings.mode);
    let problem = this
        .link_preview
        .policy
        .as_ref()
        .and_then(|loaded| loaded.effective.problem.as_ref())
        .map(ToString::to_string);
    let active_mode = if cfg!(target_os = "linux") {
        this.client.link_preview_settings().mode
    } else {
        Mode::Off
    };
    let effective = match active_mode {
        Mode::Off => "Off",
        Mode::Direct => "Direct",
        Mode::ProxyRequired => "Proxy required",
    };
    let effective =
        format!("Active policy: {effective}. System configuration may restrict your saved choice.");
    div().flex().flex_col().gap_2()
        .when(cfg!(target_os = "linux"), |settings| settings.child(crate::settings::segmented("link-preview-mode", "Link previews", [
            ("Off", Mode::Off), ("Direct", Mode::Direct), ("Proxy required", Mode::ProxyRequired),
        ].into_iter().map(|(label, chosen)| {
            (label, mode == chosen, std::rc::Rc::new(move |this: &mut Desktop, _: &mut Window, cx: &mut Context<Desktop>| {
                this.choose_link_preview_mode(chosen, cx);
            }) as crate::settings::Action)
        }).collect(), cx)))
        .when(!cfg!(target_os = "linux"), |settings| settings.child(div().text_sm().child(text!("Link previews are not available on this platform yet.").with_id("link-preview-platform-unavailable"))))
        .child(div().text_xs().text_color(color(TEXT_MUTED)).child(text!("Off makes no requests. Direct contacts the website. Proxy required stays disabled until an explicit proxy and proxied DNS are configured. Receivers never fetch previews.").with_id("link-preview-privacy")))
        .child(div().text_xs().text_color(color(TEXT_MUTED)).child(text!(effective).with_id("link-preview-effective-policy")))
        .when(this.link_preview.writes.active.is_some(), |settings| settings.child(div().text_xs().text_color(color(TEXT_MUTED)).child(text!("Saving preference; previews stay off until it finishes.").with_id("link-preview-saving-policy"))))
        .when_some(problem, |settings, problem| settings.child(div().text_xs().text_color(color(TEXT_MUTED)).child(text!(problem).with_id("link-preview-policy-status"))))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    // GPUI's prelude also exports a test attribute.
    use super::{Mode, PolicyWrites, Settings};

    fn choice(mode: Mode) -> Settings {
        Settings {
            mode,
            ..Settings::default()
        }
    }

    #[test]
    fn policy_choices_serialize_writes_coalesce_and_only_apply_latest_reply() {
        let mut writes = PolicyWrites::default();
        let first = writes.choose(choice(Mode::Direct)).unwrap().unwrap();
        // This first write is physically blocked. No subsequent choice may
        // start another writer, even after many pointer/key actions.
        for index in 0_u64..1000 {
            let mode = if index.is_multiple_of(2) {
                Mode::Off
            } else {
                Mode::ProxyRequired
            };
            assert!(writes.choose(choice(mode)).unwrap().is_none());
            assert_eq!(writes.active.as_ref().unwrap().revision, first.revision);
        }
        assert_eq!(writes.desired().unwrap().mode, Mode::ProxyRequired);
        assert!(
            writes.finished(first.revision + 1).is_none(),
            "an unrelated reply cannot release active IO credit"
        );
        let (apply, latest) = writes.finished(first.revision).unwrap();
        assert!(!apply, "obsolete success cannot enable Direct");
        let latest = latest.unwrap();
        assert_eq!(latest.settings.mode, Mode::ProxyRequired);
        assert!(writes.pending.is_none());
        assert_eq!(writes.active.as_ref().unwrap().revision, latest.revision);
        assert!(
            writes.finished(first.revision).is_none(),
            "duplicate completion cannot retire the latest write"
        );
        let (apply, next) = writes.finished(latest.revision).unwrap();
        assert!(apply);
        assert!(next.is_none());
        assert!(writes.active.is_none());
    }

    #[test]
    fn policy_choice_during_replacement_write_still_wins_disk_order() {
        let mut writes = PolicyWrites::default();
        let first = writes.choose(choice(Mode::Direct)).unwrap().unwrap();
        writes.choose(choice(Mode::ProxyRequired)).unwrap();
        let (_, second) = writes.finished(first.revision).unwrap();
        let second = second.unwrap();
        assert!(writes.choose(choice(Mode::Off)).unwrap().is_none());
        let (apply, third) = writes.finished(second.revision).unwrap();
        assert!(!apply);
        let third = third.unwrap();
        // Whether an older IO reports success or failure, only the latest
        // completion may change effective policy, and Off is written last.
        assert_eq!(
            [
                first.settings.mode,
                second.settings.mode,
                third.settings.mode
            ],
            [Mode::Direct, Mode::ProxyRequired, Mode::Off]
        );
        assert_eq!(
            writes
                .finished(third.revision)
                .map(|(apply, next)| (apply, next.is_none())),
            Some((true, true))
        );
    }

    #[test]
    fn exhausted_policy_revision_never_reuses_old_authority_or_starts_more_io() {
        let mut writes = PolicyWrites::default();
        let first = writes.choose(choice(Mode::Direct)).unwrap().unwrap();
        writes.revision = u64::MAX;
        assert!(writes.choose(choice(Mode::Off)).is_err());
        assert!(writes.pending.is_none());
        assert_eq!(writes.active.as_ref().unwrap().revision, first.revision);
        let (apply, next) = writes.finished(first.revision).unwrap();
        assert!(!apply);
        assert!(next.is_none());
        assert!(writes.choose(choice(Mode::Direct)).is_err());
    }
}
