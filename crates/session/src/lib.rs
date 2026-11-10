// SPDX-License-Identifier: EUPL-1.2
//! Native Mumble networking and worker supervision, independent of GPUI.
pub mod config;
pub mod connection;
mod tls;
pub mod udp;
pub mod workers;

use thiserror::Error;
#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("TLS: {0}")]
    Tls(String),
    #[error("server certificate requires confirmation: {reason}")]
    Certificate {
        sha256: String,
        changed: bool,
        reason: String,
    },
    #[error("timed out while {0}")]
    Timeout(&'static str),
    #[error("Mumble protocol: {0}")]
    Protocol(String),
    #[error("server rejected connection: {0}")]
    Rejected(String),
    #[error("disconnected by server: {0}")]
    Removed(String),
    #[error("worker failed: {0}")]
    Worker(String),
}
