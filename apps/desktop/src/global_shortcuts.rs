// SPDX-License-Identifier: EUPL-1.2
//! System-wide shortcuts. On Linux the XDG GlobalShortcuts portal binds them
//! in one session (the desktop may ask the user once, and may pick other
//! triggers); Windows and macOS use the platform hotkey APIs. Only press and
//! release notifications cross into Nuschl.
use nuschl_session::config::ShortcutAction;
use std::collections::BTreeMap;
use tokio::runtime::Handle;
#[cfg(target_os = "linux")]
use tokio::sync::mpsc;

/// Bindings in GPUI keystroke notation.
pub type Bindings = Vec<(ShortcutAction, String)>;

#[derive(Debug)]
pub enum GlobalEvent {
    Pressed(ShortcutAction),
    Released(ShortcutAction),
    /// The shortcuts in effect, each with the desktop's description of its
    /// trigger. A requested shortcut that is missing was not granted.
    Bound(BTreeMap<ShortcutAction, String>),
    Unavailable(String),
}

/// The system-wide shortcuts for as long as this value lives.
pub struct GlobalShortcuts {
    queued: std::collections::VecDeque<GlobalEvent>,
    #[cfg(target_os = "linux")]
    events: mpsc::Receiver<GlobalEvent>,
    #[cfg(target_os = "linux")]
    task: tokio::task::JoinHandle<()>,
    /// Windows and macOS deliver hotkeys through the platform event loop of the
    /// thread that registered them, so the manager stays on GPUI's main thread.
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    hotkeys: Option<(
        global_hotkey::GlobalHotKeyManager,
        Vec<(global_hotkey::hotkey::HotKey, ShortcutAction)>,
    )>,
}

impl Drop for GlobalShortcuts {
    fn drop(&mut self) {
        // The portal session ends with the task's own D-Bus connection.
        #[cfg(target_os = "linux")]
        self.task.abort();
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        if let Some((manager, hotkeys)) = self.hotkeys.take() {
            let hotkeys: Vec<_> = hotkeys.into_iter().map(|(hotkey, _)| hotkey).collect();
            let _ = manager.unregister_all(&hotkeys);
        }
    }
}

/// A binding as an XDG shortcut trigger: "ctrl-shift-m" is "CTRL+SHIFT+m".
#[cfg(any(target_os = "linux", test))]
pub fn xdg_trigger(binding: &str) -> Option<String> {
    let keystroke = crate::shortcuts::parse(binding)?;
    let modifiers = keystroke.modifiers;
    let mut trigger: String = [
        (modifiers.control, "CTRL+"),
        (modifiers.alt, "ALT+"),
        (modifiers.shift, "SHIFT+"),
        (modifiers.platform, "LOGO+"),
    ]
    .into_iter()
    .filter_map(|(held, name)| held.then_some(name))
    .collect();
    // The key is an xkbcommon keysym name.
    trigger.push_str(match keystroke.key.as_str() {
        "control" => "Control_L",
        "alt" => "Alt_L",
        "shift" => "Shift_L",
        "platform" => "Super_L",
        "enter" => "Return",
        "escape" => "Escape",
        "backspace" => "BackSpace",
        "tab" => "Tab",
        "delete" => "Delete",
        "insert" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pageup" => "Page_Up",
        "pagedown" => "Page_Down",
        "left" => "Left",
        "right" => "Right",
        "up" => "Up",
        "down" => "Down",
        "`" => "grave",
        "-" => "minus",
        "=" => "equal",
        "[" => "bracketleft",
        "]" => "bracketright",
        "\\" => "backslash",
        ";" => "semicolon",
        "'" => "apostrophe",
        "," => "comma",
        "." => "period",
        "/" => "slash",
        key if key.starts_with('f') && key[1..].parse::<u8>().is_ok() => {
            return Some(trigger + &key.to_ascii_uppercase());
        }
        key => key,
    });
    Some(trigger)
}

/// GNOME describes a trigger as a GTK accelerator, "Press <Shift><Control>m":
/// that reads "Shift+Ctrl+M". Other descriptions stay as the desktop wrote them.
#[cfg(any(target_os = "linux", test))]
fn readable_trigger(description: &str) -> String {
    let Some(mut rest) = description.strip_prefix("Press ") else {
        return description.to_string();
    };
    let mut parts = Vec::new();
    while let Some((modifier, tail)) = rest.strip_prefix('<').and_then(|rest| rest.split_once('>'))
    {
        parts.push(match modifier {
            "Control" | "Primary" => "Ctrl",
            "Super" | "Meta" => "Super",
            modifier => modifier,
        });
        rest = tail;
    }
    if rest.is_empty() || rest.contains(['<', '>', ' ']) {
        return description.to_string();
    }
    let key = if rest.chars().count() == 1 {
        rest.to_uppercase()
    } else {
        rest.to_string()
    };
    parts.push(&key);
    parts.join("+")
}

/// A binding in the hotkey crate's notation: "ctrl-shift-m" is "control+shift+m".
/// It has no modifier-only hotkeys.
#[cfg(any(target_os = "windows", target_os = "macos", test))]
pub fn hotkey_text(binding: &str) -> Option<String> {
    let keystroke = crate::shortcuts::parse(binding)?;
    if keystroke.is_modifier_key() {
        return None;
    }
    let modifiers = keystroke.modifiers;
    let mut text: String = [
        (modifiers.control, "control+"),
        (modifiers.alt, "alt+"),
        (modifiers.shift, "shift+"),
        (modifiers.platform, "super+"),
    ]
    .into_iter()
    .filter_map(|(held, name)| held.then_some(name))
    .collect();
    text.push_str(&keystroke.key);
    Some(text)
}

impl GlobalShortcuts {
    /// Call on the main (UI) thread.
    pub fn start(handle: &Handle, bindings: Bindings) -> Self {
        #[cfg(target_os = "linux")]
        {
            let (sender, events) = mpsc::channel(16);
            let task = handle.spawn(async move {
                if let Err(error) = run(bindings, sender.clone()).await {
                    let _ = sender.send(GlobalEvent::Unavailable(error)).await;
                }
            });
            Self {
                queued: Default::default(),
                events,
                task,
            }
        }
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        {
            let _ = handle;
            let (hotkeys, event) = match register(&bindings) {
                Ok((manager, registered, triggers)) => {
                    (Some((manager, registered)), GlobalEvent::Bound(triggers))
                }
                Err(error) => (None, GlobalEvent::Unavailable(error)),
            };
            Self {
                queued: [event].into(),
                hotkeys,
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
        {
            let _ = (handle, bindings);
            Self {
                queued: [GlobalEvent::Unavailable(
                    "global shortcuts are not supported on this platform".into(),
                )]
                .into(),
            }
        }
    }

    /// The next press, release or status change, without blocking.
    pub fn try_next(&mut self) -> Option<GlobalEvent> {
        if let Some(event) = self.queued.pop_front() {
            return Some(event);
        }
        #[cfg(target_os = "linux")]
        {
            self.events.try_recv().ok()
        }
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        {
            use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
            let (_, registered) = self.hotkeys.as_ref()?;
            while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
                if let Some((_, action)) = registered.iter().find(|(key, _)| key.id() == event.id) {
                    return Some(match event.state {
                        HotKeyState::Pressed => GlobalEvent::Pressed(*action),
                        HotKeyState::Released => GlobalEvent::Released(*action),
                    });
                }
            }
            None
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
        {
            None
        }
    }
}

#[cfg(target_os = "linux")]
async fn run(bindings: Bindings, sender: mpsc::Sender<GlobalEvent>) -> Result<(), String> {
    use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut, Shortcut};
    use futures_util::StreamExt;
    let unavailable =
        |error: ashpd::Error| format!("global shortcuts portal unavailable ({error})");
    // A connection of its own: the portal learns an unsandboxed program's app
    // ID only before anything else is asked on a connection, and closing the
    // connection ends the session when the bindings change.
    let connection = zbus::Connection::session()
        .await
        .map_err(|error| format!("no session bus ({error})"))?;
    if let Ok(app_id) = ashpd::AppID::try_from("org.nuschl.Nuschl") {
        // Refused where the portal already knows the app (Flatpak, or a
        // desktop launcher's systemd scope); an older portal lacks it.
        if let Err(error) =
            ashpd::register_host_app_with_connection(connection.clone(), app_id).await
        {
            tracing::debug!(%error, "the portal did not register the app ID");
        }
    }
    let portal = GlobalShortcuts::with_connection(connection)
        .await
        .map_err(unavailable)?;
    let session = portal
        .create_session(Default::default())
        .await
        .map_err(unavailable)?;
    // ashpd keeps the session's object path to itself, but serializes as it.
    let session_path = serde_json::to_value(&session)
        .ok()
        .and_then(|path| path.as_str().map(str::to_owned))
        .unwrap_or_default();
    let mut activated = portal.receive_activated().await.map_err(unavailable)?;
    let mut deactivated = portal.receive_deactivated().await.map_err(unavailable)?;
    let mut changed = portal
        .receive_shortcuts_changed()
        .await
        .map_err(unavailable)?;
    let requested: Vec<_> = bindings
        .iter()
        .map(|(action, binding)| {
            NewShortcut::new(action.id(), crate::shortcuts::label(*action))
                .preferred_trigger(xdg_trigger(binding).as_deref())
        })
        .collect();
    let bound = portal
        .bind_shortcuts(&session, &requested, None, Default::default())
        .await
        .and_then(|request| request.response())
        .map_err(|error| format!("the desktop did not bind the shortcuts ({error})"))?;
    // The desktop describes the triggers it chose; without a description, the
    // preferred one stands.
    let mut triggers = BTreeMap::new();
    let mut learn = |shortcuts: &[Shortcut]| {
        for shortcut in shortcuts {
            let Some((action, binding)) = bindings
                .iter()
                .find(|(action, _)| action.id() == shortcut.id())
            else {
                continue;
            };
            let description = match shortcut.trigger_description() {
                "" => crate::shortcuts::display(binding),
                description => readable_trigger(description),
            };
            triggers.insert(*action, description);
        }
        GlobalEvent::Bound(triggers.clone())
    };
    if sender.send(learn(bound.shortcuts())).await.is_err() {
        return Ok(());
    }
    let ours = |path: zbus::zvariant::ObjectPath<'_>, id: &str| {
        (path.as_str() == session_path)
            .then(|| ShortcutAction::from_id(id))
            .flatten()
    };
    loop {
        let event = tokio::select! {
            Some(event) = activated.next() => ours(event.session_handle(), event.shortcut_id()).map(GlobalEvent::Pressed),
            Some(event) = deactivated.next() => ours(event.session_handle(), event.shortcut_id()).map(GlobalEvent::Released),
            Some(event) = changed.next() => (event.session_handle().as_str() == session_path).then(|| learn(event.shortcuts())),
            else => return Err("the global shortcuts portal closed".into()),
        };
        if let Some(event) = event
            && sender.send(event).await.is_err()
        {
            return Ok(());
        }
    }
}

/// Register each binding the platform accepts; the rest work while Nuschl is focused.
#[cfg(any(target_os = "windows", target_os = "macos"))]
#[allow(clippy::type_complexity)]
fn register(
    bindings: &Bindings,
) -> Result<
    (
        global_hotkey::GlobalHotKeyManager,
        Vec<(global_hotkey::hotkey::HotKey, ShortcutAction)>,
        BTreeMap<ShortcutAction, String>,
    ),
    String,
> {
    use global_hotkey::{GlobalHotKeyManager, hotkey::HotKey};
    let manager = GlobalHotKeyManager::new().map_err(|error| error.to_string())?;
    let mut registered = Vec::new();
    let mut triggers = BTreeMap::new();
    for (action, binding) in bindings {
        let Some(hotkey) = hotkey_text(binding).and_then(|text| text.parse::<HotKey>().ok()) else {
            continue;
        };
        match manager.register(hotkey) {
            Ok(()) => {
                registered.push((hotkey, *action));
                triggers.insert(*action, crate::shortcuts::display(binding));
            }
            Err(error) => tracing::info!(%error, binding, "could not register a global hotkey"),
        }
    }
    Ok((manager, registered, triggers))
}

#[cfg(test)]
mod tests {
    use super::{hotkey_text, readable_trigger, xdg_trigger};
    #[test]
    fn maps_bindings_to_xdg_triggers() {
        assert_eq!(xdg_trigger("f13").as_deref(), Some("F13"));
        assert_eq!(xdg_trigger("control").as_deref(), Some("Control_L"));
        assert_eq!(xdg_trigger("space").as_deref(), Some("space"));
        assert_eq!(xdg_trigger("`").as_deref(), Some("grave"));
        assert_eq!(xdg_trigger("ctrl-shift-m").as_deref(), Some("CTRL+SHIFT+m"));
        assert_eq!(
            xdg_trigger("super-alt-pageup").as_deref(),
            Some("ALT+LOGO+Page_Up")
        );
        assert_eq!(xdg_trigger("ctrl--").as_deref(), Some("CTRL+minus"));
        assert_eq!(xdg_trigger(""), None);
    }
    #[test]
    fn reads_desktop_trigger_descriptions() {
        assert_eq!(readable_trigger("Press <Shift><Control>m"), "Shift+Ctrl+M");
        assert_eq!(readable_trigger("Press F13"), "F13");
        assert_eq!(readable_trigger("Press <Super>space"), "Super+space");
        assert_eq!(readable_trigger("Ctrl+Shift+M"), "Ctrl+Shift+M");
        assert_eq!(readable_trigger("Press <Control>"), "Press <Control>");
    }
    #[test]
    fn maps_bindings_to_hotkeys() {
        assert_eq!(hotkey_text("f13").as_deref(), Some("f13"));
        assert_eq!(
            hotkey_text("ctrl-alt-shift-super-m").as_deref(),
            Some("control+alt+shift+super+m")
        );
        assert_eq!(hotkey_text("control"), None);
    }
}
