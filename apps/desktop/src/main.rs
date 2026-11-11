// SPDX-License-Identifier: EUPL-1.2
// A GUI program on Windows: without this, starting Nuschel from Explorer, the
// Start menu or the Run key (autostart) opens an empty console window as well.
// Command-line use goes through nuschel-cli.
#![cfg_attr(windows, windows_subsystem = "windows")]
mod assets;
mod autostart;
mod backend;
mod chrome;
mod dialogs;
mod document;
mod emoji;
mod global_ptt;
mod images;
mod menus;
mod notify;
mod panels;
mod preview;
mod scrolling;
mod settings;
mod state;
mod theme;
mod update;
mod view;
mod viewer;

use std::{cell::RefCell, path::PathBuf, rc::Rc};

use clap::{Parser, Subcommand};
use gpui_kit::{
    AppContext, Bounds, TitlebarOptions, WindowBounds, WindowDecorations, WindowOptions, point, px,
    size,
};
use nuschel_session::config::Profile;

#[derive(Parser)]
#[command(version, about = "Nuschel GPUI desktop foundation")]
struct Arguments {
    #[arg(long, global = true)]
    profile_dir: Option<PathBuf>,
    /// Connect on startup: `mumble://[user@]host[:port][/channel/path]`.
    #[arg(value_name = "URL")]
    url: Option<String>,
    /// Started at login: open minimised.
    #[arg(long)]
    autostart: bool,
    #[command(subcommand)]
    command: Option<Action>,
}

#[derive(Subcommand)]
enum Action {
    /// Render literal text and fenced code through the content worker, without opening a window.
    Render { text: String },
    /// Exercise the exact Linux content sandbox with a trusted local executable.
    #[cfg(target_os = "linux")]
    #[command(hide = true)]
    SandboxProbe {
        executable: PathBuf,
        #[arg(trailing_var_arg = true)]
        arguments: Vec<std::ffi::OsString>,
    },
    /// Run the media worker in the sandbox a call gives it, speaking its
    /// protocol on this process's standard input and output (audio tests).
    #[cfg(target_os = "linux")]
    #[command(hide = true)]
    MediaSandbox,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = Arguments::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nuschel=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    #[cfg(target_os = "linux")]
    if let Some(Action::SandboxProbe {
        executable,
        arguments,
    }) = arguments.command
    {
        let mut command = nuschel_ipc::host::content_sandbox_command(&executable, &arguments)?;
        let status = runtime.block_on(async { command.status().await })?;
        return if status.success() {
            Ok(())
        } else {
            Err("sandbox probe failed".into())
        };
    }
    #[cfg(target_os = "linux")]
    if let Some(Action::MediaSandbox) = arguments.command {
        let worker = nuschel_session::workers::sibling("nuschel-media-worker")?;
        let mut command = nuschel_ipc::host::media_sandbox_command(&worker)?;
        let status = runtime.block_on(async { command.status().await })?;
        std::process::exit(status.code().unwrap_or(1));
    }

    if let Some(Action::Render { text }) = arguments.command {
        let worker = backend::content_worker().ok_or("cannot locate the content worker")?;
        let document = runtime.block_on(nuschel_ipc::host::render_content(worker, &text))?;
        println!("{}", serde_json::to_string(&document)?);
        return Ok(());
    }

    let profile = Profile::open(arguments.profile_dir)?;
    update::remember_state_in(&profile.config_dir);
    let bookmarks = profile.load_servers()?;
    let startup = arguments
        .url
        .as_deref()
        .map(nuschel_session::config::MumbleUrl::parse)
        .transpose()?;
    let runtime_handle = runtime.handle().clone();
    // GPUI owns the event loop; preserve startup errors until it returns so failed
    // launches also report failure to the invoking shell or process supervisor.
    let launch_error = Rc::new(RefCell::new(None));
    let window_error = Rc::clone(&launch_error);
    gpui_kit::application()
        .with_assets(assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            // Input carets and selections resolve from Base's theme, not inherited text colors.
            let theme = gpui_kit::base::Theme::global_mut(cx);
            theme.appearance = gpui_kit::base::ThemeAppearance::Dark;
            theme.tokens.colors = gpui_kit::base::ColorTokens::dark();
            theme.tokens.colors.accent = gpui_kit::rgb(0x557ee6).into();
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1280.), px(820.)),
                    cx,
                ))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Nuschel".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(14.), px(12.))),
                }),
                // GNOME and several other compositors do not draw decorations; Nuschel
                // draws a consistent title bar with window controls on Linux.
                window_decorations: Some(if cfg!(target_os = "linux") {
                    WindowDecorations::Client
                } else {
                    WindowDecorations::Server
                }),
                window_min_size: Some(size(px(860.), px(520.))),
                // The desktop entry's name: shells use it to find the icon.
                app_id: Some("org.nuschel.Nuschel".into()),
                // Only X11 takes the icon from the window. Windows loads it
                // from the executable (build.rs), macOS from the bundle.
                #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                icon: assets::window_icon(cx),
                ..Default::default()
            };
            let minimised = arguments.autostart;
            if let Err(error) = gpui_kit::open_window(options, cx, |window, cx| {
                window.set_window_title("Nuschel");
                if minimised {
                    window.minimize_window();
                }
                cx.new(|cx| {
                    state::Desktop::new(profile, bookmarks, startup, runtime_handle, window, cx)
                })
            }) {
                *window_error.borrow_mut() =
                    Some(format!("could not open the desktop window: {error}"));
                // Linux starts its event loop after this launch callback. Quitting
                // here would be cleared when that loop starts; queue it instead.
                cx.spawn(async |cx| {
                    cx.update(|cx| cx.quit());
                })
                .detach();
            }
        });
    if let Some(error) = launch_error.take() {
        return Err(error.into());
    }
    Ok(())
}
