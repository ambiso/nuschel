// SPDX-License-Identifier: EUPL-1.2
//! Sender-only link previews. Pure data/policy first; no received-message
//! renderer or server calls a fetcher implicitly. This crate parses no page:
//! HTML is tokenized only in the content worker (`nuschl_content::preview`).
//!
//! Offline building blocks (no network, DNS, TLS or sockets):
//! - [`policy`]: fetch modes, routes and the page-controlled target policy;
//! - [`metadata`]: the bounded value Open Graph extraction returns;
//! - [`card`]: the validated card and its conversion to and from the carried
//!   [`model`];
//! - [`projection`]: the one-message legacy HTML projection for stock Mumble;
//! - [`draft`]: binding a card to the exact draft, with removal and
//!   generations.
#![forbid(unsafe_code)]

pub use nuschl_core::preview as model;
pub mod card;
pub mod draft;
pub mod metadata;
pub mod policy;
pub mod projection;
pub mod text;
