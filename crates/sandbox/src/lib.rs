// SPDX-License-Identifier: EUPL-1.2
//! How Nuschl confines its worker processes.
//!
//! The Windows launch stage itself (`nuschl-sandbox-windows`) is a separate
//! crate: it is the one first-party crate allowed unsafe code, and only the
//! workers link it.

pub mod windows_policy;
