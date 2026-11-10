// SPDX-License-Identifier: EUPL-1.2
//! System-wide push-to-talk. On Linux the XDG GlobalShortcuts portal binds the
//! shortcut (the desktop asks the user to confirm it); Windows and macOS use the
//! platform hotkey APIs. Only press and release notifications cross into Nuschel.
use tokio::runtime::Handle;
#[cfg(target_os = "linux")]
use tokio::sync::mpsc;

#[derive(Debug)]
pub enum GlobalEvent {
    Pressed,
    Released,
    /// The shortcut is active, with the desktop's description of its trigger.
    Bound(String),
    Unavailable(String),
}

/// The system-wide shortcut for as long as this value lives.
pub struct GlobalPtt {
    queued: std::collections::VecDeque<GlobalEvent>,
    #[cfg(target_os = "linux")]
    events: mpsc::Receiver<GlobalEvent>,
    #[cfg(target_os = "linux")]
    task: tokio::task::JoinHandle<()>,
    /// Windows and macOS deliver hotkeys through the platform event loop of the
    /// thread that registered them, so the manager stays on GPUI's main thread.
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    hotkey: Option<(
        global_hotkey::GlobalHotKeyManager,
        global_hotkey::hotkey::HotKey,
    )>,
}

impl Drop for GlobalPtt {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        self.task.abort();
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        if let Some((manager, hotkey)) = self.hotkey.take() {
            let _ = manager.unregister(hotkey);
        }
    }
}

/// GPUI key names (e.g. "f13", "space") as XDG shortcut triggers ("F13", "space").
pub fn xdg_trigger(key: &str) -> String {
    let lower = key.to_ascii_lowercase();
    match lower.as_str() {
        "control" | "ctrl" => "CTRL".into(),
        "alt" => "ALT".into(),
        "shift" => "SHIFT".into(),
        "super" | "platform" | "cmd" => "LOGO".into(),
        "enter" => "Return".into(),
        "escape" => "Escape".into(),
        "backspace" => "BackSpace".into(),
        "pageup" => "Page_Up".into(),
        "pagedown" => "Page_Down".into(),
        "`" => "grave".into(),
        key if key.starts_with('f') && key[1..].parse::<u8>().is_ok() => key.to_ascii_uppercase(),
        key => key.into(),
    }
}

impl GlobalPtt {
    /// Call on the main (UI) thread.
    pub fn start(handle: &Handle, key: String) -> Self {
        #[cfg(target_os = "linux")]
        {
            let (sender, events) = mpsc::channel(16);
            let task = handle.spawn(async move {
                if let Err(error) = run(key, sender.clone()).await {
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
            let mut queued = std::collections::VecDeque::new();
            let hotkey = match register(&key) {
                Ok(registered) => {
                    queued.push_back(GlobalEvent::Bound(key));
                    Some(registered)
                }
                Err(error) => {
                    queued.push_back(GlobalEvent::Unavailable(error));
                    None
                }
            };
            Self { queued, hotkey }
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
        {
            let _ = (handle, key);
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
            let (_, hotkey) = self.hotkey.as_ref()?;
            while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
                if event.id == hotkey.id() {
                    return Some(match event.state {
                        HotKeyState::Pressed => GlobalEvent::Pressed,
                        HotKeyState::Released => GlobalEvent::Released,
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
async fn run(key: String, sender: mpsc::Sender<GlobalEvent>) -> Result<(), String> {
    use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
    use futures_util::StreamExt;
    const ID: &str = "push-to-talk";
    let portal = GlobalShortcuts::new()
        .await
        .map_err(|error| format!("global shortcuts portal unavailable ({error})"))?;
    let session = portal
        .create_session(Default::default())
        .await
        .map_err(|error| error.to_string())?;
    let mut activated = portal
        .receive_activated()
        .await
        .map_err(|error| error.to_string())?;
    let mut deactivated = portal
        .receive_deactivated()
        .await
        .map_err(|error| error.to_string())?;
    let trigger = xdg_trigger(&key);
    let shortcut =
        NewShortcut::new(ID, "Push to talk in Nuschel").preferred_trigger(Some(trigger.as_str()));
    let bound = portal
        .bind_shortcuts(&session, &[shortcut], None, Default::default())
        .await
        .and_then(|request| request.response())
        .map_err(|error| format!("the desktop did not bind the shortcut ({error})"))?;
    let description = bound
        .shortcuts()
        .iter()
        .find(|shortcut| shortcut.id() == ID)
        .map(|shortcut| shortcut.trigger_description().to_string())
        .filter(|description| !description.is_empty())
        .unwrap_or(trigger);
    let _ = sender.send(GlobalEvent::Bound(description)).await;
    loop {
        let event = tokio::select! {
            Some(event) = activated.next() => (event.shortcut_id() == ID).then_some(GlobalEvent::Pressed),
            Some(event) = deactivated.next() => (event.shortcut_id() == ID).then_some(GlobalEvent::Released),
            else => return Err("the global shortcuts portal closed".into()),
        };
        if let Some(event) = event
            && sender.send(event).await.is_err()
        {
            return Ok(());
        }
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn register(
    key: &str,
) -> Result<
    (
        global_hotkey::GlobalHotKeyManager,
        global_hotkey::hotkey::HotKey,
    ),
    String,
> {
    use global_hotkey::{GlobalHotKeyManager, hotkey::HotKey};
    let hotkey: HotKey = xdg_trigger(key)
        .replace("CTRL", "Control")
        .replace("LOGO", "Super")
        .parse()
        .map_err(|error| format!("unsupported key {key} ({error})"))?;
    let manager = GlobalHotKeyManager::new().map_err(|error| error.to_string())?;
    manager
        .register(hotkey)
        .map_err(|error| error.to_string())?;
    Ok((manager, hotkey))
}

#[cfg(test)]
mod tests {
    use super::xdg_trigger;
    #[test]
    fn maps_gpui_keys_to_xdg_triggers() {
        assert_eq!(xdg_trigger("f13"), "F13");
        assert_eq!(xdg_trigger("control"), "CTRL");
        assert_eq!(xdg_trigger("space"), "space");
        assert_eq!(xdg_trigger("`"), "grave");
    }
}
