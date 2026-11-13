// SPDX-License-Identifier: EUPL-1.2
// A GUI program on Windows: without this, starting Nuschl from Explorer, the
// Start menu or the Run key (autostart) opens an empty console window as well.
// Command-line use goes through nuschl-cli.
#![cfg_attr(windows, windows_subsystem = "windows")]
mod acl_editor;
mod appearance;
mod assets;
mod attachments;
mod autostart;
mod bitrate;
mod chrome;
mod dialogs;
mod document;
mod document_links;
mod editing;
mod emoji;
mod finder;
#[cfg(feature = "frame-timing")]
mod frame_timing;
mod global_shortcuts;
mod icons;
mod identity;
mod images;
mod layout;
mod math;
mod menus;
mod motion;
mod names;
mod notify;
mod panels;
mod presentation;
mod preview;
mod rail;
mod scrolling;
mod search;
mod selection;
mod services;
mod settings;
mod shortcuts;
mod slider;
mod state;
mod theme;
mod timeline_keys;
mod transfer;
mod typing;
mod unread;
mod update;
mod version_notice;
mod view;
mod viewer;
mod voice;

use std::{cell::RefCell, path::PathBuf, rc::Rc};

use clap::{Parser, Subcommand};
use gpui_kit::{AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, point, px, size};
use nuschl_session::config::Profile;

#[derive(Parser)]
#[command(version, about = "Nuschl GPUI desktop foundation")]
struct Arguments {
    #[arg(long, global = true)]
    profile_dir: Option<PathBuf>,
    /// Connect on startup: `mumble://[user@]host[:port][/channel/path]`, or a
    /// share link (`nuschl://join/<token>#…`, `https://<web host>/join/…`).
    #[arg(value_name = "URL")]
    url: Option<String>,
    /// Started at login: open minimised.
    #[arg(long)]
    autostart: bool,
    /// Offer this file in the conversation the URL opens, once connected
    /// (magic-wormhole); repeat for more files.
    #[arg(long = "offer", value_name = "FILE")]
    offers: Vec<PathBuf>,
    /// Who should draw the window's frame on Linux; overrides
    /// NUSCHL_WINDOW_DECORATIONS. Only a preference: on Wayland the compositor
    /// decides, and with `auto` Nuschl states none.
    #[arg(long, value_enum, value_name = "WHO")]
    window_decorations: Option<chrome::DecorationChoice>,
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
    /// Run the transfer worker in the sandbox an offer (`--file`) or a
    /// download gives it, speaking its protocol on this process's standard
    /// input and output (transfer tests).
    #[cfg(target_os = "linux")]
    #[command(hide = true)]
    TransferSandbox {
        #[arg(long)]
        file: Option<PathBuf>,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = Arguments::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nuschl=info".into()),
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
        let mut command = nuschl_ipc::host::content_sandbox_command(&executable, &arguments)?;
        let status = runtime.block_on(async { command.status().await })?;
        return if status.success() {
            Ok(())
        } else {
            Err("sandbox probe failed".into())
        };
    }
    #[cfg(target_os = "linux")]
    if let Some(Action::MediaSandbox) = arguments.command {
        let worker = nuschl_session::workers::sibling("nuschl-media-worker")?;
        let mut command = nuschl_ipc::host::media_sandbox_command(&worker)?;
        let status = runtime.block_on(async { command.status().await })?;
        std::process::exit(status.code().unwrap_or(1));
    }

    #[cfg(target_os = "linux")]
    if let Some(Action::TransferSandbox { file }) = &arguments.command {
        let worker = nuschl_session::workers::sibling("nuschl-transfer-worker")?;
        let mut command = nuschl_ipc::host::transfer_sandbox_command(&worker, file.as_deref())?;
        let status = runtime.block_on(async { command.status().await })?;
        std::process::exit(status.code().unwrap_or(1));
    }

    if let Some(Action::Render { text }) = arguments.command {
        let worker =
            nuschl_client::native::content_worker().ok_or("cannot locate the content worker")?;
        let document = runtime.block_on(nuschl_ipc::host::render_content(worker, &text))?;
        println!("{}", serde_json::to_string(&document)?);
        return Ok(());
    }

    let default_profile = arguments.profile_dir.is_none();
    update::remember_profile(arguments.profile_dir.as_deref())?;
    let profile = Profile::open(arguments.profile_dir)?;
    if default_profile && let Err(error) = autostart::migrate_legacy() {
        tracing::warn!(%error, "could not migrate the old login entry");
    }
    for moved in profile.moved() {
        tracing::info!("{moved}");
    }
    update::remember_state(profile.update_state_path());
    let loaded = profile.load();
    for problem in &loaded.problems {
        tracing::warn!("{problem}");
    }
    let startup = arguments
        .url
        .as_deref()
        .map(nuschl_session::config::MumbleUrl::parse)
        .transpose()?;
    let decorations = arguments.window_decorations.unwrap_or_else(|| {
        chrome::DecorationChoice::from_environment()
            .unwrap_or_else(|problem| {
                tracing::warn!("{problem}");
                None
            })
            .unwrap_or_default()
    });
    let runtime_handle = runtime.handle().clone();
    // GPUI owns the event loop; preserve startup errors until it returns so failed
    // launches also report failure to the invoking shell or process supervisor.
    let launch_error = Rc::new(RefCell::new(None));
    let window_error = Rc::clone(&launch_error);
    gpui_kit::application()
        .with_assets(assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            emoji::install();
            appearance::bind_keys(cx);
            shortcuts::bind_text_editing(cx);
            #[cfg(feature = "frame-timing")]
            frame_timing::init();
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
                    title: Some("Nuschl".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(14.), px(12.))),
                }),
                // A request, not a decision: the compositor decides who decorates.
                //
                // Wayland: GPUI sends this through xdg-decoration (`set_mode`, or
                // `unset_mode` for no preference), and the compositor answers in its
                // `configure`, which may say CLIENT-SIDE whatever was asked, at startup
                // or at any later time. GPUI then reports `Decorations::Client` and
                // Nuschl draws its own frame, window controls and resize edges (see
                // `chrome::frame`, which reads `window.window_decorations()` on every
                // render). Without the xdg-decoration protocol (GNOME, Weston) the
                // window is client-side from the start. When the answer is server-side,
                // Nuschl draws no frame or window controls.
                //
                // The default, `auto`, states no preference on Wayland: a compositor
                // honours an explicit client-side request even where its user asked
                // apps to omit their decorations (niri's `prefer-no-csd`), and an
                // explicit server-side one overrides compositors that prefer the app
                // to draw them. On X11 it asks for client-side (see
                // `DecorationChoice::request`).
                window_decorations: Some(decorations.request(cx.compositor_name())),
                window_min_size: Some(size(px(860.), px(520.))),
                // GPUI draws inactive windows at 30 fps at most, and GNOME takes the
                // keyboard focus for the length of an interactive resize: every resize
                // would run at half the display's rate. Nuschl draws only when
                // something changed, so an inactive window costs nothing at rest.
                inactive_frame_interval: None,
                // The desktop entry's name: shells use it to find the icon.
                app_id: Some("org.nuschl.Nuschl".into()),
                // Only X11 takes the icon from the window. Windows loads it
                // from the executable (build.rs), macOS from the bundle.
                #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                icon: assets::window_icon(cx),
                ..Default::default()
            };
            let minimised = arguments.autostart;
            let offers = arguments.offers;
            if let Err(error) = gpui_kit::open_window(options, cx, |window, cx| {
                window.set_window_title("Nuschl");
                if minimised {
                    window.minimize_window();
                }
                cx.new(|cx| {
                    let mut desktop =
                        state::Desktop::new(profile, loaded, startup, runtime_handle, window, cx);
                    desktop.offer_when_connected(offers, cx);
                    desktop
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
