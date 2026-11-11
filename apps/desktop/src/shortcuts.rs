// SPDX-License-Identifier: EUPL-1.2
//! Keyboard shortcuts: bindings in GPUI keystroke notation, and how presses
//! and releases from the window and from the system-wide backends become
//! actions. Holds act on press and release, toggles once per press. Key
//! repeat never acts, nor does the second report of a press that both the
//! window and the desktop saw.
use crate::global_shortcuts::{Bindings, GlobalEvent, GlobalShortcuts};
use gpui_kit::{Keystroke, Modifiers, OperatingSystem};
use nuschel_core::audio::TransmitMode;
use nuschel_session::config::{ClientSettings, ShortcutAction};
use std::collections::{BTreeMap, BTreeSet};

pub const UNAVAILABLE: &str =
    "System-wide shortcuts are not available on this desktop; they work while Nuschel is focused.";

pub fn label(action: ShortcutAction) -> &'static str {
    match action {
        ShortcutAction::PushToTalk => "Push to talk",
        ShortcutAction::ToggleMute => "Toggle mute",
        ShortcutAction::ToggleDeafen => "Toggle deafen",
        ShortcutAction::PushToMute => "Push to mute",
        ShortcutAction::Find => "Find a channel or person",
        ShortcutAction::Search => "Search this conversation",
        ShortcutAction::SearchAll => "Search all servers",
    }
}

/// Written in the settings for an action deliberately left without a key
/// (in-app actions have defaults, so a missing entry means the default).
pub const UNSET: &str = "none";

/// An in-app action's key when the settings name none: Ctrl+K, Ctrl+F and
/// Ctrl+Shift+F (Cmd on macOS). Voice actions have no default.
fn default_binding(action: ShortcutAction) -> Option<&'static str> {
    let mac = cfg!(target_os = "macos");
    match action {
        ShortcutAction::Find => Some(if mac { "super-k" } else { "ctrl-k" }),
        ShortcutAction::Search => Some(if mac { "super-f" } else { "ctrl-f" }),
        ShortcutAction::SearchAll => Some(if mac { "super-shift-f" } else { "ctrl-shift-f" }),
        _ => None,
    }
}

/// The key bound to an action: the settings', else its default.
pub fn binding(settings: &ClientSettings, action: ShortcutAction) -> Option<&str> {
    match settings.shortcuts.get(&action).map(String::as_str) {
        Some(UNSET) => None,
        Some(key) => Some(key),
        None => default_binding(action),
    }
}

/// Push to talk does something only in push-to-talk mode, push to mute only
/// in the others. An action that would do nothing neither takes its key in
/// the window nor holds it system-wide.
pub fn applies(action: ShortcutAction, transmit: TransmitMode) -> bool {
    match action {
        ShortcutAction::PushToTalk => transmit == TransmitMode::PushToTalk,
        ShortcutAction::PushToMute => transmit != TransmitMode::PushToTalk,
        _ => true,
    }
}

/// The voice bindings in effect for the current transmission mode (the ones
/// held and registered system-wide; in-app actions go through
/// [`app_action`]).
pub fn active(settings: &ClientSettings) -> Bindings {
    ShortcutAction::ALL
        .into_iter()
        .filter(|action| action.system_wide() && applies(*action, settings.audio.transmit))
        .filter_map(|action| Some((action, binding(settings, action)?.to_string())))
        .collect()
}

/// The in-app action a key press in the window triggers, if any.
pub fn app_action(settings: &ClientSettings, keystroke: &Keystroke) -> Option<ShortcutAction> {
    ShortcutAction::ALL
        .into_iter()
        .filter(|action| !action.system_wide())
        .find(|action| {
            binding(settings, *action)
                .and_then(parse)
                .is_some_and(|bound| matches(&bound, keystroke))
        })
}

pub fn parse(binding: &str) -> Option<Keystroke> {
    Keystroke::parse_with_os(binding, OperatingSystem::Linux)
        .ok()
        .filter(|keystroke| !keystroke.key.is_empty())
}

/// A pressed key in the settings' notation: "ctrl-shift-m", "f13", "control".
pub fn notation(keystroke: &Keystroke) -> String {
    let modifiers = keystroke.modifiers;
    let mut text: String = [
        (modifiers.control, "ctrl-"),
        (modifiers.alt, "alt-"),
        (modifiers.shift, "shift-"),
        (modifiers.platform, "super-"),
    ]
    .into_iter()
    .filter_map(|(held, name)| held.then_some(name))
    .collect();
    text.push_str(&keystroke.key);
    text
}

/// How a binding reads: "Ctrl+Shift+M", "F13".
pub fn display(binding: &str) -> String {
    let Some(keystroke) = parse(binding) else {
        return binding.to_string();
    };
    let platform = if cfg!(target_os = "macos") {
        "Cmd"
    } else {
        "Super"
    };
    let modifiers = keystroke.modifiers;
    let mut parts: Vec<String> = [
        (modifiers.control, "Ctrl"),
        (modifiers.alt, "Alt"),
        (modifiers.shift, "Shift"),
        (modifiers.platform, platform),
    ]
    .into_iter()
    .filter_map(|(held, name)| held.then_some(name))
    .map(String::from)
    .collect();
    parts.push(match keystroke.key.as_str() {
        "control" => "Ctrl".into(),
        "platform" => platform.into(),
        "pageup" => "Page Up".into(),
        "pagedown" => "Page Down".into(),
        "escape" => "Esc".into(),
        key => {
            let mut chars = key.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        }
    });
    parts.join("+")
}

/// A press is the binding: the same key with the same modifiers.
fn matches(binding: &Keystroke, pressed: &Keystroke) -> bool {
    let (wanted, held) = (binding.modifiers, pressed.modifiers);
    binding.key.eq_ignore_ascii_case(&pressed.key)
        && (wanted.control, wanted.alt, wanted.shift, wanted.platform)
            == (held.control, held.alt, held.shift, held.platform)
}

/// Whether a modifier-only binding ("control") is held.
fn modifier_held(binding: &Keystroke, modifiers: &Modifiers) -> Option<bool> {
    match binding.key.as_str() {
        "control" => Some(modifiers.control),
        "alt" => Some(modifiers.alt),
        "shift" => Some(modifiers.shift),
        "platform" => Some(modifiers.platform),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    Window,
    System,
}

/// What an action does now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    PushToTalk(bool),
    PushToMute(bool),
    ToggleMute,
    ToggleDeafen,
}

/// The actions whose keys are down, and who reported them.
#[derive(Default)]
pub struct Held(BTreeSet<(ShortcutAction, Source)>);
impl Held {
    /// A key went down or up. An action acts when it goes from no source
    /// holding it to one, and (holds only) back.
    pub fn set(&mut self, action: ShortcutAction, source: Source, down: bool) -> Option<Effect> {
        let was = self.down(action);
        let changed = if down {
            self.0.insert((action, source))
        } else {
            self.0.remove(&(action, source))
        };
        if !changed || was == self.down(action) {
            return None;
        }
        match action {
            ShortcutAction::PushToTalk => Some(Effect::PushToTalk(down)),
            ShortcutAction::PushToMute => Some(Effect::PushToMute(down)),
            ShortcutAction::ToggleMute => down.then_some(Effect::ToggleMute),
            ShortcutAction::ToggleDeafen => down.then_some(Effect::ToggleDeafen),
            // In-app actions are not held (see `app_action`).
            ShortcutAction::Find | ShortcutAction::Search | ShortcutAction::SearchAll => None,
        }
    }
    /// Everything one source held is released (focus lost, backend gone).
    pub fn release(&mut self, source: Source) -> Vec<Effect> {
        let actions: Vec<_> = self.held_by(source).collect();
        actions
            .into_iter()
            .filter_map(|action| self.set(action, source, false))
            .collect()
    }
    fn down(&self, action: ShortcutAction) -> bool {
        self.0.iter().any(|(held, _)| *held == action)
    }
    fn held_by(&self, source: Source) -> impl Iterator<Item = ShortcutAction> + '_ {
        self.0
            .iter()
            .filter(move |(_, by)| *by == source)
            .map(|(action, _)| *action)
    }
}

/// Recording, what is held, and the system-wide backend.
#[derive(Default)]
pub struct Shortcuts {
    /// The action whose key is being recorded.
    pub recording: Option<ShortcutAction>,
    pub held: Held,
    /// The bindings wanted system-wide, and their backend until it gives up.
    global: Option<(Bindings, Option<GlobalShortcuts>)>,
    /// The system-wide triggers as the desktop describes them, once bound.
    pub system: Option<BTreeMap<ShortcutAction, String>>,
    /// Why system-wide shortcuts are not in effect (yet), with the detail.
    pub status: Option<(String, String)>,
}

impl Shortcuts {
    /// The actions a key press or release in the window belongs to. A
    /// release ends what the window holds with that key, whatever the
    /// modifiers are by then.
    pub fn window_actions(
        &self,
        settings: &ClientSettings,
        keystroke: &Keystroke,
        down: bool,
    ) -> Vec<ShortcutAction> {
        active(settings)
            .into_iter()
            .filter(|(action, binding)| {
                parse(binding).is_some_and(|binding| {
                    if down {
                        matches(&binding, keystroke)
                    } else {
                        binding.key.eq_ignore_ascii_case(&keystroke.key)
                            && self.held_by_window(*action)
                    }
                })
            })
            .map(|(action, _)| action)
            .collect()
    }

    /// Modifier-only bindings ("control") follow the modifier state.
    pub fn window_modifiers(
        &mut self,
        settings: &ClientSettings,
        modifiers: &Modifiers,
    ) -> Vec<Effect> {
        active(settings)
            .into_iter()
            .filter_map(|(action, binding)| {
                let down = modifier_held(&parse(&binding)?, modifiers)?;
                self.held.set(action, Source::Window, down)
            })
            .collect()
    }

    fn held_by_window(&self, action: ShortcutAction) -> bool {
        self.held.held_by(Source::Window).any(|held| held == action)
    }

    /// Take new bindings and bind them system-wide, replacing the backend.
    /// `None` when nothing changed; otherwise every held key is released,
    /// since its release may no longer be recognised.
    pub fn rebind(
        &mut self,
        wanted: Bindings,
        handle: &tokio::runtime::Handle,
    ) -> Option<Vec<Effect>> {
        let current = self.global.as_ref().map(|(bound, _)| bound);
        if current == Some(&wanted) || (current.is_none() && wanted.is_empty()) {
            return None;
        }
        self.system = None;
        if wanted.is_empty() {
            self.global = None;
            self.status = None;
        } else {
            self.status = Some((
                "Requesting system-wide shortcuts…".into(),
                "The desktop may ask you to confirm them.".into(),
            ));
            let started = GlobalShortcuts::start(handle, wanted.clone());
            self.global = Some((wanted, Some(started)));
        }
        let mut released = self.held.release(Source::System);
        released.extend(self.held.release(Source::Window));
        Some(released)
    }

    /// Take in the system-wide backend's next report: what to do, or `None`
    /// when nothing is pending.
    pub fn poll_global(&mut self) -> Option<Vec<Effect>> {
        let global = &mut self.global.as_mut()?.1;
        let event = global.as_mut()?.try_next()?;
        Some(match event {
            GlobalEvent::Pressed(action) => self
                .held
                .set(action, Source::System, true)
                .into_iter()
                .collect(),
            GlobalEvent::Released(action) => self
                .held
                .set(action, Source::System, false)
                .into_iter()
                .collect(),
            GlobalEvent::Bound(triggers) => {
                self.system = Some(triggers);
                self.status = None;
                Vec::new()
            }
            GlobalEvent::Unavailable(reason) => {
                tracing::info!(%reason, "system-wide shortcuts are unavailable");
                *global = None;
                self.system = None;
                self.status = Some((UNAVAILABLE.into(), reason));
                self.held.release(Source::System)
            }
        })
    }

    pub fn polling(&self) -> bool {
        self.global
            .as_ref()
            .is_some_and(|(_, backend)| backend.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(text: &str) -> Keystroke {
        parse(text).unwrap()
    }

    #[test]
    fn in_app_actions_have_defaults_and_stay_out_of_the_desktop() {
        let mut settings = ClientSettings::default();
        let find = if cfg!(target_os = "macos") {
            "super-k"
        } else {
            "ctrl-k"
        };
        assert_eq!(binding(&settings, ShortcutAction::Find), Some(find));
        assert_eq!(
            app_action(&settings, &key(find)),
            Some(ShortcutAction::Find)
        );
        assert_eq!(binding(&settings, ShortcutAction::ToggleMute), None);
        // Rebound, and cleared on purpose (not back to the default).
        settings
            .shortcuts
            .insert(ShortcutAction::Find, "ctrl-p".into());
        assert_eq!(
            app_action(&settings, &key("ctrl-p")),
            Some(ShortcutAction::Find)
        );
        assert_eq!(app_action(&settings, &key(find)), None);
        settings
            .shortcuts
            .insert(ShortcutAction::Find, UNSET.into());
        assert_eq!(binding(&settings, ShortcutAction::Find), None);
        // Only voice actions are held and registered system-wide.
        settings
            .shortcuts
            .insert(ShortcutAction::ToggleMute, "ctrl-m".into());
        let registered = active(&settings);
        assert!(registered.iter().all(|(action, _)| action.system_wide()));
        assert!(
            registered
                .iter()
                .any(|(action, _)| *action == ShortcutAction::ToggleMute)
        );
        assert!(
            !registered
                .iter()
                .any(|(action, _)| *action == ShortcutAction::Search)
        );
    }

    #[test]
    fn parses_and_shows_bindings() {
        for binding in [
            "f13",
            "ctrl-shift-m",
            "alt-`",
            "super-space",
            "control",
            "ctrl--",
        ] {
            assert_eq!(notation(&key(binding)), binding);
        }
        // Older files and hand edits spell modifiers differently.
        assert_eq!(notation(&key("shift-ctrl-M")), "ctrl-shift-m");
        assert_eq!(notation(&key("cmd-k")), "super-k");
        assert_eq!(key("ctrl").key, "control");
        assert_eq!(display("ctrl-shift-m"), "Ctrl+Shift+M");
        assert_eq!(display("f13"), "F13");
        assert_eq!(display("space"), "Space");
        assert_eq!(display("control"), "Ctrl");
        assert_eq!(display("alt-pagedown"), "Alt+Page Down");
        assert!(parse("").is_none() && parse("ctrl-").is_none());
    }

    #[test]
    fn presses_match_key_and_modifiers() {
        assert!(matches(&key("ctrl-m"), &key("ctrl-m")));
        assert!(matches(&key("f13"), &key("F13")));
        assert!(!matches(&key("ctrl-m"), &key("m")));
        assert!(!matches(&key("m"), &key("ctrl-m")));
        assert!(!matches(&key("ctrl-m"), &key("ctrl-shift-m")));
    }

    #[test]
    fn toggles_act_once_per_press_and_holds_on_both_edges() {
        use ShortcutAction as A;
        use Source::{System, Window};
        let mut held = Held::default();
        assert_eq!(
            held.set(A::ToggleMute, Window, true),
            Some(Effect::ToggleMute)
        );
        // Key repeat, and the desktop reporting the same press.
        assert_eq!(held.set(A::ToggleMute, Window, true), None);
        assert_eq!(held.set(A::ToggleMute, System, true), None);
        assert_eq!(held.set(A::ToggleMute, Window, false), None);
        assert_eq!(held.set(A::ToggleMute, System, false), None);
        assert_eq!(
            held.set(A::ToggleMute, System, true),
            Some(Effect::ToggleMute)
        );
        assert_eq!(held.set(A::ToggleMute, System, false), None);
        // A release nobody pressed does nothing.
        assert_eq!(held.set(A::ToggleDeafen, Window, false), None);

        assert_eq!(
            held.set(A::PushToTalk, System, true),
            Some(Effect::PushToTalk(true))
        );
        assert_eq!(held.set(A::PushToTalk, Window, true), None);
        assert_eq!(held.set(A::PushToTalk, System, false), None);
        assert_eq!(
            held.set(A::PushToTalk, Window, false),
            Some(Effect::PushToTalk(false))
        );
        assert_eq!(
            held.set(A::PushToMute, Window, true),
            Some(Effect::PushToMute(true))
        );
        assert_eq!(
            held.set(A::ToggleDeafen, Window, true),
            Some(Effect::ToggleDeafen)
        );
        // Losing focus releases what the window held; toggles have no release.
        assert_eq!(held.release(Window), [Effect::PushToMute(false)]);
        assert_eq!(
            held.set(A::ToggleDeafen, Window, true),
            Some(Effect::ToggleDeafen)
        );
    }

    #[test]
    fn window_keys_follow_the_mode_and_release_by_key() {
        let mut settings = ClientSettings::default();
        settings
            .shortcuts
            .insert(ShortcutAction::PushToTalk, "f13".into());
        settings
            .shortcuts
            .insert(ShortcutAction::ToggleMute, "ctrl-m".into());
        settings
            .shortcuts
            .insert(ShortcutAction::PushToMute, "control".into());
        let mut shortcuts = Shortcuts::default();
        // Voice activity: push to talk does nothing, so F13 stays a key.
        assert!(
            shortcuts
                .window_actions(&settings, &key("f13"), true)
                .is_empty()
        );
        assert_eq!(
            shortcuts.window_actions(&settings, &key("ctrl-m"), true),
            [ShortcutAction::ToggleMute]
        );
        // A release counts only for what the window holds.
        assert!(
            shortcuts
                .window_actions(&settings, &key("m"), false)
                .is_empty()
        );
        shortcuts
            .held
            .set(ShortcutAction::ToggleMute, Source::Window, true);
        assert_eq!(
            shortcuts.window_actions(&settings, &key("m"), false),
            [ShortcutAction::ToggleMute]
        );
        let control = Modifiers::control();
        assert_eq!(
            shortcuts.window_modifiers(&settings, &control),
            [Effect::PushToMute(true)]
        );
        assert_eq!(shortcuts.window_modifiers(&settings, &control), []);
        assert_eq!(
            shortcuts.window_modifiers(&settings, &Modifiers::none()),
            [Effect::PushToMute(false)]
        );
        settings.audio.transmit = TransmitMode::PushToTalk;
        assert_eq!(
            shortcuts.window_actions(&settings, &key("f13"), true),
            [ShortcutAction::PushToTalk]
        );
        assert_eq!(shortcuts.window_modifiers(&settings, &control), []);
    }
}
