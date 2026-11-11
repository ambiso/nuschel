// SPDX-License-Identifier: EUPL-1.2
//! `nuscheld`: a Mumble-compatible server (protocol 1.5, with the legacy UDP
//! format for older clients). Voice is routed without decoding; state is kept
//! in memory and written through to the storage worker. See docs/server.md.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod admin;
pub mod auth;
pub mod authz;
pub mod chat;
pub mod config;
pub mod connection;
pub mod conversations;
pub mod extension;
#[doc(hidden)]
pub mod fuzzing;
pub mod handlers;
pub mod history;
pub mod kt;
pub mod limits;
pub mod mls;
pub mod murmur;
pub mod obfuscate;
pub mod page_limits;
pub mod password;
pub mod retention;
pub mod routing;
pub mod server;
pub mod state;
pub mod storage;
pub mod text;
pub mod timefmt;
pub mod tls;
pub mod udp;

/// The ACL engine's data types (its evaluation is private to [`authz`]).
pub use authz::acl;
