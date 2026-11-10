// SPDX-License-Identifier: EUPL-1.2
//! Native Mumble networking and worker supervision, independent of GPUI.
pub mod config;
pub mod connection;
pub mod model;
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
    #[error("server rejected connection: {reason}")]
    Rejected {
        kind: model::RejectKind,
        reason: String,
    },
    #[error("{}", removed_text(actor.as_deref(), reason.as_deref(), *ban))]
    Removed {
        actor: Option<String>,
        reason: Option<String>,
        ban: bool,
    },
    #[error("worker failed: {0}")]
    Worker(String),
}

fn removed_text(actor: Option<&str>, reason: Option<&str>, ban: bool) -> String {
    let verb = if ban { "banned" } else { "kicked" };
    match (actor, reason.filter(|reason| !reason.is_empty())) {
        (Some(actor), Some(reason)) => format!("You were {verb} by {actor}: {reason}"),
        (Some(actor), None) => format!("You were {verb} by {actor}"),
        (None, Some(reason)) => format!("Disconnected by the server: {reason}"),
        (None, None) => "Disconnected by the server".into(),
    }
}

impl Error {
    /// Whether reconnecting automatically could help. Certificates, rejected
    /// logins and moderation need a decision from the user instead.
    pub fn transient(&self) -> bool {
        matches!(
            self,
            Self::Io(_) | Self::Timeout(_) | Self::Protocol(_) | Self::Tls(_)
        )
    }
}
