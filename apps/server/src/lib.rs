// SPDX-License-Identifier: EUPL-1.2
//! `nuscheld`: a Mumble-compatible server (protocol 1.5, with the legacy UDP
//! format for older clients). Voice is routed without decoding; state is kept
//! in memory and written through to the storage worker. See docs/server.md.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod acl;
pub mod auth;
pub mod config;
pub mod connection;
#[doc(hidden)]
pub mod fuzzing;
pub mod handlers;
pub mod limits;
pub mod password;
pub mod routing;
pub mod server;
pub mod state;
pub mod storage;
pub mod text;
pub mod timefmt;
pub mod tls;
pub mod udp;
