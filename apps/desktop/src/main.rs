// SPDX-License-Identifier: EUPL-1.2
mod view;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use gpui_kit::{AppContext, WindowOptions};
use nuschel_client::ClientPreferences;
use nuschel_core::{ContentBlock, MessageDocument, PreviewPolicy};

const EXAMPLE: &str = "Welcome to Nuschel.\nNewlines stay newlines.\n\n```rust\nfn main() {\n    println!(\"Hello, Mumble!\");\n}\n```\n\nThis desktop shell keeps content parsing in a separate worker.\nThe original message stays available when its preview is collapsed.\n\nOne\nTwo\nThree\nFour\nFive\nSix\nSeven\nEight\nNine\nTen";

#[derive(Parser)]
#[command(version, about = "Nuschel GPUI desktop foundation")]
struct Arguments {
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
        .with_env_filter("nuschel=info")
        .with_writer(std::io::stderr)
        .init();
    let runtime = tokio::runtime::Builder::new_current_thread()
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

    // A fixed development message is processed before starting the GUI event loop.
    // Live messages will use asynchronous bounded requests through the session adapter.
    let document = runtime
        .block_on(nuschel_ipc::host::render_content(&worker, EXAMPLE))
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "content worker unavailable; showing literal text");
            MessageDocument {
                blocks: vec![ContentBlock::Text {
                    text: EXAMPLE.into(),
                }],
            }
        });
    let has_more = PreviewPolicy::default().preview(EXAMPLE, false).has_more;
    let preferences = ClientPreferences::default();
    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        if let Err(error) = gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            window.set_window_title("Nuschel");
            cx.new(|_| view::Desktop {
                document,
                expanded: false,
                has_more,
                history_enabled: preferences.save_local_history,
            })
        }) {
            tracing::error!(%error, "could not open the desktop window");
            cx.quit();
        }
    });
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
