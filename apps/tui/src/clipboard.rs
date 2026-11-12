// SPDX-License-Identifier: EUPL-1.2
//! Explicit local clipboard MIME reads. Encoded PNG goes straight to the
//! existing sandboxed content worker; this process never decodes it.
use crate::app::App;
use nuschl_client::{
    Conversation, input::Input, model::TextTarget, platform::Request, server::Status,
};
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
    peer_certificate: Option<String>,
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
            peer_certificate: channel
                .direct_peer
                .as_ref()
                .and_then(|peer| peer.certificate_hash.clone())
                .filter(|value| !value.is_empty()),
        })
    }

    fn matches(&self, current: &Self) -> bool {
        if self.conversation != current.conversation || self.identity != current.identity {
            return false;
        }
        if self.peer_certificate.is_some() && self.identity.is_some() {
            return self.peer_certificate == current.peer_certificate;
        }
        self.generation == current.generation
            && self.peer == current.peer
            && (self.peer.is_some() || self.storage == current.storage)
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
        .clone()
        .unwrap_or_default()
        .inline_image_byte_budget())
}

#[derive(Default)]
pub struct Clipboard {
    pending: Option<PendingImage>,
    ready: Option<(Destination, EncodedImage)>,
    task: Option<tokio::task::JoinHandle<()>>,
    flight: Option<Flight>,
}

type PendingImage = (
    Destination,
    oneshot::Receiver<Result<(EncodedImage, &'static str), String>>,
);

struct Flight {
    server: String,
    generation: u64,
    target: TextTarget,
    body: String,
    caption: String,
    at: std::time::Instant,
}

impl Clipboard {
    #[cfg(test)]
    pub fn with_test_image(app: &App) -> Self {
        Self {
            pending: None,
            ready: Some((
                Destination::capture(app).unwrap(),
                EncodedImage {
                    html: "<img src=\"data:image/png;base64,dGVzdA==\" />".into(),
                    thumbnail: nuschl_core::ContentBlock::Image {
                        width: 1,
                        height: 1,
                        rgba: "AAAAAA==".into(),
                        source: None,
                    },
                },
            )),
            task: None,
            flight: None,
        }
    }
    pub fn label(&self) -> Option<&'static str> {
        if self.flight.is_some() {
            Some("Image sending…; retained until the connection acknowledges")
        } else if self.pending.is_some() {
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
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.pending = None;
        self.ready = None;
        self.flight = None;
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
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
        self.clipboard.task = Some(tokio::spawn(async move {
            let result = async {
                let worker = nuschl_client::native::content_worker()
                    .ok_or("The content worker is missing; the image was not attached.")?;
                let clipboard = crate::clipboard_backend::read().await?;
                let image = nuschl_ipc::host::encode_image(worker, &clipboard.bytes, budget as u32)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok((image, clipboard.provider))
            }
            .await;
            let _ = reply.send(result);
        }));
        self.say("Reading a PNG image from the local clipboard…");
    }

    pub fn poll_clipboard(&mut self) {
        if self.clipboard.flight.as_ref().is_some_and(|flight| {
            flight.at.elapsed() > Duration::from_secs(15)
                || self
                    .client
                    .servers
                    .iter()
                    .find(|server| server.id == flight.server)
                    .is_none_or(|server| {
                        server.generation != flight.generation || server.status != Status::Connected
                    })
        }) {
            self.clipboard.flight = None;
            self.warn("Image send was not acknowledged; image and caption are retained. Check the conversation before retrying (a retry may duplicate it).");
        }
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
            self.clipboard.task = None;
            match result {
                Ok((image, provider)) => {
                    self.clipboard.ready = Some((destination, image));
                    self.say(format!("Clipboard image attached ({provider}). Enter sends it; /discard-image removes it."));
                }
                Err(reason) => self.warn(format!("Image not attached: {reason}")),
            }
        }
    }

    /// One image per explicit paste. The destination is frozen at attach,
    /// not inferred from the currently open conversation after an await.
    pub fn send_image(&mut self, caption: &str) {
        if self.client.sending() {
            return self.warn("The previous message is still on its way.");
        }
        if self.clipboard.flight.is_some() {
            return self.warn("The image is still awaiting the connection's send acknowledgment.");
        }
        let Some((destination, image)) = self.clipboard.ready.as_ref() else {
            return self.warn(if self.clipboard.pending.is_some() {
                "The clipboard image is still being prepared."
            } else {
                "Paste an image first (/paste-image)."
            });
        };
        let destination = destination.clone();
        if !Destination::capture(self).is_ok_and(|current| destination.matches(&current)) {
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
        let peer = server.channels[channel_index].direct_peer.as_ref();
        let same_peer = if let Some(certificate) = &destination.peer_certificate {
            peer.and_then(|peer| peer.certificate_hash.as_ref()) == Some(certificate)
        } else {
            peer.map(|peer| peer.instance) == destination.peer
        };
        if !same_peer {
            return self.warn("The image's recipient changed. Discard it and paste again.");
        }
        if image.html.len() > limit {
            return self
                .warn("The server's image limit changed; discard the image and paste again.");
        }
        let mut body = String::new();
        if !caption.trim().is_empty() {
            let caption = nuschl_session::connection::legacy_body(caption, true);
            if let Err(reason) = server
                .limits
                .clone()
                .unwrap_or_default()
                .check_text_wire(&caption)
            {
                return self.warn(reason.to_string());
            }
            body.push_str(&caption);
            body.push_str("<br/>");
        }
        body.push_str(&image.html);
        if body.len() > limit {
            return self.warn("The caption and image exceed the server's image limit; shorten the caption or send it separately.");
        }
        let flight = Flight {
            server: server.id.clone(),
            generation: server.generation,
            target: target.clone(),
            body: body.clone(),
            caption: caption.to_string(),
            at: std::time::Instant::now(),
        };
        match self.client.request(
            server_index,
            Request::SendHtml {
                target,
                html: vec![body],
                expected_peer,
            },
        ) {
            Ok(()) => {
                self.clipboard.flight = Some(flight);
                self.say("Image queued; attachment and caption remain until the connection acknowledges the send.");
            }
            Err(reason) => self.warn(format!("Image was not sent: {reason}")),
        }
    }

    pub fn clipboard_input(&mut self, input: &Input) {
        let Some(flight) = &self.clipboard.flight else {
            return;
        };
        if let Input::Sent {
            server,
            generation,
            target,
            body,
        } = input
            && *server == flight.server
            && *generation == flight.generation
            && *target == flight.target
            && *body == flight.body
            && self.client.servers.iter().any(|view| {
                view.id == *server
                    && view.generation == *generation
                    && view.status == Status::Connected
            })
        {
            let caption = flight.caption.clone();
            let destination = self
                .clipboard
                .ready
                .as_ref()
                .map(|(destination, _)| destination.conversation.clone());
            self.clipboard.flight = None;
            self.clipboard.ready = None;
            if let Some(destination) = destination {
                self.acknowledge_composer(&destination, &caption);
            }
            self.say("Image sent to the server.");
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ReadError {
    Timeout,
    Other(String),
}

impl ReadError {
    pub(super) fn message(self) -> String {
        match self {
            Self::Timeout => "The clipboard read timed out; no image was attached.".into(),
            Self::Other(reason) => reason,
        }
    }
}

pub(super) async fn png_from(program: &str, arguments: &[&str]) -> Result<Vec<u8>, ReadError> {
    let mut child = Command::new("prlimit")
        .args(["--as=268435456", "--cpu=3", "--", program])
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| {
            ReadError::Other(format!(
                "{program} is unavailable; install it to read PNG clipboard images."
            ))
        })?;
    let mut output = child
        .stdout
        .take()
        .ok_or_else(|| ReadError::Other("Clipboard helper has no output.".into()))?;
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
    .await;
    let result = match result {
        Ok(result) => result.map_err(ReadError::Other),
        Err(_) => Err(ReadError::Timeout),
    };
    if result.is_err() {
        let _ = child.kill().await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn discard_aborts_the_owned_clipboard_task() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let proof = dropped.clone();
        let (entered, started) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _guard = Dropped(proof);
            let _ = entered.send(());
            std::future::pending::<()>().await;
        });
        let mut clipboard = Clipboard::default();
        clipboard.task = Some(task);
        started.await.unwrap();
        clipboard.discard();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!clipboard.attached());
    }
}
