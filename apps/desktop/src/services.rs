// SPDX-License-Identifier: EUPL-1.2
//! The desktop's own background work, beside the SDK's driver: the Settings
//! audio check, update checks and installs, website icons, the image
//! viewer's larger decodes, and images attached for sending. Each reports a
//! [`ServiceEvent`] for the desktop to apply.
use nuschl_client::native::content_worker;
use nuschl_core::MessageDocument;
use nuschl_session::config::ClientSettings;
use std::sync::{Arc, Mutex};
use tokio::{
    runtime::Handle,
    sync::{mpsc, watch},
};

pub enum ServiceEvent {
    /// Wake only: a bounded watch mailbox holds the most recent completion.
    LinkPreviewPolicy,
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

#[derive(Clone)]
pub struct PolicyReply {
    pub revision: u64,
    pub result: Result<nuschl_session::config::LinkPreviewSettings, String>,
}

struct PolicySave {
    profile: nuschl_session::config::Profile,
    action: crate::link_preview::PolicyAction,
}

#[derive(Default)]
struct PolicyQueue {
    running: bool,
    latest_revision: u64,
    pending: Option<PolicySave>,
}

impl PolicyQueue {
    fn enqueue(&mut self, save: PolicySave) -> Result<bool, ()> {
        if save.action.revision <= self.latest_revision {
            return Err(());
        }
        self.latest_revision = save.action.revision;
        self.pending = Some(save);
        let start = !self.running;
        self.running = true;
        Ok(start)
    }

    fn take(&mut self) -> Option<PolicySave> {
        let save = self.pending.take();
        if save.is_none() {
            self.running = false;
        }
        save
    }
}

/// No GUI await or notification backpressure between saves. The owned
/// blocking task drains its one replaceable latest job even after GUI drop.
fn save_preview_policies(
    queue: Arc<Mutex<PolicyQueue>>,
    replies: watch::Sender<Option<PolicyReply>>,
) {
    drain_preview_policies(queue, replies, |save| {
        save.profile.save_link_previews(&save.action.settings)
    });
}

fn drain_preview_policies(
    queue: Arc<Mutex<PolicyQueue>>,
    replies: watch::Sender<Option<PolicyReply>>,
    mut save: impl FnMut(&PolicySave) -> std::io::Result<nuschl_session::config::LinkPreviewSettings>,
) {
    loop {
        let next = match queue.lock() {
            Ok(mut queue) => queue.take(),
            // A poisoned owner stays fail closed; never start replacement IO.
            Err(_) => return,
        };
        let Some(next) = next else {
            return;
        };
        // The mutex is not held during local disk IO or completion publication.
        let result =
            save(&next).map_err(|_| "Link preview settings could not be saved.".to_owned());
        replies.send_replace(Some(PolicyReply {
            revision: next.action.revision,
            result,
        }));
    }
}

pub struct Services {
    handle: Handle,
    sender: mpsc::Sender<ServiceEvent>,
    settings: watch::Receiver<ClientSettings>,
    image_permits: std::sync::Arc<tokio::sync::Semaphore>,
    policy_queue: Arc<Mutex<PolicyQueue>>,
    policy_completed: watch::Sender<Option<PolicyReply>>,
    policy_results: watch::Receiver<Option<PolicyReply>>,
    preview: Option<(
        mpsc::Sender<crate::preview::Command>,
        tokio::task::JoinHandle<()>,
    )>,
}

impl Services {
    /// Immediate bounded handoff. The background owner, not GUI replies,
    /// serializes all writes through the latest accepted preference.
    pub fn save_link_preview_policy(
        &self,
        profile: nuschl_session::config::Profile,
        action: crate::link_preview::PolicyAction,
    ) -> Result<(), &'static str> {
        let start = self
            .policy_queue
            .lock()
            .map_err(|_| "Link preview writer is unavailable.")?
            .enqueue(PolicySave { profile, action })
            .map_err(|_| "Link preview preference was superseded.")?;
        if start {
            let queue = self.policy_queue.clone();
            let replies = self.policy_completed.clone();
            self.handle
                .spawn_blocking(move || save_preview_policies(queue, replies));
        }
        Ok(())
    }

    pub fn link_preview_policy_result(&mut self) -> Option<PolicyReply> {
        self.policy_results.borrow_and_update().as_ref().cloned()
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
                    worker,
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
        let (policy_completed, policy_results) = watch::channel(None);
        let mut policy_wakes = policy_results.clone();
        let wake_sender = sender.clone();
        handle.spawn(async move {
            while policy_wakes.changed().await.is_ok() {
                // Notifications can wait on the GUI; the disk writer cannot.
                drop(policy_wakes.borrow_and_update());
                if wake_sender
                    .send(ServiceEvent::LinkPreviewPolicy)
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
        let services = Self {
            handle,
            sender,
            settings,
            image_permits: std::sync::Arc::new(tokio::sync::Semaphore::new(2)),
            policy_queue: Arc::new(Mutex::new(PolicyQueue::default())),
            policy_completed,
            policy_results,
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

#[cfg(test)]
mod preview_policy_tests {
    use super::{PolicyQueue, PolicySave, drain_preview_policies};
    use crate::link_preview::PolicyAction;
    use nuschl_client::preview::{Mode, Settings};
    use nuschl_session::config::Profile;
    use std::{
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
            mpsc,
        },
        time::Duration,
    };
    use tokio::sync::watch;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(1);
            let root = std::env::temp_dir().join(format!(
                "nuschl-preview-policy-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = std::fs::DirBuilder::new();
                builder.mode(0o700);
                builder.create(&root).unwrap();
            }
            // Never reuse or remove a path whose ownership was not established.
            #[cfg(not(unix))]
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn job(profile: &Profile, revision: u64, mode: Mode) -> PolicySave {
        PolicySave {
            profile: profile.clone(),
            action: PolicyAction {
                revision,
                settings: Settings {
                    mode,
                    ..Settings::default()
                },
            },
        }
    }

    #[test]
    fn real_background_writer_persists_latest_off_after_gui_drop_and_toggle_storm() {
        let scratch = Scratch::new();
        let profile = Profile::open(Some(scratch.0.clone())).unwrap();
        let queue = Arc::new(Mutex::new(PolicyQueue::default()));
        assert!(
            queue
                .lock()
                .unwrap()
                .enqueue(job(&profile, 1, Mode::Direct))
                .unwrap()
        );
        let (replies, gui) = watch::channel(None);
        let (entered, started) = mpsc::sync_channel(0);
        let (release, held) = mpsc::sync_channel(0);
        let writes = Arc::new(Mutex::new(Vec::new()));
        let physical = writes.clone();
        let owned = queue.clone();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed = finished.clone();
        let worker = runtime.spawn_blocking(move || {
            let mut first = true;
            drain_preview_policies(owned, replies, |save| {
                if first {
                    first = false;
                    entered.send(()).unwrap();
                    held.recv_timeout(Duration::from_secs(10)).unwrap();
                }
                physical.lock().unwrap().push(save.action.settings.mode);
                // Actual Profile atomic write/fsync/reload, not a fake result.
                save.profile.save_link_previews(&save.action.settings)
            });
            completed.store(true, Ordering::Release);
        });
        started.recv_timeout(Duration::from_secs(10)).unwrap();
        for revision in 2_u64..=1001 {
            let mode = if revision == 1001 || revision.is_multiple_of(2) {
                Mode::Off
            } else {
                Mode::ProxyRequired
            };
            assert!(
                !queue
                    .lock()
                    .unwrap()
                    .enqueue(job(&profile, revision, mode))
                    .unwrap(),
                "a second physical writer was requested while the first IO was held"
            );
        }
        assert_eq!(
            queue
                .lock()
                .unwrap()
                .pending
                .as_ref()
                .unwrap()
                .action
                .revision,
            1001
        );
        // No GUI receiver or GUI callback is left to start a replacement save.
        // The background owner must still write Off after the held Direct IO.
        drop(gui);
        drop(queue);
        release.send(()).unwrap();
        drop(worker);
        // Graceful runtime teardown must wait for this single blocking owner;
        // it must not require an async callback or GUI event between writes.
        drop(runtime);
        assert!(
            finished.load(Ordering::Acquire),
            "background owner panicked or failed to finish at runtime teardown"
        );
        assert_eq!(*writes.lock().unwrap(), vec![Mode::Direct, Mode::Off]);
        assert_eq!(profile.load_link_previews().user.settings.mode, Mode::Off);
    }

    #[test]
    fn completion_mailbox_retains_latest_without_gui_backpressure() {
        let scratch = Scratch::new();
        let profile = Profile::open(Some(scratch.0.clone())).unwrap();
        let queue = Arc::new(Mutex::new(PolicyQueue::default()));
        assert!(
            queue
                .lock()
                .unwrap()
                .enqueue(job(&profile, 1, Mode::Direct))
                .unwrap()
        );
        // Replacing an unstarted job still starts only the already-owned task.
        assert!(
            !queue
                .lock()
                .unwrap()
                .enqueue(job(&profile, 2, Mode::Off))
                .unwrap()
        );
        let (replies, mut gui) = watch::channel(None);
        drain_preview_policies(queue.clone(), replies, |save| {
            save.profile.save_link_previews(&save.action.settings)
        });
        let reply = gui.borrow_and_update().clone().unwrap();
        assert_eq!(reply.revision, 2);
        assert_eq!(reply.result.unwrap().effective.settings.mode, Mode::Off);
        assert!(!queue.lock().unwrap().running);
        assert!(queue.lock().unwrap().pending.is_none());
        assert!(
            queue
                .lock()
                .unwrap()
                .enqueue(job(&profile, 2, Mode::Direct))
                .is_err(),
            "duplicate source authority must not restart a physical writer"
        );
    }
}
