// SPDX-License-Identifier: EUPL-1.2
//! The full screen on the terminal: raw mode on the alternate screen, with
//! focus changes (a conversation is read only while the terminal is in
//! front, where the terminal reports it) and bracketed paste. The terminal
//! is restored on the way out, also after a panic.
use crate::{app::App, view};
use ratatui::{
    DefaultTerminal,
    crossterm::{
        event::{
            self, DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste,
            EnableFocusChange, Event,
        },
        execute,
    },
};

fn restore() {
    let _ = execute!(std::io::stdout(), DisableBracketedPaste, DisableFocusChange);
    ratatui::restore();
}

/// Run the full screen until the user quits.
pub async fn run(
    mut app: App,
    mut events: tokio::sync::mpsc::Receiver<nuschl_client::native::Event>,
) -> std::io::Result<()> {
    let mut terminal: DefaultTerminal = ratatui::try_init()?;
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        hook(info);
    }));
    execute!(std::io::stdout(), EnableBracketedPaste, EnableFocusChange)?;
    // Query before the event reader owns stdin. Unsupported terminals keep
    // text descriptions, and --plain never emits graphics queries.
    if let Ok(picker) = ratatui_image::picker::Picker::from_query_stdio() {
        app.graphics.borrow_mut().configure(picker);
    }
    let (keys, mut input) = tokio::sync::mpsc::unbounded_channel::<Event>();
    // crossterm reads blocking; its own thread forwards what it reads.
    std::thread::spawn(move || {
        while let Ok(event) = event::read() {
            if keys.send(event).is_err() {
                break;
            }
        }
    });
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
    let mut media_tick = tokio::time::interval(std::time::Duration::from_millis(100));
    let result = loop {
        if let Err(error) = terminal.draw(|frame| view::draw(frame, &mut app)) {
            break Err(error);
        }
        if app.quit {
            break Ok(());
        }
        tokio::select! {
            Some(event) = events.recv() => {
                let mut batch = vec![event];
                while batch.len() < 256 && let Ok(event) = events.try_recv() {
                    batch.push(event);
                }
                app.events(batch);
            }
            Some(event) = input.recv() => match event {
                Event::Key(key) => app.key(key),
                Event::Paste(text) => app.paste(&text),
                Event::FocusGained => app.focus(true),
                Event::FocusLost => app.focus(false),
                _ => {}
            },
            _ = tick.tick() => app.tick(),
            _ = media_tick.tick() => app.poll_media(),
        }
    };
    app.tick();
    restore();
    result
}
