// SPDX-License-Identifier: EUPL-1.2
//! Sender-only link previews. Pure data/policy first; no received-message
//! renderer or server calls a fetcher implicitly.
#![forbid(unsafe_code)]

pub use nuschl_core::preview as model;
pub mod policy;
