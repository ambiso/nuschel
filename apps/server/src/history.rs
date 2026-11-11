// SPDX-License-Identifier: EUPL-1.2
//! History storage off the control path (docs/extension.md §4, §6). Handlers
//! assign sequence numbers under the state lock and queue the write; one
//! writer task stores them in order (its own storage worker) and finishes
//! each under the lock: acknowledgement, live fan-out to whoever may read the
//! conversation at that moment, and the stock clients' copy. History pages go
//! to a reader task with a bounded queue (another worker); their result is
//! re-checked against the current authorisation state before it is sent.
use crate::{conversations::Id, server::Server, storage::StorageHandle};
use bytes::Bytes;
use nuschel_ipc::server::{
    PageQuery, ServerStorageRequest as Request, ServerStorageResponse as Response,
};
use nuschel_protocol::nuschel::v1;
use std::sync::Arc;
use tokio::sync::mpsc;

/// History pages waiting for the reader, server-wide.
const READ_QUEUE: usize = 64;

/// What to do once a write is stored.
pub enum Done {
    Nothing,
    /// A Nuschel client's message (or edit, deletion): acknowledge, deliver.
    Request {
        session: u32,
        request_id: u64,
        conversation: Id,
        seq: u64,
        delivery: Delivery,
    },
    /// A stock client's channel message: deliver to Nuschel readers, or the
    /// stock frame if storing failed.
    Stock {
        /// The sender, who has its own copy.
        source: u32,
        conversation: Id,
        message: v1::Message,
        fallback: Bytes,
    },
}

/// The live part of a stored write.
pub enum Delivery {
    /// A new message, and the stock clients' copy of it.
    Message {
        message: v1::Message,
        stock: Option<Stock>,
    },
    /// An edit or deletion event (refers to `target`).
    Event { message: v1::Message },
}

/// A stock `TextMessage` to make once the message is stored.
pub struct Stock {
    /// The sender's session (the stock `actor`), if still connected.
    pub actor: u32,
    pub html: String,
}

pub struct Write {
    pub request: Request,
    pub done: Done,
}

pub struct Read {
    pub session: u32,
    pub request_id: u64,
    pub conversation: Id,
    pub query: PageQuery,
    /// `authz_epoch` when the page was authorised.
    pub epoch: u64,
    pub clipped: bool,
}

/// The handlers' ends of the queues.
pub struct Queues {
    writes: mpsc::UnboundedSender<Write>,
    reads: mpsc::Sender<Read>,
}

/// The tasks' ends, started once the server exists.
pub struct Receivers {
    writes: mpsc::UnboundedReceiver<Write>,
    reads: mpsc::Receiver<Read>,
}

pub fn queues() -> (Queues, Receivers) {
    let (writes, write_receiver) = mpsc::unbounded_channel();
    let (reads, read_receiver) = mpsc::channel(READ_QUEUE);
    (
        Queues { writes, reads },
        Receivers {
            writes: write_receiver,
            reads: read_receiver,
        },
    )
}

impl Queues {
    /// Queue a write. A closed queue means the server is shutting down.
    pub fn store(&self, request: Request, done: Done) {
        let _ = self.writes.send(Write { request, done });
    }

    pub fn write(&self, request: Request) {
        self.store(request, Done::Nothing);
    }

    /// Queue a page read; `false` when the reader is saturated.
    pub fn read(&self, read: Read) -> bool {
        self.reads.try_send(read).is_ok()
    }

    /// Queues whose other ends are gone (fuzzing and unit tests).
    pub fn detached() -> Self {
        queues().0
    }
}

/// Start the writer and the reader.
pub fn spawn(
    server: &Arc<Server>,
    writer: StorageHandle,
    reader: StorageHandle,
    receivers: Receivers,
) -> Vec<tokio::task::JoinHandle<()>> {
    let Receivers {
        mut writes,
        mut reads,
    } = receivers;
    let write_server = server.clone();
    let read_server = server.clone();
    vec![
        tokio::spawn(async move {
            while let Some(job) = writes.recv().await {
                let response = writer.request(job.request).await;
                write_server.lock().stored(job.done, response);
            }
        }),
        tokio::spawn(async move {
            while let Some(job) = reads.recv().await {
                let response = reader
                    .request(Request::HistoryPage {
                        query: job.query.clone(),
                    })
                    .await;
                let page = match response {
                    Some(Response::Page(page)) => Some(page),
                    _ => None,
                };
                read_server.lock().page_read(job, page);
            }
        }),
    ]
}
