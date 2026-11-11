// SPDX-License-Identifier: EUPL-1.2
//! The launch stage of Nuschel's Windows workers.
//!
//! A worker executable started with `nuschel_ipc::windows_sandbox::LAUNCH_FLAG`
//! calls [`launch_if_requested`] first thing in `main`. That creates (or reuses)
//! the AppContainer profile the policy names, grants it the policy's
//! directories, and starts the same executable inside it with only the
//! worker's arguments: a suspended process with its own job object (kill on
//! close, one process, a memory limit, no access to other processes' windows,
//! the clipboard, global atoms or desktops), the three standard handles and
//! nothing else inherited. The launch stage waits and exits with the worker's
//! exit code. It refuses to start the worker unconfined: any failure exits
//! with [`LAUNCH_FAILED`].
//!
//! On other platforms this crate is empty, and it is the only first-party
//! crate that may contain `unsafe` code, confined to its `win32` module.

#[cfg(windows)]
mod win32;
#[cfg(windows)]
pub use win32::{inherited_file, launch_if_requested};

/// Exit code of a launch stage that could not start its worker confined.
pub const LAUNCH_FAILED: i32 = 125;
