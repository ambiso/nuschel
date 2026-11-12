// SPDX-License-Identifier: EUPL-1.2
//! Image composition authority. No image parsing or original file bytes here.
use crate::content_broker::{
    ImageBegin, ImageBudget, ImageSend, validate_image_html, validate_thumbnail,
};
use nuschl_client::model::TextTarget;
use nuschl_core::{MAX_CONTENT_BYTES, model::ServerLimits};

/// Fixed grammar makes the stripped-src text budget computable without an
/// HTML/XML parser in the credential-bearing SDK. Original image data is ASCII.
pub fn compose(image: &str, caption: &str, limits: &ServerLimits) -> Result<String, String> {
    validate_image_html(image)?;
    if !limits.allow_html || caption.len() > MAX_CONTENT_BYTES {
        return Err("This server does not allow this image message.".into());
    }
    let caption = nuschl_legacy_html::legacy_body(caption, true);
    let mut text = "<img src=\"\" />".to_owned();
    let mut wire = image.to_owned();
    if !caption.is_empty() {
        text.push_str("<br/>");
        text.push_str(&caption);
        wire.push_str("<br/>");
        wire.push_str(&caption);
    }
    limits
        .check_text_wire(&text)
        .map_err(|error| error.to_string())?;
    limits
        .check_html_total(&wire)
        .map_err(|error| error.to_string())?;
    Ok(wire)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Destination {
    pub server: String,
    pub generation: u64,
    pub channel: u32,
    pub instance: u64,
    pub target: TextTarget,
    pub peer: Option<u64>,
    pub own: u64,
}

#[derive(Clone)]
pub struct Captured {
    pub ticket: String,
    pub destination: Destination,
    pub caption: String,
    pub attachment_revision: String,
    pub draft_revision: String,
}

#[derive(Default)]
pub struct Images {
    next: u64,
    context: Option<(String, Destination)>,
    captured: Option<Captured>,
}

impl Images {
    pub fn invalidate(&mut self) {
        self.context = None;
        self.captured = None;
    }
    pub fn context(&mut self, destination: Option<Destination>) -> Option<String> {
        let Some(destination) = destination else {
            self.invalidate();
            return None;
        };
        if self
            .context
            .as_ref()
            .is_none_or(|(_, old)| old != &destination)
        {
            self.invalidate();
            self.next = self.next.checked_add(1)?;
            self.context = Some((format!("context:{}", self.next), destination));
        }
        self.context.as_ref().map(|(token, _)| token.clone())
    }
    pub fn begin(
        &mut self,
        request: ImageBegin,
        destination: Destination,
        limits: &ServerLimits,
    ) -> Result<ImageBudget, String> {
        if !limits.allow_html
            || request.caption.len() > MAX_CONTENT_BYTES
            || [&request.attachment_revision, &request.draft_revision]
                .into_iter()
                .any(|value| value.is_empty() || value.len() > 128)
            || self
                .context
                .as_ref()
                .is_none_or(|(token, old)| token != &request.context || old != &destination)
        {
            return Err(
                "This image draft or destination changed. Keep the attachment and try again."
                    .into(),
            );
        }
        let caption = nuschl_legacy_html::legacy_body(&request.caption, true);
        limits
            .check_text_wire(&format!(
                "<img src=\"\" />{}{}",
                if caption.is_empty() { "" } else { "<br/>" },
                caption
            ))
            .map_err(|error| error.to_string())?;
        let overhead = caption
            .len()
            .saturating_add(if caption.is_empty() { 0 } else { 5 });
        let budget = limits.inline_image_byte_budget().saturating_sub(overhead);
        if budget < 64 {
            return Err(
                "The caption leaves no room for an image under this server's current limit.".into(),
            );
        }
        self.next = self
            .next
            .checked_add(1)
            .ok_or("Image ticket counter exhausted.")?;
        let ticket = format!("image:{}", self.next);
        self.captured = Some(Captured {
            ticket: ticket.clone(),
            destination,
            caption: request.caption,
            attachment_revision: request.attachment_revision,
            draft_revision: request.draft_revision,
        });
        Ok(ImageBudget {
            ticket,
            max_input_bytes: 8 * 1024 * 1024,
            max_html_bytes: budget,
        })
    }
    pub fn validate(
        &self,
        request: &ImageSend,
        destination: &Destination,
        limits: &ServerLimits,
    ) -> Result<Captured, String> {
        let captured = self
            .captured
            .as_ref()
            .filter(|captured| {
                captured.ticket == request.ticket
                    && &captured.destination == destination
                    && captured.attachment_revision == request.attachment_revision
                    && captured.draft_revision == request.draft_revision
            })
            .ok_or("This image draft or destination changed. Keep the attachment and try again.")?;
        if !limits.allow_html {
            return Err("This server no longer allows inline images.".into());
        }
        validate_image_html(&request.html)?;
        validate_thumbnail(&request.thumbnail)?;
        compose(&request.html, &captured.caption, limits)?;
        Ok(captured.clone())
    }
    pub fn cancel(&mut self, ticket: &str) {
        if self
            .captured
            .as_ref()
            .is_some_and(|captured| captured.ticket == ticket)
        {
            self.captured = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn destination() -> Destination {
        Destination {
            server: "owned".into(),
            generation: 7,
            channel: 2,
            instance: 9,
            target: TextTarget::Channel(2),
            peer: None,
            own: 12,
        }
    }
    fn begin(context: String) -> ImageBegin {
        ImageBegin {
            context,
            caption: "<caption>\nnext".into(),
            attachment_revision: "a1".into(),
            draft_revision: "d1".into(),
        }
    }
    const IMAGE: &str = "<img src=\"data:image/png;base64,iVBORw0KGgo%3D\" />";
    fn send(ticket: String) -> ImageSend {
        ImageSend {
            ticket,
            attachment_revision: "a1".into(),
            draft_revision: "d1".into(),
            html: IMAGE.into(),
            thumbnail: nuschl_core::ContentBlock::Image {
                width: 1,
                height: 1,
                rgba: "AAAAAA==".into(),
                source: None,
            },
        }
    }
    #[test]
    fn opaque_context_cannot_retarget_reused_channel_or_reopened_draft() {
        let mut images = Images::default();
        let destination = destination();
        let limits = ServerLimits::default();
        let context = images.context(Some(destination.clone())).unwrap();
        let budget = images
            .begin(begin(context.clone()), destination.clone(), &limits)
            .unwrap();
        assert!(budget.max_html_bytes < limits.inline_image_byte_budget());
        let mut changed = destination.clone();
        changed.instance += 1;
        assert!(
            images
                .begin(begin(context.clone()), changed, &limits)
                .is_err()
        );
        images.invalidate();
        images.context(Some(destination.clone()));
        assert!(images.begin(begin(context), destination, &limits).is_err());
    }

    #[test]
    fn current_limits_source_revision_own_presence_and_cancel_are_rechecked() {
        let mut images = Images::default();
        let destination = destination();
        let limits = ServerLimits::default();
        let context = images.context(Some(destination.clone())).unwrap();
        let budget = images
            .begin(begin(context), destination.clone(), &limits)
            .unwrap();
        let mut request = send(budget.ticket.clone());
        assert!(images.validate(&request, &destination, &limits).is_ok());
        request.draft_revision = "different".into();
        assert!(images.validate(&request, &destination, &limits).is_err());
        request.draft_revision = "d1".into();
        let mut changed = destination.clone();
        changed.own += 1;
        assert!(images.validate(&request, &changed, &limits).is_err());
        let reduced = ServerLimits {
            image_message_length: IMAGE.len() - 1,
            ..limits.clone()
        };
        assert!(images.validate(&request, &destination, &reduced).is_err());
        images.cancel("unrelated");
        assert!(images.validate(&request, &destination, &limits).is_ok());
        images.cancel(&budget.ticket);
        assert!(images.validate(&request, &destination, &limits).is_err());
    }

    #[test]
    fn fixed_image_caption_is_one_escaped_message_with_independent_text_and_total_caps() {
        let limits = ServerLimits::default();
        let wire = compose(IMAGE, "<literal>\nnext", &limits).unwrap();
        assert_eq!(wire, format!("{IMAGE}<br/>&lt;literal&gt;<br/>next"));
        assert!(
            compose(
                IMAGE,
                "",
                &ServerLimits {
                    message_length: 1,
                    ..limits.clone()
                }
            )
            .is_err()
        );
        assert!(
            compose(
                IMAGE,
                "",
                &ServerLimits {
                    image_message_length: IMAGE.len() - 1,
                    ..limits.clone()
                }
            )
            .is_err()
        );
        assert!(
            compose(
                IMAGE,
                "",
                &ServerLimits {
                    message_length: 0,
                    image_message_length: 0,
                    ..limits.clone()
                }
            )
            .is_ok()
        );
        assert!(
            compose(
                IMAGE,
                "",
                &ServerLimits {
                    allow_html: false,
                    ..limits
                }
            )
            .is_err()
        );
        for html in [
            "<img src=\"https://outside.invalid\" />",
            "<img src=\"data:image/png;base64,iVBORw0KGgo%3D\" onerror=\"x\" />",
            "<img src=\"data:image/png;base64,iVBORw0KGgo%3d\" />",
        ] {
            assert!(compose(html, "", &ServerLimits::default()).is_err());
        }
    }
}
