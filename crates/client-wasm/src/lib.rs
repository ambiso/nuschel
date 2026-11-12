// SPDX-License-Identifier: EUPL-1.2
//! The client SDK in the browser (docs/web.md): wasm-bindgen bindings over
//! `nuschl-client`, and connections to Mumble and Nuschl servers through the
//! web relay's tunnel with TLS in WebAssembly. Built with `just wasm-client`;
//! the Svelte client in web/app loads it.
//!
//! [`session`] and [`snapshot`] are plain Rust and tested natively; the
//! browser glue is only built for `wasm32-unknown-unknown`.

pub mod session;
pub mod snapshot;
pub mod storage;
pub mod voice;
#[cfg(any(test, all(target_arch = "wasm32", target_os = "unknown")))]
mod welcome;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod browser;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub use browser::WebClient;
