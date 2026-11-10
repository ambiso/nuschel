// SPDX-License-Identifier: EUPL-1.2
mod assets;
mod backend;
mod chrome;
mod dialogs;
mod document;
mod menus;
mod panels;
mod scrolling;
mod settings;
mod state;
mod theme;
mod view;

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

    let worker = sibling_worker()?;
    if let Some(Action::Render { text }) = arguments.command {
        let document = runtime.block_on(nuschel_ipc::host::render_content(&worker, &text))?;
        println!("{}", serde_json::to_string(&document)?);
        return Ok(());
    }

    let profile = Profile::open(arguments.profile_dir)?;
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
                app_id: Some("org.nuschel.Nuschel".into()),
                ..Default::default()
            };
            if let Err(error) = gpui_kit::open_window(options, cx, |window, cx| {
                window.set_window_title("Nuschel");
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

fn sibling_worker() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let executable = std::env::current_exe()?;
    let directory = executable
        .parent()
        .ok_or("executable has no parent directory")?;
    Ok(directory.join(format!(
        "nuschel-content-worker{}",
        std::env::consts::EXE_SUFFIX
    )))
}
