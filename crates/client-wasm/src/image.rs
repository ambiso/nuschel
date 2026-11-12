// SPDX-License-Identifier: EUPL-1.2
//! Image composition authority. No image parsing or original file bytes here.
use crate::content_broker::{
    ImageBegin, ImageBudget, ImageSend, validate_image_html, validate_thumbnail,
};
use nuschl_client::model::TextTarget;
use nuschl_core::{MAX_CONTENT_BYTES, model::ServerLimits};

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
            .check_text_wire(&caption)
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
        let caption = nuschl_legacy_html::legacy_body(&captured.caption, true);
        limits
            .check_text_wire(&caption)
            .map_err(|error| error.to_string())?;
        let mut combined = request.html.clone();
        if !caption.is_empty() {
            combined.push_str("<br/>");
            combined.push_str(&caption);
        }
        limits
            .check_html_total(&combined)
            .map_err(|error| error.to_string())?;
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
}
