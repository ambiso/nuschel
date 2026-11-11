// SPDX-License-Identifier: EUPL-1.2
//! Bounded asynchronous adapters for persistent worker processes.
use crate::Error;
use nuschel_core::{ChatMessage, HistoryPage, SearchCursor, SearchPage};
use nuschel_ipc::{
    Envelope, LiveMediaEvent, LiveMediaRequest, MAX_FRAME_BYTES, PROTOCOL_VERSION, StorageRequest,
    StorageResponse, TransferEvent, TransferRequest, write_frame,
};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout},
    sync::mpsc,
    time::{Duration, timeout},
};

pub fn sibling(name: &str) -> std::io::Result<PathBuf> {
    let current = std::env::current_exe()?;
    let parent = current
        .parent()
        .ok_or_else(|| std::io::Error::other("executable has no directory"))?;
    Ok(parent.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
}

pub struct Storage {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
    request_id: u64,
    _containment: nuschel_ipc::host::Containment,
}
impl Storage {
    pub fn start(directory: &Path) -> Result<Self, Error> {
        let executable = sibling("nuschel-storage-worker")?;
        let mut command = nuschel_ipc::host::storage_sandbox_command(&executable, directory)
            .map_err(|e| Error::Worker(e.to_string()))?;
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let containment =
            nuschel_ipc::host::contain(&child).map_err(|e| Error::Worker(e.to_string()))?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| Error::Worker("storage stdin missing".into()))?;
        let output = child
            .stdout
            .take()
            .ok_or_else(|| Error::Worker("storage stdout missing".into()))?;
        Ok(Self {
            child,
            input,
            output,
            request_id: 0,
            _containment: containment,
        })
    }
    async fn request(&mut self, payload: StorageRequest) -> Result<StorageResponse, Error> {
        self.request_id += 1;
        let id = self.request_id;
        let exchange = async {
            send(&mut self.input, &Envelope::new(id, payload)).await?;
            let response: Envelope<StorageResponse> = receive(&mut self.output).await?;
            if response.request_id != id {
                return Err(Error::Worker("storage response ID mismatch".into()));
            }
            match response.payload {
                StorageResponse::Rejected(error) => Err(Error::Worker(error)),
                response => Ok(response),
            }
        };
        match timeout(Duration::from_secs(5), exchange).await {
            Ok(Ok(response)) => Ok(response),
            result => {
                let _ = self.child.kill().await;
                match result {
                    Ok(Err(error)) => Err(error),
                    _ => Err(Error::Timeout("waiting for local history")),
                }
            }
        }
    }
    pub async fn append(&mut self, message: ChatMessage) -> Result<i64, Error> {
        match self.request(StorageRequest::Append { message }).await? {
            StorageResponse::Appended(id) => Ok(id),
            _ => Err(Error::Worker("unexpected storage response".into())),
        }
    }
    pub async fn page(
        &mut self,
        server: String,
        channel: String,
        before: Option<i64>,
    ) -> Result<HistoryPage, Error> {
        self.history(StorageRequest::HistoryPage {
            server,
            channel,
            before,
            limit: 40,
        })
        .await
    }
    /// A page around message `id`, to show it in its conversation.
    pub async fn page_around(
        &mut self,
        server: String,
        channel: String,
        id: i64,
    ) -> Result<HistoryPage, Error> {
        self.history(StorageRequest::HistoryAround {
            server,
            channel,
            id,
            limit: 40,
        })
        .await
    }
    pub async fn page_after(
        &mut self,
        server: String,
        channel: String,
        after: i64,
    ) -> Result<HistoryPage, Error> {
        self.history(StorageRequest::HistoryAfter {
            server,
            channel,
            after,
            limit: 40,
        })
        .await
    }
    async fn history(&mut self, request: StorageRequest) -> Result<HistoryPage, Error> {
        match self.request(request).await? {
            StorageResponse::HistoryPage(page) => Ok(page),
            _ => Err(Error::Worker("unexpected storage response".into())),
        }
    }
    /// Full-text search of these servers' history, or of one conversation
    /// of a single server.
    pub async fn search(
        &mut self,
        servers: Vec<String>,
        channel: Option<String>,
        query: String,
        after: Option<SearchCursor>,
    ) -> Result<SearchPage, Error> {
        match self
            .request(StorageRequest::Search {
                servers,
                channel,
                query,
                after,
                limit: 50,
            })
            .await?
        {
            StorageResponse::SearchResults(page) => Ok(page),
            _ => Err(Error::Worker("unexpected storage response".into())),
        }
    }
    pub async fn stop(mut self) {
        let _ = self.child.kill().await;
    }
}

pub struct Media {
    pub commands: mpsc::Sender<LiveMediaRequest>,
    pub events: mpsc::Receiver<LiveMediaEvent>,
    task: tokio::task::JoinHandle<()>,
}
impl Media {
    pub fn start() -> Result<Self, Error> {
        let executable = sibling("nuschel-media-worker")?;
        let mut command = nuschel_ipc::host::media_sandbox_command(&executable)
            .map_err(|e| Error::Worker(e.to_string()))?;
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let containment =
            nuschel_ipc::host::contain(&child).map_err(|e| Error::Worker(e.to_string()))?;
        let mut input = child
            .stdin
            .take()
            .ok_or_else(|| Error::Worker("media stdin missing".into()))?;
        let mut output = child
            .stdout
            .take()
            .ok_or_else(|| Error::Worker("media stdout missing".into()))?;
        let (tx, mut rx) = mpsc::channel(32);
        let (events_tx, events) = mpsc::channel(32);
        let task = tokio::spawn(async move {
            // The worker lives exactly as long as this task holds its containment.
            let _containment = containment;
            let mut reader = tokio::spawn(async move {
                while let Ok(frame) = receive::<LiveMediaEvent>(&mut output).await {
                    if events_tx.send(frame.payload).await.is_err() {
                        break;
                    }
                }
            });
            let mut reader_finished = false;
            loop {
                tokio::select! {
                    _ = &mut reader => { reader_finished = true; break; },
                    payload = rx.recv() => {
                        let Some(payload) = payload else { break; };
                        let shutdown = matches!(payload, LiveMediaRequest::Shutdown);
                        if send(&mut input, &Envelope::new(0, payload)).await.is_err() || shutdown { break; }
                    },
                }
            }
            if !reader_finished {
                reader.abort();
                let _ = reader.await;
            }
            let _ = child.kill().await;
        });
        Ok(Self {
            commands: tx,
            events,
            task,
        })
    }
}
impl Drop for Media {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// One magic-wormhole offer or download in its own sandboxed worker. An offer
/// exposes only `file`, read-only; a download gets no file at all. Dropping it
/// kills the worker.
pub struct Transfer {
    pub commands: mpsc::Sender<TransferRequest>,
    pub events: mpsc::Receiver<TransferEvent>,
    task: tokio::task::JoinHandle<()>,
}
impl Transfer {
    pub fn start(file: Option<&Path>) -> Result<Self, Error> {
        let executable = sibling("nuschel-transfer-worker")?;
        let mut command = nuschel_ipc::host::transfer_sandbox_command(&executable, file)
            .map_err(|e| Error::Worker(e.to_string()))?;
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let containment =
            nuschel_ipc::host::contain(&child).map_err(|e| Error::Worker(e.to_string()))?;
        let (Some(mut input), Some(mut output)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(Error::Worker("transfer worker pipes missing".into()));
        };
        let (commands, mut requests) = mpsc::channel(4);
        // Few events in flight: a slow disk slows the worker, and the sender with it.
        let (events_tx, events) = mpsc::channel(8);
        let task = tokio::spawn(async move {
            let _containment = containment;
            let mut reader = tokio::spawn(async move {
                while let Ok(frame) = receive::<TransferEvent>(&mut output).await {
                    if events_tx.send(frame.payload).await.is_err() {
                        break;
                    }
                }
            });
            let mut exited = false;
            loop {
                tokio::select! {
                    _ = &mut reader => { exited = true; break },
                    request = requests.recv() => {
                        let Some(request) = request else { break };
                        if send(&mut input, &Envelope::new(1, request)).await.is_err() {
                            break;
                        }
                    },
                }
            }
            // A cancelled worker gets a moment to say goodbye to its peers.
            drop(input);
            if !exited && timeout(Duration::from_secs(2), &mut reader).await.is_err() {
                reader.abort();
            }
            let _ = child.kill().await;
        });
        Ok(Self {
            commands,
            events,
            task,
        })
    }
}
impl Drop for Transfer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn send<T: Serialize>(input: &mut ChildStdin, frame: &Envelope<T>) -> Result<(), Error> {
    let mut bytes = Vec::new();
    write_frame(&mut bytes, frame).map_err(|e| Error::Worker(e.to_string()))?;
    timeout(Duration::from_secs(3), input.write_all(&bytes))
        .await
        .map_err(|_| Error::Timeout("writing to worker"))??;
    Ok(())
}
async fn receive<T: DeserializeOwned>(output: &mut ChildStdout) -> Result<Envelope<T>, Error> {
    let size = output.read_u32().await? as usize;
    if size > MAX_FRAME_BYTES {
        return Err(Error::Worker("oversized worker response".into()));
    }
    let mut bytes = vec![0; size];
    output.read_exact(&mut bytes).await?;
    let response: Envelope<T> =
        serde_json::from_slice(&bytes).map_err(|e| Error::Worker(e.to_string()))?;
    if response.version != PROTOCOL_VERSION {
        return Err(Error::Worker("worker protocol mismatch".into()));
    }
    Ok(response)
}
