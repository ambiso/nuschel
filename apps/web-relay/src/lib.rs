// SPDX-License-Identifier: EUPL-1.2
//! The web client's tunnel relay (docs/web.md): a WebSocket in, a TCP
//! connection (and UDP datagrams) out to a Mumble or Nuschl server. TLS runs
//! in the browser, so the relay forwards ciphertext it cannot read. It is not
//! an open proxy: only public addresses, only configured ports, only servers
//! that answer a Mumble ping or a TLS handshake, with limits per client address
//! and per tunnel.

pub mod config;
pub mod limits;
pub mod policy;
pub mod probe;
pub mod relay;

pub use config::Config;
pub use relay::Relay;
