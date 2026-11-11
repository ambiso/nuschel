// SPDX-License-Identifier: EUPL-1.2
//! Identities and key transparency in the desktop (docs/e2ee.md §4, §5.5): what the
//! session's driver reports per server, the verification part of a user's profile
//! (state, safety number, "Mark as verified"), and the alarms.
//!
//! Wording follows docs/e2ee.md §4: a verified contact whose identity changed is
//! blocked until the user accepts it (sending direct messages to them stays
//! possible on servers without end-to-end encryption, but the warning stays); a
//! contact verified on another server whose log binds the same identity shows
//! "verified (same identity as on X)", by server, never by name.
use crate::{
    dialogs::{Tone, styled_button},
    state::Desktop,
    theme::{self, *},
};
use gpui_kit::{prelude::*, *};
use nuschel_ipc::crypto::{ContactState, ContactView, HeadOutcome, IdentityView, MonitorOutcome};
use nuschel_session::{
    connection::Command,
    identity::{IdentityCommand, IdentityEvent},
};
use std::collections::BTreeMap;

/// What a server's session reported about identities.
#[derive(Default)]
pub struct IdentityState {
    /// Contacts by account id.
    pub contacts: BTreeMap<[u8; 16], ContactView>,
    /// Registration numbers to account ids, from checks that named a user.
    pub accounts: BTreeMap<u32, [u8; 16]>,
    /// Our identity, as last reported.
    pub own: Option<IdentityView>,
    /// The server's log: the last head check.
    pub log: Option<HeadOutcome>,
    pub monitor: Option<MonitorOutcome>,
    /// A recovery key to show once.
    pub recovery_key: Option<String>,
    /// The blocking alarm: someone else's key on our account, or a log that lied.
    pub alarm: Option<String>,
    /// Checks in flight, by registration number (shown as "Checking…").
    pub checking: BTreeMap<u32, bool>,
}

impl IdentityState {
    /// Folds an event in; returns a notice for the server's banner, if any.
    #[cfg(test)]
    pub fn apply(&mut self, event: IdentityEvent) -> Option<String> {
        self.apply_named(event, |_| None)
    }

    /// Like [`Self::apply`], naming contacts by registration number where known
    /// (`name` gives a registered user's current name).
    pub fn apply_named(
        &mut self,
        event: IdentityEvent,
        name: impl Fn(u32) -> Option<String>,
    ) -> Option<String> {
        match event {
            IdentityEvent::Own(view) => {
                let alarm = view.alarms.first().map(|alarm| alarm.reason.clone());
                if alarm.is_some() {
                    self.alarm = alarm;
                }
                self.own = Some(view);
                None
            }
            IdentityEvent::RecoveryKey(key) => {
                self.recovery_key = Some(key);
                None
            }
            IdentityEvent::Log(outcome) => {
                let notice = match &outcome {
                    HeadOutcome::Misbehaviour { reason } => {
                        self.alarm = Some(reason.clone());
                        Some(format!(
                            "This server's key-transparency log contradicts itself: {reason} \
                             Identities on this server are not checked automatically any more."
                        ))
                    }
                    _ => None,
                };
                self.log = Some(outcome);
                notice
            }
            IdentityEvent::Published { .. } => None,
            IdentityEvent::Monitor(outcome) => {
                let notice = match &outcome {
                    MonitorOutcome::OwnKeyAlarm { reason } => {
                        self.alarm = Some(reason.clone());
                        Some(reason.clone())
                    }
                    MonitorOutcome::Misbehaviour { reason } => {
                        self.alarm = Some(reason.clone());
                        Some(format!(
                            "This server's key-transparency log contradicts itself: {reason}"
                        ))
                    }
                    _ => None,
                };
                self.monitor = Some(outcome);
                notice
            }
            IdentityEvent::Contact { user_id, view } => {
                let Ok(account) = <[u8; 16]>::try_from(view.account.as_slice()) else {
                    return None;
                };
                if let Some(user_id) = user_id {
                    self.accounts.insert(user_id, account);
                    self.checking.remove(&user_id);
                }
                let before = self.contacts.get(&account).map(|old| old.state.clone());
                let changed = matches!(view.state, ContactState::KeyChanged { .. })
                    && !matches!(before, Some(ContactState::KeyChanged { .. }));
                let who = self
                    .accounts
                    .iter()
                    .find(|(_, known)| **known == account)
                    .and_then(|(user_id, _)| name(*user_id));
                let notice = changed.then(|| match (&view.state, who) {
                    (ContactState::KeyChanged { blocking: true }, Some(who)) => format!(
                        "{who}'s identity changed, and you had verified them. Open their \
                         profile and compare safety numbers before you trust it."
                    ),
                    (ContactState::KeyChanged { blocking: true }, None) => {
                        "A verified contact's identity changed. Open their profile and \
                         compare safety numbers before you trust it."
                            .to_owned()
                    }
                    (_, Some(who)) => format!(
                        "{who}'s identity changed. Their profile shows the new safety number."
                    ),
                    (_, None) => {
                        "A contact's identity changed. Their profile shows the new safety number."
                            .to_owned()
                    }
                });
                self.contacts.insert(account, view);
                notice
            }
            IdentityEvent::Failed(_) => None,
        }
    }

    pub fn contact_of(&self, user_id: u32) -> Option<&ContactView> {
        self.accounts
            .get(&user_id)
            .and_then(|account| self.contacts.get(account))
    }
}

/// The Nuschel identity for Settings, straight from the crypto worker.
#[derive(Default)]
pub struct OwnIdentity {
    pub view: Option<IdentityView>,
    /// Shown once, until the user saved or skipped it.
    pub recovery_key: Option<String>,
    pub error: Option<String>,
}

impl OwnIdentity {
    pub fn apply(&mut self, result: Result<nuschel_ipc::crypto::CryptoResponse, String>) {
        use nuschel_ipc::crypto::CryptoResponse;
        match result {
            Ok(CryptoResponse::Identity(view)) => {
                if !view.recovery_pending {
                    self.recovery_key = None;
                }
                self.view = Some(view);
                self.error = None;
            }
            Ok(CryptoResponse::RecoveryKey(key)) => self.recovery_key = Some(key),
            Ok(_) => {}
            Err(error) => self.error = Some(error),
        }
    }
}

/// Settings → Identity: the Nuschel identity, its recovery key, the policy.
pub fn settings_rows(this: &Desktop, cx: &mut Context<Desktop>) -> Vec<AnyElement> {
    use nuschel_ipc::crypto::{CryptoRequest, RecoveryPolicy};
    let own = &this.own_identity;
    let fingerprint = own.view.as_ref().and_then(|view| view.fingerprint.clone());
    let mut rows = vec![
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(text!("Nuschel identity").with_id("kt-own-heading")),
            )
            .child(
                div()
                    .text_xs()
                    .font_family(theme::mono_font())
                    .text_color(color(TEXT_MUTED))
                    .child(
                        text!(fingerprint.clone().unwrap_or_else(|| {
                            "Created when you first connect to a Nuschel server with key                              transparency."
                                .into()
                        }))
                        .with_id("kt-own-fingerprint"),
                    ),
            )
            .into_any_element(),
    ];
    if let Some(view) = &own.view
        && view.fingerprint.is_some()
    {
        let status = if view.recovery_pending {
            "A recovery key was made with your identity. Write it down, or skip it: without              it, losing this device means a new identity."
        } else if view.recoverable {
            "Recoverable with your recovery key."
        } else {
            "No recovery key: losing this device means a new identity, which your contacts              see as a change."
        };
        rows.push(
            div()
                .text_xs()
                .text_color(color(if view.recovery_pending {
                    WARNING
                } else {
                    TEXT_FAINT
                }))
                .child(text!(status).with_id("kt-recovery-status"))
                .into_any_element(),
        );
        for alarm in &view.alarms {
            rows.push(
                div()
                    .text_xs()
                    .text_color(color(DANGER))
                    .child(text!(format!("{}: {}", alarm.label, alarm.reason)).with_id("kt-alarm"))
                    .into_any_element(),
            );
        }
        if let Some(key) = &own.recovery_key {
            rows.push(
                div()
                    .p_3()
                    .rounded(px(6.))
                    .bg(color(INPUT))
                    .font_family(theme::mono_font())
                    .child(text!(key.clone()).with_id("kt-recovery-key"))
                    .into_any_element(),
            );
        }
        if view.recovery_pending {
            let mut buttons = div().flex().gap_2();
            if own.recovery_key.is_none() {
                buttons = buttons.child(styled_button(
                    "kt-show-recovery",
                    "Show recovery key",
                    Tone::Primary,
                    |this, _, cx| {
                        this.driver.identity(CryptoRequest::ShowRecoveryKey);
                        cx.notify();
                    },
                    cx,
                ));
            } else {
                buttons = buttons.child(styled_button(
                    "kt-saved-recovery",
                    "I wrote it down",
                    Tone::Primary,
                    |this, _, cx| {
                        this.own_identity.recovery_key = None;
                        this.driver.identity(CryptoRequest::RecoveryKeySaved);
                        cx.notify();
                    },
                    cx,
                ));
            }
            if this.settings.recovery_key != RecoveryPolicy::Require {
                buttons = buttons.child(styled_button(
                    "kt-skip-recovery",
                    "Skip",
                    Tone::Ghost,
                    |this, _, cx| {
                        this.own_identity.recovery_key = None;
                        this.driver.identity(CryptoRequest::SkipRecoveryKey);
                        cx.notify();
                    },
                    cx,
                ));
            }
            rows.push(buttons.into_any_element());
        }
    } else {
        let set = |policy: RecoveryPolicy| -> crate::settings::Action {
            std::rc::Rc::new(
                move |this: &mut Desktop, _: &mut Window, _: &mut Context<Desktop>| {
                    this.update_settings(|settings| settings.recovery_key = policy)
                },
            )
        };
        let policy = this.settings.recovery_key;
        rows.push(crate::settings::managed(
            this,
            "recovery_key",
            crate::settings::segmented(
                "kt-recovery-policy",
                "Recovery key when the identity is created",
                vec![
                    (
                        "Offer",
                        policy == RecoveryPolicy::Offer,
                        set(RecoveryPolicy::Offer),
                    ),
                    (
                        "Require",
                        policy == RecoveryPolicy::Require,
                        set(RecoveryPolicy::Require),
                    ),
                    (
                        "None",
                        policy == RecoveryPolicy::Never,
                        set(RecoveryPolicy::Never),
                    ),
                ],
                cx,
            ),
        ));
    }
    if let Some(error) = &own.error {
        rows.push(
            div()
                .text_xs()
                .text_color(color(DANGER))
                .child(text!(error.clone()).with_id("kt-own-error"))
                .into_any_element(),
        );
    }
    rows
}

/// Asks the server's session to check a registered user (their profile opened, or
/// a direct conversation with them).
pub fn check(this: &mut Desktop, server: usize, user_id: u32, direct: bool) {
    if let Some(ui) = this.ui_of_mut(server) {
        ui.identity.checking.insert(user_id, true);
    }
    this.run(
        server,
        Command::Identity(IdentityCommand::Check {
            user_id: Some(user_id),
            account: None,
            direct,
        }),
    );
}

/// The headline for a contact's state.
#[cfg(test)]
pub fn summary(view: &ContactView) -> (String, Token) {
    let (headline, tone) = describe(view);
    (headline.0, tone)
}

/// (headline, explanation) and the colour.
fn describe(view: &ContactView) -> ((String, Option<&'static str>), Token) {
    match &view.state {
        ContactState::Unknown => (("Not checked yet".into(), None), TEXT_MUTED),
        ContactState::NoIdentity => (
            (
                "No identity on this server".into(),
                Some("They have not used a Nuschel client with key transparency here yet."),
            ),
            TEXT_MUTED,
        ),
        ContactState::Unverified => (
            (
                "Not verified".into(),
                Some("Compare the safety number with them in person or on a call you trust."),
            ),
            TEXT_MUTED,
        ),
        ContactState::Verified { elsewhere: None } => (("Verified".into(), None), SUCCESS),
        ContactState::Verified {
            elsewhere: Some(server),
        } => (
            (format!("Verified (same identity as on {server})"), None),
            SUCCESS,
        ),
        ContactState::KeyChanged { blocking: true } => (
            (
                "Identity changed".into(),
                Some(
                    "You had verified this person, and their account now has a different \
                     identity. Compare safety numbers again before you trust it.",
                ),
            ),
            DANGER,
        ),
        ContactState::KeyChanged { blocking: false } => (
            (
                "Identity changed".into(),
                Some("Their account has a different identity than when you first saw it."),
            ),
            WARNING,
        ),
        ContactState::Unavailable { verified, .. } => (
            (
                if *verified {
                    "Verified, but automatic checks are unavailable".into()
                } else {
                    "Automatic verification unavailable".into()
                },
                Some("The server could not prove this identity. Nuschel will try again."),
            ),
            WARNING,
        ),
        ContactState::Misbehaviour { .. } => (
            (
                "Do not trust this identity".into(),
                Some("The server's key log contradicts itself."),
            ),
            DANGER,
        ),
    }
}

/// The blocking alarm: someone else's key on our account, or a log that lied.
pub fn alarm(this: &Desktop, server: usize, reason: &str, cx: &mut Context<Desktop>) -> AnyElement {
    let _ = this;
    div()
        .px_6()
        .pb_6()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_color(color(DANGER))
                .font_weight(FontWeight::SEMIBOLD)
                .child(crate::assets::icon("alert", 18., color(DANGER)))
                .child(text!("Check this before you continue").with_id("kt-alarm-title")),
        )
        .child(
            div()
                .text_sm()
                .child(text!(reason.to_owned()).with_id("kt-alarm-reason")),
        )
        .child(
            div().text_sm().text_color(color(TEXT_MUTED)).child(
                text!(
                    "If you did not set up Nuschel on another device for this account, the \
                     server (or someone controlling it) is presenting another key as yours. \
                     Tell your contacts on another channel, and contact the server's operator. \
                     Revoking devices comes with device linking."
                )
                .with_id("kt-alarm-advice"),
            ),
        )
        .child(div().flex().gap_2().child(styled_button(
            "kt-alarm-acknowledge",
            "I understand",
            Tone::Danger,
            move |this, _, cx| {
                if let Some(ui) = this.ui_of_mut(server) {
                    ui.identity.alarm = None;
                }
                this.run(server, Command::Identity(IdentityCommand::AcknowledgeAlarm));
                this.panel = None;
                cx.notify();
            },
            cx,
        )))
        .into_any_element()
}

/// The safety number as three lines of four groups.
pub fn safety_lines(number: &str) -> Vec<String> {
    let groups: Vec<&str> = number.split(' ').collect();
    groups.chunks(4).map(|line| line.join(" ")).collect()
}

/// The verification part of a registered user's profile.
pub fn section(
    this: &Desktop,
    server: usize,
    user_id: u32,
    own: bool,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let none = IdentityState::default();
    let state = this.ui_of(server).map_or(&none, |ui| &ui.identity);
    let mut column = div()
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .rounded(px(8.))
        .border_1()
        .border_color(color(BORDER))
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(text!("Identity").with_id("kt-heading")),
        );
    if own {
        let fingerprint = state
            .own
            .as_ref()
            .and_then(|own| own.fingerprint.clone())
            .unwrap_or_else(|| "Not created yet".into());
        return column
            .child(div().text_sm().text_color(color(TEXT_MUTED)).child(
                text!("This is you. Others compare safety numbers with you.").with_id("kt-own"),
            ))
            .child(
                div()
                    .text_xs()
                    .font_family(theme::mono_font())
                    .child(text!(fingerprint).with_id("kt-own-fingerprint")),
            )
            .into_any_element();
    }
    let Some(view) = state.contact_of(user_id) else {
        let text = if state.log.is_none() {
            "This server does not publish identities (no key transparency)."
        } else if state.checking.contains_key(&user_id) {
            "Checking their identity…"
        } else {
            "Not checked yet."
        };
        return column
            .child(
                div()
                    .text_sm()
                    .text_color(color(TEXT_MUTED))
                    .child(text!(text).with_id("kt-status")),
            )
            .into_any_element();
    };
    let ((headline, explanation), tone) = describe(view);
    column = column
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .text_color(color(tone))
                .child(crate::assets::icon(
                    match view.state {
                        ContactState::Verified { .. } => "check",
                        ContactState::KeyChanged { .. } | ContactState::Misbehaviour { .. } => {
                            "alert"
                        }
                        _ => "info",
                    },
                    16.,
                    color(tone),
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(text!(headline).with_id("kt-status")),
                ),
        )
        .when_some(explanation, |column, explanation| {
            column.child(
                div()
                    .text_xs()
                    .text_color(color(TEXT_MUTED))
                    .child(text!(explanation).with_id("kt-explanation")),
            )
        });
    // A key change explains itself above; other notices (a binding that came and
    // went) are shown here.
    if let Some(notice) = view
        .notice
        .as_ref()
        .filter(|_| !matches!(view.state, ContactState::KeyChanged { .. }))
    {
        column = column.child(
            div()
                .px_3()
                .py_2()
                .rounded(px(6.))
                .bg(tint(WARNING, 0x26))
                .text_sm()
                .child(text!(notice.clone()).with_id("kt-notice")),
        );
    }
    if let Some(number) = &view.safety_number {
        column =
            column
                .child(
                    div().text_xs().text_color(color(TEXT_MUTED)).child(
                        text!(
                            if matches!(view.state, ContactState::KeyChanged { blocking: true }) {
                                "The safety number you verified (before the change)"
                            } else {
                                "Safety number"
                            }
                        )
                        .with_id("kt-safety-label"),
                    ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .font_family(theme::mono_font())
                        .text_lg()
                        .children(safety_lines(number).into_iter().enumerate().map(
                            |(index, line)| {
                                div().child(text!(line).with_id(("kt-safety-line", index)))
                            },
                        )),
                );
    }
    let account = <[u8; 16]>::try_from(view.account.as_slice()).ok();
    let mut buttons = div().flex().gap_2().flex_wrap();
    match (&view.state, account) {
        (ContactState::KeyChanged { .. }, Some(account)) => {
            buttons = buttons.child(styled_button(
                "kt-accept",
                "Accept new identity",
                Tone::Danger,
                move |this, _, cx| {
                    this.run(
                        server,
                        Command::Identity(IdentityCommand::AcceptKeyChange { account }),
                    );
                    cx.notify();
                },
                cx,
            ));
        }
        (ContactState::Verified { elsewhere: None }, Some(account)) => {
            buttons = buttons.child(styled_button(
                "kt-unverify",
                "Remove verification",
                Tone::Secondary,
                move |this, _, cx| {
                    this.run(
                        server,
                        Command::Identity(IdentityCommand::UnmarkVerified { account }),
                    );
                    cx.notify();
                },
                cx,
            ));
        }
        (
            ContactState::Unverified
            | ContactState::Unavailable {
                verified: false, ..
            },
            Some(account),
        ) if view.safety_number.is_some() => {
            buttons = buttons.child(styled_button(
                "kt-verify",
                "Mark as verified",
                Tone::Primary,
                move |this, _, cx| {
                    this.run(
                        server,
                        Command::Identity(IdentityCommand::MarkVerified { account }),
                    );
                    cx.notify();
                },
                cx,
            ));
        }
        _ => {}
    }
    buttons = buttons.child(styled_button(
        "kt-recheck",
        "Check again",
        Tone::Ghost,
        move |this, _, cx| {
            check(this, server, user_id, false);
            cx.notify();
        },
        cx,
    ));
    let compare = view.safety_number.is_some()
        && !matches!(view.state, ContactState::KeyChanged { blocking: true });
    column
        .when(compare, |column| {
            column.child(
                div().text_xs().text_color(color(TEXT_FAINT)).child(
                    text!("They see the same 60 digits. Compare them outside this server.")
                        .with_id("kt-hint"),
                ),
            )
        })
        .child(buttons)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that glob includes GPUI's `test` macro, which shadows `#[test]`.
    use super::{
        ContactState, ContactView, IdentityEvent, IdentityState, MonitorOutcome, SUCCESS,
        safety_lines, summary,
    };

    fn view(state: ContactState) -> ContactView {
        ContactView {
            server: "s".into(),
            account: vec![1; 16],
            state,
            identity: Some("ab".repeat(32)),
            safety_number: Some(
                (0..12)
                    .map(|n| format!("{n:05}"))
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            qr: Vec::new(),
            notice: None,
            direct: false,
        }
    }

    #[test]
    fn safety_numbers_show_as_three_lines_of_four_groups() {
        let lines = safety_lines(
            view(ContactState::Unverified)
                .safety_number
                .as_ref()
                .unwrap(),
        );
        assert_eq!(
            lines,
            [
                "00000 00001 00002 00003",
                "00004 00005 00006 00007",
                "00008 00009 00010 00011"
            ]
        );
    }

    #[test]
    fn cross_server_verification_names_the_server_never_the_person() {
        let (text, tone) = summary(&view(ContactState::Verified {
            elsewhere: Some("Example server".into()),
        }));
        assert_eq!(text, "Verified (same identity as on Example server)");
        assert!(tone == SUCCESS);
    }

    #[test]
    fn key_changes_raise_one_notice_and_alarms_stick() {
        let mut state = IdentityState::default();
        let first = IdentityEvent::Contact {
            user_id: Some(7),
            view: view(ContactState::Verified { elsewhere: None }),
        };
        assert_eq!(state.apply(first), None);
        assert!(state.contact_of(7).is_some());
        let changed = IdentityEvent::Contact {
            user_id: None,
            view: view(ContactState::KeyChanged { blocking: true }),
        };
        assert!(
            state
                .apply(changed.clone())
                .unwrap()
                .contains("verified contact")
        );
        assert_eq!(state.apply(changed), None, "only once");
        let alarm = IdentityEvent::Monitor(MonitorOutcome::OwnKeyAlarm {
            reason: "Someone else's key".into(),
        });
        assert!(state.apply(alarm).is_some());
        assert!(state.alarm.is_some());
    }
}
