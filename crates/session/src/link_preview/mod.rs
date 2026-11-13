// SPDX-License-Identifier: EUPL-1.2
//! Sender-only preview fetch authority. HTTP/HTML/image parsing stays in the
//! no-network content role; only this session owner resolves and opens sockets.
mod resolve;
mod supervise;
mod transport;

use nuschl_link_preview::{
    card::Card,
    metadata::Unavailable,
    policy::{Settings, Target},
};
use std::{
    path::PathBuf,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, Semaphore, oneshot, watch};
use tokio::time::Instant;

pub const LOGICAL_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONNECTIONS: usize = 16;
const MAX_RAW_BYTES: usize = 4 * 1024 * 1024;

/// One process-wide pipeline and no wait queue. Off/incomplete settings do not
/// acquire credit, create a task, resolve a name, open a socket or start a child.
pub async fn fetch(
    executable: PathBuf,
    settings: Settings,
    target: Target,
    cancel: watch::Receiver<bool>,
) -> Result<Card, Unavailable> {
    let route = settings.route().map_err(|error| match error {
        nuschl_link_preview::policy::Error::Address => Unavailable::Address,
        nuschl_link_preview::policy::Error::MissingProxy
        | nuschl_link_preview::policy::Error::MissingDns
        | nuschl_link_preview::policy::Error::Proxy
        | nuschl_link_preview::policy::Error::Dns => Unavailable::Proxy,
        _ => Unavailable::Network,
    })?;
    let cancel = Arc::new(Cancel {
        external: cancel,
        local: AtomicBool::new(false),
        wake: Notify::new(),
    });
    if cancel.is_cancelled() {
        return Err(Unavailable::Network);
    }
    static CREDIT: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let permit = CREDIT
        .get_or_init(|| Arc::new(Semaphore::new(1)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| Unavailable::Network)?;
    let (tx, rx) = oneshot::channel();
    let actor_cancel = cancel.clone();
    tokio::spawn(async move {
        let _permit = permit;
        let mut budget = Budget {
            deadline: Instant::now() + LOGICAL_TIMEOUT,
            connections: 0,
            raw: 0,
            redirects: 0,
        };
        let result =
            supervise::pipeline(&executable, &route, &target, &actor_cancel, &mut budget).await;
        // All child retirement is complete before this task relinquishes credit.
        let result = if actor_cancel.is_cancelled() && result.is_ok() {
            Err(Unavailable::Network)
        } else {
            result
        };
        // Publish completion only once capacity is available to the Hub's
        // already-bounded latest request; retirement has finished above.
        drop(_permit);
        let _ = tx.send(result);
    });
    struct Revoke(Arc<Cancel>);
    impl Drop for Revoke {
        fn drop(&mut self) {
            self.0.revoke();
        }
    }
    let _revoke = Revoke(cancel.clone());
    // Physical completion only. The Driver Hub may notify its logical caller
    // at 30s, but must keep awaiting this same future with cancellation set.
    // An already-started libc lookup cannot be forcibly cancelled through std.
    rx.await.unwrap_or(Err(Unavailable::Network))
}

pub(super) struct Cancel {
    external: watch::Receiver<bool>,
    local: AtomicBool,
    wake: Notify,
}
impl Cancel {
    fn is_cancelled(&self) -> bool {
        self.local.load(Ordering::Acquire)
            || *self.external.borrow()
            || self.external.has_changed().is_err()
    }
    fn revoke(&self) {
        self.local.store(true, Ordering::Release);
        self.wake.notify_one();
    }
    async fn cancelled(&self) {
        let mut external = self.external.clone();
        loop {
            if self.is_cancelled() {
                return;
            }
            tokio::select! {_=self.wake.notified()=>{},result=external.changed()=>{if result.is_err(){return}}}
        }
    }
}

pub(super) struct Budget {
    deadline: Instant,
    connections: usize,
    raw: usize,
    redirects: usize,
}
impl Budget {
    fn check(&self, cancel: &Cancel) -> Result<(), Unavailable> {
        if cancel.is_cancelled() {
            Err(Unavailable::Network)
        } else if Instant::now() >= self.deadline {
            Err(Unavailable::Timeout)
        } else {
            Ok(())
        }
    }
    fn connection(&mut self, cancel: &Cancel) -> Result<(), Unavailable> {
        self.check(cancel)?;
        self.connections += 1;
        if self.connections > MAX_CONNECTIONS {
            Err(Unavailable::TooLarge)
        } else {
            Ok(())
        }
    }
    fn raw(&mut self, amount: usize) -> Result<(), Unavailable> {
        self.raw = self.raw.checked_add(amount).ok_or(Unavailable::TooLarge)?;
        if self.raw > MAX_RAW_BYTES {
            Err(Unavailable::TooLarge)
        } else {
            Ok(())
        }
    }
}

pub(super) async fn io_stage<T>(
    cancel: &Cancel,
    budget: &Budget,
    seconds: u64,
    work: impl std::future::Future<Output = std::io::Result<T>>,
) -> Result<T, Unavailable> {
    budget.check(cancel)?;
    let deadline = budget
        .deadline
        .min(Instant::now() + Duration::from_secs(seconds));
    tokio::select! {
        _=cancel.cancelled()=>Err(Unavailable::Network),
        result=tokio::time::timeout_at(deadline,work)=>match result {Ok(Ok(value))=>Ok(value),Ok(Err(_))=>Err(Unavailable::Network),Err(_)=>Err(Unavailable::Timeout)}
    }
}
