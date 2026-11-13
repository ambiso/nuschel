// SPDX-License-Identifier: EUPL-1.2
//! The desktop's own background work, beside the SDK's driver: the Settings
//! audio check, update checks and installs, website icons, the image
//! viewer's larger decodes, and images attached for sending. Each reports a
//! [`ServiceEvent`] for the desktop to apply.
use nuschl_client::native::content_worker;
use nuschl_core::MessageDocument;
use nuschl_session::config::ClientSettings;
use tokio::{
    runtime::Handle,
    sync::{mpsc, watch},
};

pub enum ServiceEvent {
    LinkPreviewPolicy {
        revision: u64,
        result: Result<nuschl_session::config::LinkPreviewSettings, String>,
    },
    LinkPreviewImage {
        ticket: crate::link_preview::ImageTicket,
        pixels: Option<nuschl_ipc::preview::Pixels>,
    },
    /// The local audio check in Settings.
    Preview(crate::preview::Event),
    Update(crate::update::UpdateEvent),
    /// A server's website icon, decoded by the content worker (`None`: none found).
    ServerIcon {
        server: String,
        icon: Option<nuschl_core::ContentBlock>,
    },
    /// The image viewer's larger decode (`None` if the worker refused it).
    Viewed {
        request: u64,
        document: Option<MessageDocument>,
    },
    /// An attached image re-encoded for sending, or why it cannot be sent.
    Prepared {
        id: u64,
        result: Result<nuschl_ipc::EncodedImage, String>,
    },
}

pub struct Services {
    handle: Handle,
    sender: mpsc::Sender<ServiceEvent>,
    settings: watch::Receiver<ClientSettings>,
    image_permits: std::sync::Arc<tokio::sync::Semaphore>,
    preview: Option<(
        mpsc::Sender<crate::preview::Command>,
        tokio::task::JoinHandle<()>,
    )>,
}

impl Services {
    /// One composer-owned physical save at a time. The blocking operation
    /// owns only a local Profile clone and finishes before reporting credit.
    pub fn save_link_preview_policy(
        &self,
        profile: nuschl_session::config::Profile,
        action: crate::link_preview::PolicyAction,
    ) {
        let events = self.sender.clone();
        self.handle.spawn(async move {
            let revision = action.revision;
            let result =
                tokio::task::spawn_blocking(move || profile.save_link_previews(&action.settings))
                    .await
                    .map_err(|_| "Link preview settings could not be saved.".to_owned())
                    .and_then(|result| {
                        result.map_err(|_| "Link preview settings could not be saved.".to_owned())
                    });
            let _ = events
                .send(ServiceEvent::LinkPreviewPolicy { revision, result })
                .await;
        });
    }
    /// Exact carried bytes are decoded only in the confined content worker.
    /// The composer admits one such request until its completion is received.
    pub fn render_link_preview(
        &self,
        ticket: crate::link_preview::ImageTicket,
        image: nuschl_core::preview::EmbeddedImage,
    ) {
        let events = self.sender.clone();
        self.handle.spawn(async move {
            let pixels = match content_worker() {
                Some(worker) => match nuschl_ipc::host::preview(
                    &worker,
                    nuschl_ipc::preview::Request::RenderImage {
                        schema: nuschl_ipc::preview::SCHEMA,
                        image,
                    },
                )
                .await
                {
                    Ok(nuschl_ipc::preview::Response::Pixels {
                        schema: nuschl_ipc::preview::SCHEMA,
                        pixels,
                    }) if pixels.is_bounded() => Some(pixels),
                    _ => None,
                },
                None => None,
            };
            let _ = events
                .send(ServiceEvent::LinkPreviewImage { ticket, pixels })
                .await;
        });
    }
    /// The services, following the driver's preferences, and the events
    /// they report.
    pub fn new(
        handle: Handle,
        settings: watch::Receiver<ClientSettings>,
    ) -> (Self, mpsc::Receiver<ServiceEvent>) {
        let (sender, events) = mpsc::channel(64);
        let services = Self {
            handle,
            sender,
            settings,
            image_permits: std::sync::Arc::new(tokio::sync::Semaphore::new(2)),
            preview: None,
        };
        (services, events)
    }
    /// Start the Settings audio check (devices only until the microphone test starts).
    pub fn start_preview(&mut self) {
        if self.preview.is_some() {
            return;
        }
        let (commands, receiver) = mpsc::channel(8);
        let task = self.handle.spawn(crate::preview::run(
            receiver,
            self.settings.clone(),
            self.sender.clone(),
        ));
        self.preview = Some((commands, task));
    }
    pub fn preview(&self, command: crate::preview::Command) {
        if let Some((commands, _)) = &self.preview {
            let _ = commands.try_send(command);
        }
    }
    /// Closing the sender lets the task shut its worker down cleanly.
    pub fn stop_preview(&mut self) {
        self.preview.take();
    }
    /// Decode an inline image at the viewer's size in the sandboxed content worker.
    /// Look for the server website's icon (see `icons`).
    pub fn fetch_icon(&self, server: String, host: String) {
        let events = self.sender.clone();
        self.handle.spawn(async move {
            let bytes = tokio::task::spawn_blocking(move || crate::icons::fetch(&host))
                .await
                .ok()
                .flatten();
            let icon = match (bytes, content_worker()) {
                (Some(bytes), Some(worker)) => nuschl_ipc::host::decode_icon(worker, &bytes)
                    .await
                    .ok()
                    .and_then(|document| document.blocks.into_iter().next()),
                _ => None,
            };
            let _ = events.send(ServiceEvent::ServerIcon { server, icon }).await;
        });
    }
    pub fn decode_view(&self, request: u64, uri: String) {
        let events = self.sender.clone();
        self.handle.spawn(async move {
            let document = match content_worker() {
                Some(worker) => nuschl_ipc::host::decode_view(worker, &uri).await.ok(),
                None => None,
            };
            let _ = events
                .send(ServiceEvent::Viewed { request, document })
                .await;
        });
    }
    /// Ask the release server for a newer version now.
    pub fn check_update(&self) {
        let events = self.sender.clone();
        self.handle.spawn(async move {
            let result = tokio::task::spawn_blocking(crate::update::check)
                .await
                .unwrap_or_else(|error| Err(error.to_string()));
            let _ = events
                .send(ServiceEvent::Update(crate::update::UpdateEvent::Checked(
                    result,
                )))
                .await;
        });
    }
    /// Check shortly after start and then periodically, while the setting is on.
    pub fn schedule_update_checks(&self) {
        let events = self.sender.clone();
        let settings = self.settings.clone();
        self.handle.spawn(async move {
            let mut delay = crate::update::FIRST_CHECK;
            loop {
                tokio::time::sleep(delay).await;
                delay = crate::update::INTERVAL;
                if !settings.borrow().check_updates {
                    continue;
                }
                let result = tokio::task::spawn_blocking(crate::update::check)
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()));
                if events
                    .send(ServiceEvent::Update(crate::update::UpdateEvent::Checked(
                        result,
                    )))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
    }
    /// Download, verify and swap in an update.
    pub fn install_update(&self, update: crate::update::Update) {
        let events = self.sender.clone();
        self.handle.spawn(async move {
            let progress = events.clone();
            let result = tokio::task::spawn_blocking(move || {
                let mut last = 0;
                crate::update::install(&update, |done, total| {
                    // About every 5% (try_send: progress may be dropped).
                    if done == total || done.saturating_sub(last) > total / 20 {
                        last = done;
                        let _ = progress.try_send(ServiceEvent::Update(
                            crate::update::UpdateEvent::Progress { done, total },
                        ));
                    }
                })
            })
            .await
            .unwrap_or_else(|error| Err(error.to_string()));
            let _ = events
                .send(ServiceEvent::Update(crate::update::UpdateEvent::Installed(
                    result,
                )))
                .await;
        });
    }
    /// Re-encode an attached image as stock Mumble sends one: in the
    /// sandboxed content worker, into an `<img>` of at most `limit` bytes.
    /// Two at a time; a file is read only when its turn comes.
    pub fn prepare_image(&self, id: u64, source: crate::attachments::Source, limit: usize) {
        let events = self.sender.clone();
        let permits = self.image_permits.clone();
        self.handle.spawn(async move {
            let result = async {
                let _permit = permits.acquire().await.map_err(|error| error.to_string())?;
                let worker =
                    content_worker().ok_or("images need the content worker, which is missing")?;
                let image = match source {
                    crate::attachments::Source::Bytes(bytes) => bytes,
                    crate::attachments::Source::File(path) => {
                        tokio::task::spawn_blocking(move || crate::attachments::read(&path))
                            .await
                            .map_err(|error| error.to_string())??
                    }
                };
                if image.len() as u64 > crate::attachments::MAX_IMAGE_BYTES {
                    return Err("it is larger than 8 MB".to_owned());
                }
                match nuschl_ipc::host::encode_image(worker, &image, limit as u32).await {
                    Ok(encoded) => Ok(encoded),
                    Err(nuschl_ipc::host::Error::Rejected(reason)) => Err(reason),
                    Err(error) => Err(error.to_string()),
                }
            }
            .await;
            let _ = events.send(ServiceEvent::Prepared { id, result }).await;
        });
    }
}
