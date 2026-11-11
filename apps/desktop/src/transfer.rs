// SPDX-License-Identifier: EUPL-1.2
//! Files offered in a conversation, Mumble compatibility mode. Mumble servers
//! carry no files, so an offer is a message with a magic-wormhole receive code
//! (`nuschel_core::transfer`) that stock Mumble users can use with any
//! wormhole client. A code works once: after each pickup the sandboxed
//! transfer worker makes a new one and the offer is posted again, until it is
//! stopped, [`OFFER_LIFETIME`] passes, or Nuschel exits. Nuschel shows each
//! offer as one card, at its newest message.
use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use base64::Engine as _;
use gpui_kit::{prelude::*, *};
use nuschel_core::transfer::{
    Offer, PUBLIC_RENDEZVOUS, display_name, format_offer, format_size, save_name, size_limit,
    valid_code,
};
use nuschel_ipc::{TransferEvent, TransferRequest, WormholeServers};
use nuschel_session::{config::ClientSettings, connection::Command, workers::Transfer};
use tokio::{io::AsyncWriteExt as _, sync::mpsc};

use crate::{
    dialogs::{Tone, styled_button},
    state::{ChannelView, Desktop, ServerView, Status},
    theme::*,
};

/// An offer ends this long after it was made, even while Nuschel keeps
/// running: codes in old messages should not work forever.
pub const OFFER_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);
/// At least this long between two offer messages, well inside Mumble's
/// default message rate limit (one per second, bursts of five).
const POST_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Default)]
pub struct Transfers {
    next_id: u64,
    offers: Vec<Outgoing>,
    downloads: Vec<Download>,
    /// Offers whose new code, or end, still has to be posted; oldest first.
    unposted: VecDeque<u64>,
    last_post: Option<Instant>,
    post_timer: Option<Task<()>>,
}

/// A file we offer.
struct Outgoing {
    id: u64,
    server: String,
    channel: u32,
    /// The current code, or `None` before the first and after the end.
    offer: Offer,
    /// Every code it had, to recognise our own messages.
    codes: Vec<String>,
    posted: bool,
    /// Pickups running now: bytes sent and the size.
    sending: BTreeMap<u32, (u64, u64)>,
    sent: u32,
    problem: Option<String>,
    ended: bool,
    commands: Option<mpsc::Sender<TransferRequest>>,
}

/// A file someone offered, being received or received.
struct Download {
    id: u64,
    key: Key,
    path: PathBuf,
    state: Receiving,
    commands: Option<mpsc::Sender<TransferRequest>>,
}

enum Receiving {
    Connecting,
    Progress { done: u64, total: u64 },
    Done,
    Failed(String),
    Cancelled,
}

/// One offer in one conversation: its sender, file name and size label.
#[derive(Clone, PartialEq, Eq)]
struct Key {
    server: String,
    channel: u32,
    author: String,
    name: String,
    size: String,
}

impl Key {
    fn new(server: &ServerView, channel: &ChannelView, author: &str, offer: &Offer) -> Self {
        Self {
            server: server.id.clone(),
            channel: channel.id,
            author: author.to_owned(),
            name: offer.name.clone(),
            size: format_size(offer.size),
        }
    }
}

/// What a transfer's supervising task reports to the desktop.
enum Update {
    Event(TransferEvent),
    /// The worker has exited: for a download, whether the file is complete.
    Finished(Result<(), String>),
}

/// The servers from Settings. The test override pins both servers and keeps
/// transfers on the relay: direct connections would ask a public STUN server.
fn wormhole_servers(settings: &ClientSettings) -> WormholeServers {
    let pinned = |name| std::env::var(name).ok().filter(|value| !value.is_empty());
    match (
        pinned("NUSCHEL_WORMHOLE_RENDEZVOUS"),
        pinned("NUSCHEL_WORMHOLE_RELAY"),
    ) {
        (None, None) => WormholeServers {
            rendezvous: settings.wormhole_rendezvous.clone(),
            relay: settings.wormhole_relay.clone(),
            direct: true,
        },
        (rendezvous, relay) => WormholeServers {
            rendezvous: rendezvous.unwrap_or_else(|| settings.wormhole_rendezvous.clone()),
            relay: relay.unwrap_or_else(|| settings.wormhole_relay.clone()),
            direct: false,
        },
    }
}

impl Desktop {
    /// Offer each file in the selected conversation.
    pub fn offer_files(&mut self, files: Vec<PathBuf>, cx: &mut Context<Self>) {
        let Some((index, channel)) = self.selected_indices() else {
            return;
        };
        let servers = wormhole_servers(&self.settings);
        let rendezvous =
            (servers.rendezvous != PUBLIC_RENDEZVOUS).then(|| servers.rendezvous.clone());
        let server = &mut self.servers[index];
        if server.status != Status::Connected {
            server.notice = Some("Connect before offering a file.".into());
            cx.notify();
            return;
        }
        let (server_id, channel) = (server.id.clone(), server.channels[channel].id);
        for path in files {
            let size = match std::fs::metadata(&path) {
                Ok(metadata) if metadata.is_file() => metadata.len(),
                Ok(_) => {
                    server.notice = Some("Only files can be offered, not folders.".into());
                    continue;
                }
                Err(error) => {
                    server.notice = Some(format!("Cannot read {}: {error}", path.display()));
                    continue;
                }
            };
            let name = display_name(&path.file_name().unwrap_or_default().to_string_lossy());
            let started = {
                let _runtime = self.backend.runtime().enter();
                Transfer::start(Some(&path))
            };
            let request = TransferRequest::Offer {
                name: name.clone(),
                servers: servers.clone(),
            };
            let worker = match started {
                Ok(worker) if worker.commands.try_send(request).is_ok() => worker,
                Ok(_) => continue,
                Err(error) => {
                    server.notice = Some(format!("File transfer is unavailable: {error}"));
                    continue;
                }
            };
            self.transfers.next_id += 1;
            let id = self.transfers.next_id;
            self.transfers.offers.push(Outgoing {
                id,
                server: server_id.clone(),
                channel,
                offer: Offer {
                    name,
                    size,
                    code: None,
                    rendezvous: rendezvous.clone(),
                },
                codes: Vec::new(),
                posted: false,
                sending: BTreeMap::new(),
                sent: 0,
                problem: None,
                ended: false,
                commands: Some(worker.commands.clone()),
            });
            let (updates, receiver) = mpsc::channel(32);
            self.backend.runtime().spawn(run_offer(worker, updates));
            follow(id, receiver, cx);
        }
        cx.notify();
    }

    /// Offer `files` (from the command line) in the conversation Nuschel opens
    /// at startup, once connected and in it.
    pub fn offer_when_connected(&mut self, files: Vec<PathBuf>, cx: &mut Context<Self>) {
        if files.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            for _ in 0..240 {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let ready = this.update(cx, |this, _| {
                    this.selected_indices().is_some_and(|(server, channel)| {
                        let server = &this.servers[server];
                        server.status == Status::Connected
                            && server.voice_channel() == Some(server.channels[channel].id)
                    })
                });
                match ready {
                    Ok(true) => break,
                    Ok(false) => continue,
                    Err(_) => return,
                }
            }
            let _ = this.update(cx, |this, cx| this.offer_files(files, cx));
        })
        .detach();
    }

    /// Stop offering; the conversation is told it is no longer available.
    fn stop_offer(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(offer) = self
            .transfers
            .offers
            .iter_mut()
            .find(|offer| offer.id == id)
        else {
            return;
        };
        if let Some(commands) = offer.commands.take() {
            let _ = commands.try_send(TransferRequest::Cancel);
        }
        if !offer.ended {
            offer.ended = true;
            offer.sending.clear();
            if offer.posted {
                self.announce(id, cx);
            }
        }
    }

    /// Ask where to save an offered file, then receive it.
    fn download(&mut self, key: Key, offer: Offer, cx: &mut Context<Self>) {
        let directory = downloads();
        let target = cx.prompt_for_new_path(&directory, Some(&save_name(&offer.name)));
        cx.spawn(async move |this, cx| {
            let chosen = target.await;
            let _ = this.update(cx, |this, cx| {
                match chosen {
                    Ok(Ok(Some(path))) => this.receive(key, offer, path, cx),
                    Ok(Err(error)) => {
                        this.error = Some(format!("Cannot ask where to save: {error}"))
                    }
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Receive `offer` into `path` (the user's choice) with its current code.
    fn receive(&mut self, key: Key, offer: Offer, path: PathBuf, cx: &mut Context<Self>) {
        let Some(code) = offer.code.clone() else {
            return;
        };
        let mut servers = wormhole_servers(&self.settings);
        // The sender's rendezvous server is where its code is waiting.
        servers.rendezvous = offer
            .rendezvous
            .clone()
            .unwrap_or_else(|| PUBLIC_RENDEZVOUS.into());
        let request = TransferRequest::Receive {
            code,
            max_size: size_limit(offer.size),
            servers,
        };
        let started = {
            let _runtime = self.backend.runtime().enter();
            Transfer::start(None)
        };
        self.transfers
            .downloads
            .retain(|download| download.key != key);
        self.transfers.next_id += 1;
        let id = self.transfers.next_id;
        let mut download = Download {
            id,
            key,
            path: path.clone(),
            state: Receiving::Connecting,
            commands: None,
        };
        match started {
            Ok(worker) if worker.commands.try_send(request).is_ok() => {
                download.commands = Some(worker.commands.clone());
                let (updates, receiver) = mpsc::channel(32);
                self.backend
                    .runtime()
                    .spawn(run_download(worker, path, updates));
                follow(id, receiver, cx);
            }
            Ok(_) => download.state = Receiving::Failed("The transfer did not start.".into()),
            Err(error) => {
                download.state = Receiving::Failed(format!("File transfer is unavailable: {error}"))
            }
        }
        self.transfers.downloads.push(download);
    }

    fn cancel_download(&mut self, id: u64) {
        if let Some(download) = self.transfers.downloads.iter_mut().find(|d| d.id == id) {
            if let Some(commands) = download.commands.take() {
                let _ = commands.try_send(TransferRequest::Cancel);
            }
            download.state = Receiving::Cancelled;
        }
    }

    fn apply_transfer(&mut self, id: u64, update: Update, cx: &mut Context<Self>) {
        if let Some(offer) = self
            .transfers
            .offers
            .iter_mut()
            .find(|offer| offer.id == id)
        {
            let mut announce = false;
            match update {
                Update::Event(TransferEvent::Code { code })
                    if valid_code(&code) && !offer.ended =>
                {
                    offer.offer.code = Some(code.clone());
                    offer.codes.push(code);
                    announce = true;
                }
                Update::Event(TransferEvent::Started { peer, size, .. }) if !offer.ended => {
                    offer.sending.insert(peer, (0, size));
                    offer.problem = None;
                }
                Update::Event(TransferEvent::Progress { peer, done, total }) => {
                    if let Some(progress) = offer.sending.get_mut(&peer) {
                        *progress = (done, total);
                    }
                }
                Update::Event(TransferEvent::Done { peer }) => {
                    if offer.sending.remove(&peer).is_some() {
                        offer.sent += 1;
                    }
                }
                Update::Event(TransferEvent::Failed { peer, reason }) => {
                    if let Some(peer) = peer {
                        offer.sending.remove(&peer);
                    }
                    offer.problem = Some(tidy(&reason));
                }
                Update::Event(_) => {}
                Update::Finished(_) => {
                    announce = !offer.ended && offer.posted;
                    offer.ended = true;
                    offer.commands = None;
                    offer.sending.clear();
                }
            }
            if announce {
                self.announce(id, cx);
            }
        } else if let Some(download) = self.transfers.downloads.iter_mut().find(|d| d.id == id) {
            if matches!(download.state, Receiving::Cancelled) {
                return;
            }
            download.state = match update {
                Update::Event(TransferEvent::Started { size, .. }) => Receiving::Progress {
                    done: 0,
                    total: size,
                },
                Update::Event(TransferEvent::Progress { done, total, .. }) => {
                    Receiving::Progress { done, total }
                }
                Update::Event(_) => return,
                Update::Finished(Ok(())) => Receiving::Done,
                Update::Finished(Err(reason)) => Receiving::Failed(tidy(&reason)),
            };
            if matches!(download.state, Receiving::Done | Receiving::Failed(_)) {
                download.commands = None;
            }
        }
    }

    /// Post an offer's new code (or its end) when the rate limit allows.
    fn announce(&mut self, id: u64, cx: &mut Context<Self>) {
        if !self.transfers.unposted.contains(&id) {
            self.transfers.unposted.push_back(id);
        }
        self.post_offers(cx);
    }

    fn post_offers(&mut self, cx: &mut Context<Self>) {
        while let Some(&id) = self.transfers.unposted.front() {
            let wait = self.transfers.last_post.map_or(Duration::ZERO, |last| {
                POST_INTERVAL.saturating_sub(last.elapsed())
            });
            if !wait.is_zero() {
                return self.post_later(wait, cx);
            }
            match self.post_offer(id) {
                Some(true) => {
                    self.transfers.unposted.pop_front();
                    self.transfers.last_post = Some(Instant::now());
                }
                // Not connected right now: try again shortly.
                Some(false) => return self.post_later(Duration::from_secs(5), cx),
                None => {
                    self.transfers.unposted.pop_front();
                }
            }
        }
    }

    fn post_later(&mut self, wait: Duration, cx: &mut Context<Self>) {
        if self.transfers.post_timer.is_some() {
            return;
        }
        self.transfers.post_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            let _ = this.update(cx, |this, cx| {
                this.transfers.post_timer = None;
                this.post_offers(cx);
            });
        }));
    }

    /// `Some(true)`: posted; `Some(false)`: try later; `None`: nothing to post.
    fn post_offer(&mut self, id: u64) -> Option<bool> {
        let outgoing = self
            .transfers
            .offers
            .iter_mut()
            .find(|offer| offer.id == id)?;
        let mut offer = outgoing.offer.clone();
        match outgoing.ended {
            // Nobody heard of an offer that ended before its first code was posted.
            true if !outgoing.posted => return None,
            true => offer.code = None,
            false => {
                offer.code.as_ref()?;
            }
        }
        let server = self
            .servers
            .iter_mut()
            .find(|server| server.id == outgoing.server)?;
        let channel = server
            .channels
            .iter()
            .find(|channel| channel.id == outgoing.channel)?;
        if server.status != Status::Connected {
            return Some(false);
        }
        let message = format_offer(&offer);
        let (target, expected_peer) = (
            channel.target.clone(),
            channel.direct_peer.as_ref().map(|peer| peer.instance),
        );
        let command = if server
            .limits
            .as_ref()
            .is_none_or(|limits| limits.allow_html)
        {
            Command::SendHtml {
                target,
                html: vec![message.html],
                expected_peer,
            }
        } else {
            Command::SendText {
                target,
                body: message.text,
                expected_peer,
            }
        };
        let posted = self.backend.command(&server.id, command).is_ok();
        outgoing.posted |= posted;
        Some(posted)
    }
}

/// Apply a transfer's updates to the desktop as they come.
fn follow(id: u64, mut updates: mpsc::Receiver<Update>, cx: &mut Context<Desktop>) {
    cx.spawn(async move |this, cx| {
        while let Some(update) = updates.recv().await {
            let applied = this.update(cx, |this, cx| {
                this.apply_transfer(id, update, cx);
                cx.notify();
            });
            if applied.is_err() {
                break;
            }
        }
    })
    .detach();
}

/// Relay an offer's events until its worker exits; end it after [`OFFER_LIFETIME`].
async fn run_offer(mut worker: Transfer, updates: mpsc::Sender<Update>) {
    let expiry = tokio::time::sleep(OFFER_LIFETIME);
    tokio::pin!(expiry);
    let mut expired = false;
    loop {
        tokio::select! {
            event = worker.events.recv() => match event {
                Some(event) => { let _ = updates.send(Update::Event(event)).await; }
                None => break,
            },
            _ = &mut expiry, if !expired => {
                expired = true;
                let _ = worker.commands.send(TransferRequest::Cancel).await;
                // A worker that does not stop is killed with `worker`.
                if tokio::time::timeout(Duration::from_secs(5), async {
                    while worker.events.recv().await.is_some() {}
                }).await.is_err() {
                    break;
                }
            },
        }
    }
    let _ = updates.send(Update::Finished(Ok(()))).await;
}

/// Write what the worker receives to `<path>.part`, and only a complete file
/// to `path`.
async fn run_download(mut worker: Transfer, path: PathBuf, updates: mpsc::Sender<Update>) {
    let mut part = path.clone().into_os_string();
    part.push(".part");
    let part = PathBuf::from(part);
    let mut result = save(&mut worker.events, &part, &updates).await;
    if result.is_ok() {
        result = tokio::fs::rename(&part, &path)
            .await
            .map_err(|error| format!("Cannot save {}: {error}", path.display()));
    }
    if result.is_err() {
        let _ = tokio::fs::remove_file(&part).await;
    }
    let _ = updates.send(Update::Finished(result)).await;
}

async fn save(
    events: &mut mpsc::Receiver<TransferEvent>,
    part: &Path,
    updates: &mpsc::Sender<Update>,
) -> Result<(), String> {
    let write_error = |error: std::io::Error| format!("Cannot write {}: {error}", part.display());
    let mut file = tokio::fs::File::create(part).await.map_err(write_error)?;
    let (mut size, mut written) = (None, 0_u64);
    while let Some(event) = events.recv().await {
        match event {
            TransferEvent::Started { size: total, .. } => size = Some(total),
            TransferEvent::Data { bytes } => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(bytes)
                    .map_err(|_| "The transfer worker sent garbage.")?;
                written += bytes.len() as u64;
                if size.is_none_or(|size| written > size) {
                    return Err("More data arrived than was offered.".into());
                }
                file.write_all(&bytes).await.map_err(write_error)?;
                continue;
            }
            TransferEvent::Done { .. } if size == Some(written) => {
                file.sync_all().await.map_err(write_error)?;
                return Ok(());
            }
            TransferEvent::Done { .. } => return Err("The transfer ended early.".into()),
            TransferEvent::Failed { reason, .. } => return Err(reason),
            TransferEvent::Code { .. } => continue,
            TransferEvent::Progress { .. } => {}
        }
        let _ = updates.send(Update::Event(event)).await;
    }
    Err("The transfer stopped.".into())
}

/// Where downloads go first: Downloads, or the home directory.
fn downloads() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .map(|home| {
            let downloads = home.join("Downloads");
            if downloads.is_dir() { downloads } else { home }
        })
        .unwrap_or_else(std::env::temp_dir)
}

/// A worker's reason as one short line.
fn tidy(reason: &str) -> String {
    reason
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect()
}

fn percent(done: u64, total: u64) -> u128 {
    (u128::from(done) * 100)
        .checked_div(u128::from(total))
        .map_or(100, |share| share.min(100))
}

fn people(count: u32) -> String {
    match count {
        1 => "1 person".into(),
        count => format!("{count} people"),
    }
}

/// The card shown in place of message `position`'s text, if it offers a
/// file: one card per offer, at its newest message; older messages of the
/// same offer shrink to a line.
pub fn card(
    this: &Desktop,
    server: &ServerView,
    channel: &ChannelView,
    position: usize,
    cx: &mut Context<Desktop>,
) -> Option<AnyElement> {
    let message = channel.messages.get(position)?;
    let offer = message.offer.as_ref()?;
    let replaced = channel.messages[position + 1..].iter().any(|later| {
        later.author == message.author
            && later
                .offer
                .as_ref()
                .is_some_and(|later| later.same_file(offer))
    });
    if replaced {
        return Some(
            div()
                .text_sm()
                .text_color(color(TEXT_FAINT))
                .child(
                    text!(format!("📎 {} · updated below", offer.name))
                        .with_id(("offer-replaced", position)),
                )
                .into_any_element(),
        );
    }
    let ours = this.transfers.offers.iter().rev().find(|outgoing| {
        outgoing.server == server.id
            && outgoing.channel == channel.id
            && outgoing.offer.same_file(offer)
            && match &offer.code {
                Some(code) => outgoing.codes.contains(code),
                None => outgoing.ended,
            }
    });
    let own_name = server
        .own_session
        .and_then(|session| server.users.get(&session))
        .map(|user| user.name.as_str());
    let shown = match ours {
        Some(outgoing) => sender_status(outgoing, position, cx),
        // Ours from an earlier run: it ended when Nuschel did.
        None if own_name == Some(message.author.as_str()) => Shown::status("No longer available"),
        None => {
            let key = Key::new(server, channel, &message.author, offer);
            let download = this.transfers.downloads.iter().find(|d| d.key == key);
            receiver_status(key, offer, download, position, cx)
        }
    };
    let (kind, tone) = kind(&offer.name);
    let extension = offer
        .name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_uppercase())
        .filter(|extension| (1..=4).contains(&extension.chars().count()))
        .unwrap_or_else(|| "FILE".into());
    let size = format_size(offer.size);
    let label = format!(
        "{kind} {}, {size}, from {}. {}",
        offer.name, message.author, shown.status
    );
    let header = div()
        .flex()
        .items_center()
        .gap_3()
        .child(
            div()
                .size(px(40.))
                .flex_shrink_0()
                .rounded(px(6.))
                .bg(tint(tone, 0x33))
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .font_weight(FontWeight::BOLD)
                .text_color(color(tone))
                .child(extension),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .truncate()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(color(TEXT))
                        .child(text!(offer.name.clone()).with_id(("offer-name", position))),
                )
                .child(div().text_xs().text_color(color(TEXT_MUTED)).child(
                    text!(format!("{size} · {}", message.author)).with_id(("offer-meta", position)),
                )),
        )
        .children(shown.buttons);
    let progress = shown.progress.map(|fraction| {
        div()
            .id(("offer-progress", position))
            .role(Role::ProgressIndicator)
            .aria_label(format!("{:.0} %", fraction * 100.))
            .h(px(4.))
            .w_full()
            .rounded_full()
            .bg(color(ELEVATED))
            .overflow_hidden()
            .child(
                div()
                    .h_full()
                    .w(relative(fraction))
                    .rounded_full()
                    .bg(color(ACCENT)),
            )
    });
    let status = (!shown.status.is_empty()).then(|| {
        div()
            .id(("offer-status", position))
            .role(Role::Status)
            .text_sm()
            .text_color(color(TEXT_MUTED))
            .child(text!(shown.status).with_id(("offer-status-text", position)))
    });
    Some(
        div()
            .id(("offer", position))
            .role(Role::Group)
            .aria_label(label)
            .max_w(px(480.))
            .p_3()
            .rounded(px(8.))
            .bg(color(SIDEBAR))
            .border_1()
            .border_color(color(BORDER_STRONG))
            .flex()
            .flex_col()
            .gap_2()
            .child(header)
            .children(progress)
            .children(status)
            .into_any_element(),
    )
}

/// A card's state: a line for people (and screen readers), progress, actions.
#[derive(Default)]
struct Shown {
    status: String,
    progress: Option<f32>,
    buttons: Vec<AnyElement>,
}

impl Shown {
    fn status(status: impl Into<String>) -> Self {
        Self {
            status: status.into(),
            ..Self::default()
        }
    }
}

fn sender_status(outgoing: &Outgoing, position: usize, cx: &mut Context<Desktop>) -> Shown {
    let sent = (outgoing.sent > 0).then(|| format!("sent to {}", people(outgoing.sent)));
    let shares: Vec<u128> = outgoing
        .sending
        .values()
        .map(|&(done, total)| percent(done, total))
        .collect();
    let mut shown = Shown::status(if outgoing.ended {
        "No longer available".to_string()
    } else if !shares.is_empty() {
        let shares: Vec<String> = shares.iter().map(|share| format!("{share} %")).collect();
        format!(
            "Sending to {} · {}",
            people(outgoing.sending.len() as u32),
            shares.join(", ")
        )
    } else if outgoing.offer.code.is_none() {
        "Preparing…".to_string()
    } else {
        "Waiting for someone to download".to_string()
    });
    if let Some(sent) = sent {
        shown.status = format!("{} · {sent}", shown.status);
    }
    if let (false, Some(problem)) = (outgoing.ended, &outgoing.problem) {
        shown.status = format!("{} ({problem})", shown.status);
    }
    if !outgoing.ended && !shares.is_empty() {
        shown.progress = Some(shares.iter().sum::<u128>() as f32 / shares.len() as f32 / 100.);
    }
    if !outgoing.ended {
        let id = outgoing.id;
        shown.buttons.push(styled_button(
            ("offer-stop", position),
            "Stop offering",
            Tone::Secondary,
            move |this, _, cx| {
                this.stop_offer(id, cx);
                cx.notify();
            },
            cx,
        ));
    }
    shown
}

fn receiver_status(
    key: Key,
    offer: &Offer,
    download: Option<&Download>,
    position: usize,
    cx: &mut Context<Desktop>,
) -> Shown {
    let available = offer.code.is_some();
    let start = |label: &'static str, path: Option<PathBuf>, cx: &mut Context<Desktop>| {
        let (key, offer) = (key.clone(), offer.clone());
        styled_button(
            ("offer-download", position),
            label,
            Tone::Primary,
            move |this, _, cx| {
                match &path {
                    Some(path) => this.receive(key.clone(), offer.clone(), path.clone(), cx),
                    None => this.download(key.clone(), offer.clone(), cx),
                }
                cx.notify();
            },
            cx,
        )
    };
    let with = |status: String, buttons: Vec<AnyElement>| Shown {
        status,
        progress: None,
        buttons,
    };
    let Some(download) = download else {
        return if available {
            with(String::new(), vec![start("Download", None, cx)])
        } else {
            Shown::status("No longer available")
        };
    };
    let id = download.id;
    let cancel = |cx: &mut Context<Desktop>| {
        styled_button(
            ("offer-cancel", position),
            "Cancel",
            Tone::Secondary,
            move |this, _, cx| {
                this.cancel_download(id);
                cx.notify();
            },
            cx,
        )
    };
    match &download.state {
        Receiving::Connecting => with("Connecting…".into(), vec![cancel(cx)]),
        Receiving::Progress { done, total } => Shown {
            status: format!(
                "Downloading · {} % · {} of {}",
                percent(*done, *total),
                format_size(*done),
                format_size(*total)
            ),
            progress: Some(percent(*done, *total) as f32 / 100.),
            buttons: vec![cancel(cx)],
        },
        Receiving::Done => {
            let (open, reveal) = (download.path.clone(), download.path.clone());
            let name = download
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            with(
                format!("Saved as {name}"),
                vec![
                    styled_button(
                        ("offer-open", position),
                        "Open",
                        Tone::Primary,
                        move |_, _, cx| cx.open_with_system(&open),
                        cx,
                    ),
                    styled_button(
                        ("offer-reveal", position),
                        "Show in folder",
                        Tone::Secondary,
                        move |_, _, cx| cx.reveal_path(&reveal),
                        cx,
                    ),
                ],
            )
        }
        Receiving::Failed(reason) if available => with(
            reason.clone(),
            vec![start("Try again", Some(download.path.clone()), cx)],
        ),
        Receiving::Cancelled if available => with(
            "Download cancelled".into(),
            vec![start("Download", None, cx)],
        ),
        Receiving::Failed(_) | Receiving::Cancelled => Shown::status("No longer available"),
    }
}

/// A file's kind by its extension, for the card's tile.
fn kind(name: &str) -> (&'static str, u32) {
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "heic" | "avif" | "tif"
        | "tiff" => ("Image", SUCCESS),
        "mp3" | "wav" | "flac" | "ogg" | "opus" | "m4a" | "aac" => ("Audio", WARNING),
        "mp4" | "mkv" | "webm" | "mov" | "avi" => ("Video", DANGER),
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" => ("Archive", 0xb48cf2),
        "pdf" | "txt" | "md" | "doc" | "docx" | "odt" | "rtf" | "xls" | "xlsx" | "ods" | "csv"
        | "ppt" | "pptx" | "odp" | "epub" => ("Document", ACCENT),
        _ => ("File", TEXT_MUTED),
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: GPUI's prelude has its own `test` attribute.
    use super::{
        ClientSettings, PUBLIC_RENDEZVOUS, TransferEvent, kind, mpsc, people, percent, save,
        wormhole_servers,
    };
    use base64::Engine as _;

    #[test]
    fn percentages_and_people_read_well() {
        assert_eq!(percent(45, 100), 45);
        assert_eq!(percent(0, 0), 100);
        assert_eq!(percent(u64::MAX, u64::MAX), 100);
        assert_eq!(people(1), "1 person");
        assert_eq!(people(2), "2 people");
        assert_eq!(kind("Photo.JPG").0, "Image");
        assert_eq!(kind("notes").0, "File");
    }

    /// What `save` makes of a worker's events, and the file it leaves.
    fn saved(events: Vec<TransferEvent>) -> (Result<(), String>, Vec<u8>) {
        let part = std::env::temp_dir().join(format!(
            "nuschel-transfer-test-{}-{}.part",
            std::process::id(),
            events.len()
        ));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(async {
            let (sender, mut receiver) = mpsc::channel(16);
            for event in events {
                sender.send(event).await.unwrap();
            }
            drop(sender);
            let (updates, _ignored) = mpsc::channel(16);
            save(&mut receiver, &part, &updates).await
        });
        let written = std::fs::read(&part).unwrap_or_default();
        let _ = std::fs::remove_file(&part);
        (result, written)
    }

    #[test]
    fn downloads_keep_exactly_what_was_offered() {
        let data = |text: &str| TransferEvent::Data {
            bytes: base64::engine::general_purpose::STANDARD.encode(text),
        };
        let started = |size| TransferEvent::Started {
            peer: 0,
            name: "../../etc/passwd".into(),
            size,
        };
        let done = TransferEvent::Done { peer: 0 };
        let (result, written) = saved(vec![started(6), data("abc"), data("def"), done.clone()]);
        assert_eq!((result, written.as_slice()), (Ok(()), &b"abcdef"[..]));
        // More than announced, data before the size, an early end, a stop.
        assert!(saved(vec![started(3), data("abcd")]).0.is_err());
        assert!(saved(vec![data("abc")]).0.is_err());
        assert!(saved(vec![started(6), data("abc"), done]).0.is_err());
        assert!(saved(vec![started(6), data("abc"), data("de")]).0.is_err());
        let garbage = TransferEvent::Data {
            bytes: "%%%".into(),
        };
        assert!(saved(vec![started(6), garbage]).0.is_err());
    }

    #[test]
    fn the_test_override_pins_servers_and_avoids_stun() {
        let settings = ClientSettings::default();
        let servers = wormhole_servers(&settings);
        if std::env::var_os("NUSCHEL_WORMHOLE_RENDEZVOUS").is_none()
            && std::env::var_os("NUSCHEL_WORMHOLE_RELAY").is_none()
        {
            assert_eq!(servers.rendezvous, PUBLIC_RENDEZVOUS);
            assert!(servers.direct);
        } else {
            assert!(!servers.direct);
        }
    }
}
