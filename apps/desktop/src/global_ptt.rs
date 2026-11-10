// SPDX-License-Identifier: EUPL-1.2
//! System-wide push-to-talk. On Linux the XDG GlobalShortcuts portal binds the
//! shortcut (the desktop asks the user to confirm it); Windows and macOS use the
//! platform hotkey APIs. Only press and release notifications cross into Nuschel.
use tokio::{runtime::Handle, sync::mpsc};

#[derive(Debug)]
pub enum GlobalEvent {
    Pressed,
    Released,
    /// The shortcut is active, with the desktop's description of its trigger.
    Bound(String),
    Unavailable(String),
}

pub struct GlobalPtt {
    pub events: mpsc::Receiver<GlobalEvent>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for GlobalPtt {
    fn drop(&mut self) {
        self.task.abort();
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
    pub fn start(handle: &Handle, key: String) -> Self {
        let (sender, events) = mpsc::channel(16);
        let task = handle.spawn(async move {
            if let Err(error) = run(key, sender.clone()).await {
                let _ = sender.send(GlobalEvent::Unavailable(error)).await;
            }
        });
        Self { events, task }
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
async fn run(key: String, sender: mpsc::Sender<GlobalEvent>) -> Result<(), String> {
    use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState, hotkey::HotKey};
    let hotkey: HotKey = xdg_trigger(&key)
        .replace("CTRL", "Control")
        .replace("LOGO", "Super")
        .parse()
        .map_err(|error| format!("unsupported key {key} ({error})"))?;
    let manager = GlobalHotKeyManager::new().map_err(|error| error.to_string())?;
    manager
        .register(hotkey)
        .map_err(|error| error.to_string())?;
    let _ = sender.send(GlobalEvent::Bound(key)).await;
    let receiver = GlobalHotKeyEvent::receiver();
    loop {
        while let Ok(event) = receiver.try_recv() {
            if event.id == hotkey.id() {
                let next = match event.state {
                    HotKeyState::Pressed => GlobalEvent::Pressed,
                    HotKeyState::Released => GlobalEvent::Released,
                };
                if sender.send(next).await.is_err() {
                    let _ = manager.unregister(hotkey);
                    return Ok(());
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
async fn run(_: String, _: mpsc::Sender<GlobalEvent>) -> Result<(), String> {
    Err("global shortcuts are not supported on this platform".into())
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
