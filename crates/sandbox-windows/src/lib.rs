// SPDX-License-Identifier: EUPL-1.2
//! The launch stage of Nuschl's Windows workers.
//!
//! A worker executable started with `nuschl_ipc::windows_sandbox::LAUNCH_FLAG`
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
//! This is the only first-party crate allowed `unsafe` code. Its layout keeps
//! that code small and testable (docs/windows-sandbox.md):
//! - `plan`: everything computed without Windows (strings, command line,
//!   environment, job limits, SID and ACL parsing); no unsafe.
//! - `api`: one trait method per Win32 function, with its documented
//!   contract; the Windows implementation is one call each.
//! - `os`: every unsafe call to Windows, one per block, with guards that
//!   release what Windows allocates exactly once.
//! - `raw`: the only other unsafe code, copies out of memory Windows returned.
//! - `launch`: the steps, generic over the API; no unsafe. Tests run them
//!   against a mock (`mock`) under Miri, failing each call in turn.
//!
//! On other platforms only [`LAUNCH_FAILED`] is compiled (and, for tests,
//! everything but the Windows implementation).

#[cfg(any(windows, test))]
mod api;
#[cfg(any(windows, test))]
mod launch;
#[cfg(test)]
mod mock;
#[cfg(any(windows, test))]
mod os;
#[cfg(any(windows, test))]
mod plan;
#[cfg(any(windows, test))]
mod raw;
#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::{inherited_file, launch_if_requested, retire_legacy_profile};

/// Exit code of a launch stage that could not start its worker confined.
pub const LAUNCH_FAILED: i32 = 125;
