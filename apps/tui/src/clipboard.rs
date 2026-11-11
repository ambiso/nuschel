// SPDX-License-Identifier: EUPL-1.2
//! Explicit local clipboard MIME reads. Encoded PNG goes straight to the
//! existing sandboxed content worker; this process never decodes it.
use crate::app::App;
use nuschl_client::{Conversation, platform::Request, server::Status};
use nuschl_ipc::EncodedImage;
use std::{process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command, sync::oneshot};

const MAX_BYTES: u64 = 8 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Destination {
    conversation: Conversation,
    generation: u64,
    identity: Option<String>,
    storage: String,
    peer: Option<u64>,
}

impl Destination {
    fn capture(app: &App) -> Result<Self, &'static str> {
        let (server, channel) = app
            .selected()
            .ok_or("Open a conversation before pasting an image.")?;
        let server = &app.client.servers[server];
        image_limit(server)?;
        let channel = &server.channels[channel];
        Ok(Self {
            conversation: Conversation {
                server: server.id.clone(),
                channel: channel.id,
            },
            generation: server.generation,
            identity: server.history_identity(),
            storage: channel.storage_key.clone(),
            peer: channel.direct_peer.as_ref().map(|peer| peer.instance),
        })
    }
}

fn image_limit(server: &nuschl_client::server::ServerState) -> Result<usize, &'static str> {
    if server.status != Status::Connected {
        return Err("Connect before pasting or sending an image.");
    }
    if server.mls {
        return Err("Image attachments are not supported on encrypted servers yet.");
    }
    if server
        .limits
        .as_ref()
        .is_some_and(|limits| !limits.allow_html)
    {
        return Err("This server does not accept images.");
    }
    Ok(server
        .limits
        .as_ref()
        .map_or(131_072, |limits| limits.image_message_length)
        .min(nuschl_core::MAX_CONTENT_BYTES))
}

#[derive(Default)]
pub struct Clipboard {
    pending: Option<(Destination, oneshot::Receiver<Result<EncodedImage, String>>)>,
    ready: Option<(Destination, EncodedImage)>,
}

impl Clipboard {
    pub fn label(&self) -> Option<&'static str> {
        if self.pending.is_some() {
            Some("Reading clipboard image…")
        } else if self.ready.is_some() {
            Some("1 image attached; Enter sends, /discard-image removes")
        } else {
            None
        }
    }
    pub fn attached(&self) -> bool {
        self.pending.is_some() || self.ready.is_some()
    }
    pub fn discard(&mut self) {
        self.pending = None;
        self.ready = None;
    }
}

impl App {
    pub fn paste_image(&mut self) {
        if self.options.ssh {
            return self.warn("Image paste over SSH needs terminal-mediated image MIME support; the remote machine's OS clipboard is not read.");
        }
        if !cfg!(target_os = "linux") {
            return self.warn(
                "Local clipboard image paste is currently supported on Linux (Wayland or X11).",
            );
        }
        if self.editing.is_some() {
            return self.warn("Finish or cancel the edit before attaching an image.");
        }
        if self.clipboard.attached() {
            return self.warn(
                "An image is already attached or being prepared; /discard-image removes it.",
            );
        }
        let destination = match Destination::capture(self) {
            Ok(destination) => destination,
            Err(reason) => return self.warn(reason),
        };
        let Some((server, _)) = self.selected() else {
            return;
        };
        let limit = image_limit(&self.client.servers[server]).unwrap_or(0);
        let budget = limit.saturating_sub(1024).max(limit / 2);
        let (reply, answer) = oneshot::channel();
        self.clipboard.pending = Some((destination, answer));
        tokio::spawn(async move {
            let result = async {
                let worker = nuschl_client::native::content_worker()
                    .ok_or("The content worker is missing; the image was not attached.")?;
                let bytes = read_local_png().await?;
                nuschl_ipc::host::encode_image(worker, &bytes, budget as u32)
                    .await
                    .map_err(|error| error.to_string())
            }
            .await;
            let _ = reply.send(result);
        });
        self.say("Reading a PNG image from the local clipboard…");
    }

    pub fn poll_clipboard(&mut self) {
        let result =
            self.clipboard
                .pending
                .as_mut()
                .and_then(|(_, answer)| match answer.try_recv() {
                    Ok(result) => Some(result),
                    Err(oneshot::error::TryRecvError::Closed) => {
                        Some(Err("The clipboard image task stopped.".into()))
                    }
                    Err(oneshot::error::TryRecvError::Empty) => None,
                });
        if let Some(result) = result
            && let Some((destination, _)) = self.clipboard.pending.take()
        {
            match result {
                Ok(image) => {
                    self.clipboard.ready = Some((destination, image));
                    self.say(
                        "Clipboard image attached. Enter sends it; /discard-image removes it.",
                    );
                }
                Err(reason) => self.warn(format!("Image not attached: {reason}")),
            }
        }
    }

    /// One image per explicit paste. The destination is frozen at attach,
    /// not inferred from the currently open conversation after an await.
    pub fn send_image(&mut self, caption: &str) {
        let Some((destination, image)) = self.clipboard.ready.as_ref() else {
            return self.warn("The clipboard image is still being prepared.");
        };
        let destination = destination.clone();
        if Destination::capture(self).as_ref() != Ok(&destination) {
            return self.warn("The image's conversation or connection changed. Return to it or /discard-image and paste again.");
        }
        let Some((server_index, channel_index)) = self.selected() else {
            return;
        };
        let server = &mut self.client.servers[server_index];
        let limit = match image_limit(server) {
            Ok(limit) => limit,
            Err(reason) => return self.warn(reason),
        };
        let (target, expected_peer) = match server.message_target(channel_index) {
            Ok(target) => target,
            Err(reason) => return self.warn(reason),
        };
        // Resolving a direct recipient can refresh its presence. Never let it
        // retarget the image while preparing it or changing conversations.
        if server.channels[channel_index]
            .direct_peer
            .as_ref()
            .map(|peer| peer.instance)
            != destination.peer
        {
            return self.warn("The image's recipient changed. Discard it and paste again.");
        }
        if image.html.len() > limit {
            return self
                .warn("The server's image limit changed; discard the image and paste again.");
        }
        let mut html = Vec::new();
        if !caption.trim().is_empty() {
            let caption = nuschl_session::connection::legacy_body(caption, true);
            let message_limit = server
                .limits
                .as_ref()
                .map_or(5000, |limits| limits.message_length);
            if caption.len() > message_limit {
                return self.warn("The caption exceeds the server's text limit.");
            }
            html.push(caption);
        }
        html.push(image.html.clone());
        match self.client.request(
            server_index,
            Request::SendHtml {
                target,
                html,
                expected_peer,
            },
        ) {
            Ok(()) => {
                self.clipboard.ready = None;
                self.composer.take();
                self.composer_changed();
                self.say("Image queued for this conversation.");
            }
            Err(reason) => self.warn(format!("Image was not sent: {reason}")),
        }
    }
}

async fn read_local_png() -> Result<Vec<u8>, String> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return png_from("wl-paste", &["--type", "image/png", "--no-newline"]).await;
    }
    if std::env::var_os("DISPLAY").is_some() {
        return png_from(
            "xclip",
            &["-selection", "clipboard", "-target", "image/png", "-out"],
        )
        .await;
    }
    Err("No local Wayland or X11 display is available.".into())
}

async fn png_from(program: &str, arguments: &[&str]) -> Result<Vec<u8>, String> {
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| {
            format!("{program} is unavailable; install it to read PNG clipboard images.")
        })?;
    let mut output = child
        .stdout
        .take()
        .ok_or("Clipboard helper has no output.")?;
    let result = tokio::time::timeout(DEADLINE, async {
        let mut bytes = Vec::new();
        (&mut output)
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|error| error.to_string())?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("The clipboard image exceeds 8 MiB.".into());
        }
        let status = child.wait().await.map_err(|error| error.to_string())?;
        if !status.success() || bytes.is_empty() {
            return Err("The clipboard does not provide a PNG image.".into());
        }
        Ok(bytes)
    })
    .await
    .unwrap_or_else(|_| Err("The clipboard read timed out.".into()));
    if result.is_err() {
        let _ = child.kill().await;
    }
    result
}
