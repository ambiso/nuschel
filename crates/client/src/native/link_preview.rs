// SPDX-License-Identifier: EUPL-1.2
//! One active fetch and one replaceable latest request. A cancelled child keeps
//! its capacity until the session supervisor has actually retired it.
use super::Event;
use crate::{input::Input, preview::FetchTicket};
use nuschl_link_preview::{draft::RequestId, metadata::Unavailable, policy::Settings};
use nuschl_session::workers;
use std::path::PathBuf;
use tokio::{runtime::Handle, sync::{mpsc, watch}};

#[derive(Clone)]
struct Work {
    executable: PathBuf,
    settings: Settings,
    ticket: FetchTicket,
}

pub(super) struct Hub {
    control: watch::Sender<Option<Work>>,
}

impl Hub {
    pub(super) fn new(runtime: &Handle, events: mpsc::Sender<Event>) -> Self {
        let (control, requests) = watch::channel(None);
        runtime.spawn(run(requests, events));
        Self { control }
    }

    pub(super) fn fetch(&self, settings: Settings, ticket: FetchTicket) -> Result<(), Unavailable> {
        // In particular Off/incomplete-proxy performs no helper lookup or I/O.
        settings.route().map_err(|_| Unavailable::Proxy)?;
        if self.control.is_closed() {
            return Err(Unavailable::Network);
        }
        let executable = workers::sibling("nuschl-content-worker")
            .map_err(|_| Unavailable::Network)?;
        self.control.send_replace(Some(Work { executable, settings, ticket }));
        Ok(())
    }

    pub(super) fn cancel(&self, id: RequestId) {
        self.control.send_if_modified(|work| {
            if work.as_ref().is_some_and(|work| work.ticket.id == id) {
                *work = None;
                true
            } else {
                false
            }
        });
    }
}

async fn run(mut requests: watch::Receiver<Option<Work>>, events: mpsc::Sender<Event>) {
    loop {
        let work = requests.borrow_and_update().clone();
        let Some(work) = work else {
            if requests.changed().await.is_err() {
                return;
            }
            continue;
        };
        let (cancel, cancellation) = watch::channel(false);
        let fetch = nuschl_session::link_preview::fetch(
            work.executable,
            work.settings,
            work.ticket.target,
            cancellation,
        );
        tokio::pin!(fetch);
        let result = tokio::select! {
            biased;
            changed = requests.changed() => {
                cancel.send_replace(true);
                // Never abort this owner: its child may still be exiting.
                let _ = fetch.await;
                if changed.is_err() {
                    return;
                }
                continue;
            }
            result = &mut fetch => result,
        };
        let event = Event::Input(Input::LinkPreview { id: work.ticket.id, result });
        tokio::select! {
            biased;
            changed = requests.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            sent = events.send(event) => {
                if sent.is_err() || requests.changed().await.is_err() {
                    return;
                }
            }
        }
    }
}
