// SPDX-License-Identifier: EUPL-1.2
//! Native Mumble networking and worker supervision, independent of GPUI.
pub mod config;
pub mod connection;
pub mod identity;
pub mod layout;
pub mod mls;
pub mod model;
mod rename;
pub mod sticky;
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
    #[error(
        "{host} refused the connection on port {port}: no Mumble server is listening there (check the port)"
    )]
    Refused { host: String, port: u16 },
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
    /// The server offers less than it did before (the sticky extension
    /// policy); only the user can accept that, by resetting the record.
    #[error("{0}")]
    Downgrade(String),
    /// The server refuses this client's version (docs/extension.md, "Minimum
    /// versions"). Never retried automatically, and never by plain Mumble.
    #[error(
        "This server requires Nuschl {minimum} or newer; this is Nuschl {current}. Update Nuschl to connect."
    )]
    Outdated { minimum: String, current: String },
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
            Self::Io(_)
                | Self::Refused { .. }
                | Self::Timeout(_)
                | Self::Protocol(_)
                | Self::Tls(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn refused_connections_are_retried() {
        // A restarting server refuses connections for a moment.
        let refused = Error::Refused {
            host: "voice.example.org".into(),
            port: 64738,
        };
        assert!(refused.transient());
    }

    #[test]
    fn an_outdated_client_is_told_the_version_and_not_retried() {
        let outdated = Error::Outdated {
            minimum: "0.3.0".into(),
            current: "0.2.0".into(),
        };
        assert!(!outdated.transient());
        assert_eq!(
            outdated.to_string(),
            "This server requires Nuschl 0.3.0 or newer; this is Nuschl 0.2.0. Update Nuschl to connect."
        );
    }
}
