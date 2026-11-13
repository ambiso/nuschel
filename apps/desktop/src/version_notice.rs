// SPDX-License-Identifier: EUPL-1.2
//! A Nuschl server's minimum-version policy (docs/extension.md, "Minimum
//! versions"), per server: below the recommended version a notice the user
//! can dismiss, below the minimum a warning with the date that cannot be
//! dismissed, and once refused the version to update to. Each offers the
//! updater where this build has one (Windows, macOS).
use crate::{
    dialogs::{Tone, styled_button},
    state::Desktop,
    theme::{DANGER, Token, WARNING},
    update::UpdateState,
};
use gpui_kit::{AnyElement, Context, div, prelude::*};
use nuschl_protocol::nuschl::v1::Policy;
use nuschl_protocol::nuschl::version::{Standing, Version};

/// What the server's last hello said about this client's version.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VersionNotice {
    standing: Option<Standing>,
    /// The recommended version whose notice the user dismissed; a higher one
    /// shows again.
    dismissed: Option<Version>,
}

impl VersionNotice {
    /// From the server's hello (its policy); a server without the
    /// extension asks nothing.
    pub fn negotiated(&mut self, policy: Option<&Policy>) {
        let now_ms = nuschl_core::now_ms();
        let current = nuschl_session::connection::client_version();
        self.standing = Some(policy.map_or(Standing::Current, |policy| {
            Standing::of(policy, Some(&current), now_ms.div_euclid(1000))
        }));
    }

    pub fn dismiss(&mut self) {
        if let Some(Standing::Recommended { version }) = &self.standing {
            self.dismissed = Some(version.clone());
        }
    }

    /// What to show: tone, icon, text, and whether it can be dismissed.
    fn shown(&self) -> Option<(Token, &'static str, String, bool)> {
        let current = nuschl_session::connection::client_version();
        match self.standing.as_ref()? {
            Standing::Current => None,
            Standing::Recommended { version } => (self.dismissed.as_ref() != Some(version))
                .then(|| {
                    (
                        WARNING,
                        "info",
                        format!(
                            "Update recommended: this server suggests Nuschl {version} or newer (this is {current})."
                        ),
                        true,
                    )
                }),
            Standing::Required { version, from } => Some((
                WARNING,
                "alert",
                format!(
                    "Update required: from {} this server admits only Nuschl {version} or newer (this is {current}).",
                    local_date(*from)
                ),
                false,
            )),
            Standing::Refused { version } => Some((
                DANGER,
                "alert",
                format!("Update Nuschl to connect: this server requires {version} or newer."),
                false,
            )),
        }
    }
}

/// A date in the local time zone, as the timeline writes dates.
fn local_date(unix_seconds: i64) -> String {
    jiff::Timestamp::from_second(unix_seconds).map_or_else(
        |_| String::new(),
        |stamp| {
            stamp
                .to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d")
                .to_string()
        },
    )
}

/// The updater's next step, if this build updates itself and is not busy.
fn update_button(this: &Desktop, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    if !crate::update::supported() {
        return None;
    }
    Some(match &this.update {
        UpdateState::Available(update) => styled_button(
            "version-update",
            format!("Update to {}", update.version),
            Tone::Primary,
            |this, _, cx| {
                this.install_update();
                cx.notify();
            },
            cx,
        ),
        UpdateState::Idle | UpdateState::UpToDate | UpdateState::Failed(_) => styled_button(
            "version-check",
            "Check for updates",
            Tone::Primary,
            |this, _, cx| {
                this.check_update_now();
                cx.notify();
            },
            cx,
        ),
        // Progress and the restart are in the update bar at the top.
        UpdateState::Checking | UpdateState::Installing { .. } | UpdateState::Installed => {
            return None;
        }
    })
}

/// The banner for the server at `index`, if its policy asks for an update.
pub fn banner(this: &Desktop, index: usize, cx: &mut Context<Desktop>) -> Option<AnyElement> {
    let server = this.client.servers.get(index)?;
    let (tone, icon, text, dismissible) = this.ui.get(&server.id)?.version.shown()?;
    // Once refused, the connection notice already says it; this adds the updater.
    let update = update_button(this, cx);
    if tone == DANGER && update.is_none() {
        return None;
    }
    let dismiss = dismissible.then(|| {
        styled_button(
            "version-dismiss",
            "Dismiss",
            Tone::Ghost,
            move |this, _, cx| {
                if let Some(ui) = this.ui_of_mut(index) {
                    ui.version.dismiss();
                }
                cx.notify();
            },
            cx,
        )
    });
    let actions = (update.is_some() || dismiss.is_some()).then(|| {
        div()
            .flex()
            .gap_2()
            .children(update)
            .children(dismiss)
            .into_any_element()
    });
    Some(crate::view::banner(
        "version-banner",
        tone,
        icon,
        text,
        actions,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuschl_protocol::nuschl::{v1, version};

    fn notice(policy: v1::Policy) -> VersionNotice {
        let mut notice = VersionNotice::default();
        notice.negotiated(Some(&policy));
        notice
    }

    #[test]
    fn a_recommendation_can_be_dismissed_and_a_requirement_cannot() {
        let ahead = version::to_wire(&Version::new(99, 0, 0)).unwrap();
        let mut recommended = notice(v1::Policy {
            recommended_client: ahead,
            ..Default::default()
        });
        let (_, _, text, dismissible) = recommended.shown().unwrap();
        assert!(dismissible);
        assert!(text.starts_with("Update recommended"), "{text}");
        assert!(text.contains("99.0.0"), "{text}");
        recommended.dismiss();
        assert!(recommended.shown().is_none());
        // A higher recommendation shows again.
        recommended.negotiated(Some(&v1::Policy {
            recommended_client: version::to_wire(&Version::new(99, 1, 0)).unwrap(),
            ..Default::default()
        }));
        assert!(recommended.shown().is_some());

        let mut required = notice(v1::Policy {
            minimum_client: ahead,
            minimum_client_from: 32_472_144_000,
            ..Default::default()
        });
        let (tone, _, text, dismissible) = required.shown().unwrap();
        assert!(!dismissible && tone == WARNING);
        assert!(
            text.contains("2999-01-0") && text.contains("99.0.0"),
            "{text}"
        );
        required.dismiss();
        assert!(required.shown().is_some(), "not dismissible");

        let refused = notice(v1::Policy {
            minimum_client: ahead,
            ..Default::default()
        });
        assert!(refused.shown().unwrap().0 == DANGER);
        assert!(notice(v1::Policy::default()).shown().is_none());
    }
}
