// SPDX-License-Identifier: EUPL-1.2
//! The network side of file transfers, over magic-wormhole. The supervisor
//! starts one worker per offer or download in its sandbox
//! (`nuschel_ipc::host::transfer_sandbox_command`): an offer reads only the
//! file at `--file`, a download streams what it receives back over stdout.
//! Neither opens any other file, and neither chooses a path.

use std::{
    borrow::Cow,
    cell::Cell,
    io,
    path::PathBuf,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
    time::{Duration, Instant},
};

use base64::Engine as _;
use futures::{
    AsyncWrite, FutureExt,
    future::{self, BoxFuture, Shared},
};
use magic_wormhole::{
    MailboxConnection, Wormhole, WormholeError,
    transfer::{self, TransferError},
    transit,
};
use nuschel_ipc::{
    Envelope, TransferEvent, TransferRequest, WormholeServers, read_frame, write_frame,
};

/// Pickups of one offer that run at once; the next code waits for a free slot.
const MAX_PICKUPS: usize = 4;
/// Received bytes per `Data` event.
const CHUNK: usize = 256 * 1024;
/// Words in a code ("7-guitarist-revenge"), as other wormhole clients make them.
const CODE_WORDS: usize = 2;

/// Resolves once the supervisor cancels or goes away.
type Cancel = Shared<BoxFuture<'static, ()>>;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    let file = match arguments.as_slice() {
        [] => None,
        [flag, path] if flag == "--file" => Some(PathBuf::from(path)),
        _ => return Err("usage: nuschel-transfer-worker [--file PATH]".into()),
    };
    let Some(first) = read_frame::<TransferRequest>(&mut io::stdin().lock())? else {
        return Ok(());
    };
    let (stop, stopped) = futures::channel::oneshot::channel::<()>();
    std::thread::spawn(move || {
        let mut input = io::stdin().lock();
        while let Ok(Some(request)) = read_frame::<TransferRequest>(&mut input) {
            if matches!(request.payload, TransferRequest::Cancel) {
                break;
            }
        }
        drop(stop);
    });
    let cancel: Cancel = async move {
        let _ = stopped.await;
    }
    .boxed()
    .shared();
    let result = match (first.payload, file) {
        (TransferRequest::Offer { name, servers }, Some(file)) => {
            async_io::block_on(offer(file, name, servers, cancel))
        }
        (
            TransferRequest::Receive {
                code,
                max_size,
                servers,
            },
            None,
        ) => async_io::block_on(receive(code, max_size, servers, cancel)),
        (TransferRequest::Cancel, _) => Ok(()),
        _ => Err("an offer needs a file, a download none".into()),
    };
    if let Err(reason) = result {
        emit(TransferEvent::Failed {
            peer: Some(0),
            reason,
        });
        std::process::exit(1);
    }
    Ok(())
}

/// One event to the supervisor; without one to read them there is nothing left to do.
fn emit(event: TransferEvent) {
    if write_frame(&mut io::stdout().lock(), &Envelope::new(0, event)).is_err() {
        std::process::exit(1);
    }
}

/// Offer `file` until cancelled: a code, someone receives with it, a new
/// code at once (while that pickup runs), and so on.
async fn offer(
    file: PathBuf,
    name: String,
    servers: WormholeServers,
    cancel: Cancel,
) -> Result<(), String> {
    let relay = relay_hints(&servers)?;
    async_fs::metadata(&file)
        .await
        .map_err(|error| format!("Cannot read the file: {error}"))?;
    let executor = async_executor::LocalExecutor::new();
    let active = Rc::new(Cell::new(0_usize));
    let codes = async {
        let mut peer = 0_u32;
        let mut failures = 0_u32;
        loop {
            while active.get() >= MAX_PICKUPS {
                async_io::Timer::after(Duration::from_millis(500)).await;
            }
            // Someone trying wrong codes, or a server that keeps failing,
            // waits longer each time, so the conversation does not fill up
            // with fresh codes.
            if failures > 0 {
                let seconds = 5_u64 << failures.min(6);
                async_io::Timer::after(Duration::from_secs(seconds)).await;
            }
            let connected = async {
                let mailbox = MailboxConnection::create(config(&servers), CODE_WORDS).await?;
                emit(TransferEvent::Code {
                    code: mailbox.code().to_string(),
                });
                Wormhole::connect(mailbox).await
            };
            match connected.await {
                Ok(wormhole) => {
                    failures = 0;
                    peer += 1;
                    active.set(active.get() + 1);
                    let pickup = send(wormhole, peer, file.clone(), &name, &servers, &relay);
                    let active = active.clone();
                    executor
                        .spawn(async move {
                            pickup.await;
                            active.set(active.get() - 1);
                        })
                        .detach();
                }
                Err(error) => {
                    failures += 1;
                    emit(TransferEvent::Failed {
                        peer: None,
                        reason: describe(&error, true),
                    });
                }
            }
        }
    };
    // Cancelling ends the offer, and the pickups still running with it.
    executor
        .run(future::select(codes.boxed_local(), cancel))
        .await;
    Ok(())
}

/// Send the file to one receiver.
fn send(
    wormhole: Wormhole,
    peer: u32,
    file: PathBuf,
    name: &str,
    servers: &WormholeServers,
    relay: &[transit::RelayHint],
) -> impl Future<Output = ()> + use<> {
    let (name, relay, abilities) = (name.to_owned(), relay.to_vec(), abilities(servers));
    async move {
        let result = async {
            let mut reader = async_fs::File::open(&file).await?;
            let size = reader.metadata().await?.len();
            emit(TransferEvent::Started {
                peer,
                name: name.clone(),
                size,
            });
            transfer::send_file(
                wormhole,
                relay,
                &mut reader,
                name,
                size,
                abilities,
                |_| {},
                progress(peer),
                future::pending(),
            )
            .await
        };
        emit(match result.await {
            Ok(()) => TransferEvent::Done { peer },
            Err(error) => TransferEvent::Failed {
                peer: Some(peer),
                reason: failure(&error),
            },
        });
    }
}

/// Receive one file of at most `max_size` bytes, streamed back as `Data`.
async fn receive(
    code: String,
    max_size: u64,
    servers: WormholeServers,
    cancel: Cancel,
) -> Result<(), String> {
    let relay = relay_hints(&servers)?;
    let code = code.parse().map_err(|_| "This is not a wormhole code.")?;
    let mailbox = MailboxConnection::connect(config(&servers), code, false)
        .await
        .map_err(|error| describe(&error, false))?;
    let wormhole = Wormhole::connect(mailbox)
        .await
        .map_err(|error| describe(&error, false))?;
    let request = transfer::request_file(wormhole, relay, abilities(&servers), cancel.clone())
        .await
        .map_err(|error| failure(&error))?
        .ok_or("Cancelled.")?;
    let size = request.file_size();
    if size > max_size {
        let _ = request.reject().await;
        return Err("The file is larger than its offer said.".into());
    }
    let name = request.file_name().chars().take(1024).collect();
    emit(TransferEvent::Started {
        peer: 0,
        name,
        size,
    });
    let mut sink = Sink {
        buffer: Vec::with_capacity(CHUNK),
        remaining: size,
    };
    request
        .accept(|_| {}, progress(0), &mut sink, cancel)
        .await
        .map_err(|error| failure(&error))?;
    if sink.remaining > 0 {
        return Err("Cancelled.".into());
    }
    emit(TransferEvent::Done { peer: 0 });
    Ok(())
}

/// Received bytes on their way to the supervisor, never more than announced.
struct Sink {
    buffer: Vec<u8>,
    remaining: u64,
}

impl Sink {
    fn send(&mut self) {
        if !self.buffer.is_empty() {
            let bytes = base64::engine::general_purpose::STANDARD.encode(&self.buffer);
            emit(TransferEvent::Data { bytes });
            self.buffer.clear();
        }
    }
}

impl AsyncWrite for Sink {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let Some(remaining) = this.remaining.checked_sub(bytes.len() as u64) else {
            return Poll::Ready(Err(io::Error::other("more data than the offer announced")));
        };
        this.remaining = remaining;
        this.buffer.extend_from_slice(bytes);
        if this.buffer.len() >= CHUNK {
            this.send();
        }
        Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().send();
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(context)
    }
}

/// Progress about four times a second, and at the end.
fn progress(peer: u32) -> impl FnMut(u64, u64) + 'static {
    let mut last: Option<Instant> = None;
    move |done, total| {
        if done == total || last.is_none_or(|at| at.elapsed() >= Duration::from_millis(250)) {
            last = Some(Instant::now());
            emit(TransferEvent::Progress { peer, done, total });
        }
    }
}

fn config(servers: &WormholeServers) -> magic_wormhole::AppConfig<transfer::AppVersion> {
    transfer::APP_CONFIG.rendezvous_url(Cow::Owned(servers.rendezvous.clone()))
}

fn relay_hints(servers: &WormholeServers) -> Result<Vec<transit::RelayHint>, String> {
    let url = servers
        .relay
        .parse()
        .map_err(|_| "The transit relay address is invalid.")?;
    let hint = transit::RelayHint::from_urls(None, [url]).map_err(|error| error.to_string())?;
    Ok(vec![hint])
}

fn abilities(servers: &WormholeServers) -> transit::Abilities {
    if servers.direct {
        transit::Abilities::ALL
    } else {
        transit::Abilities::FORCE_RELAY
    }
}

/// For people: `offering` says whether we hold the code or typed it.
fn describe(error: &WormholeError, offering: bool) -> String {
    match error {
        WormholeError::PakeFailed if offering => "Someone tried a wrong code.".into(),
        WormholeError::PakeFailed => "The code is wrong.".into(),
        WormholeError::UnclaimedNameplate(_) => "This code has been used or has expired.".into(),
        WormholeError::ServerError(_) => "Cannot reach the magic-wormhole server.".into(),
        other => other.to_string(),
    }
}

fn failure(error: &TransferError) -> String {
    match error {
        TransferError::Wormhole(error) => describe(error, false),
        TransferError::TransitConnect(_) => "Could not connect to the other side.".into(),
        TransferError::PeerError(reason) => {
            let reason: String = reason
                .chars()
                .filter(|c| !c.is_control())
                .take(200)
                .collect();
            format!("The other side stopped: {reason}")
        }
        other => other.to_string(),
    }
}
