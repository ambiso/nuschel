// SPDX-License-Identifier: EUPL-1.2
//! Sender-only link previews. Pure data/policy first; no received-message
//! renderer or server calls a fetcher implicitly.
//!
//! Offline building blocks (no network, DNS, TLS or sockets):
//! - [`policy`]: fetch modes, routes and the page-controlled target policy;
//! - [`extract`]: bounded Open Graph extraction over fetched HTML bytes, for
//!   the content worker's isolated process only;
//! - [`card`]: the validated card and its conversion to and from the carried
//!   [`model`];
//! - [`projection`]: the one-message legacy HTML projection for stock Mumble;
//! - [`draft`]: binding a card to the exact draft, with removal and
//!   generations;
//! - [`settings_file`]: the versioned `link-previews.json` sidecar.
#![forbid(unsafe_code)]

pub use nuschl_core::preview as model;
pub mod card;
pub mod draft;
pub mod extract;
pub mod policy;
pub mod projection;
pub mod settings_file;
pub mod text;
pub mod xml;
