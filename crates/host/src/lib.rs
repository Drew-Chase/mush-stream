//! Library surface for `mush-stream-host`.
//!
//! Re-exports the host crate's internal modules so other crates (e.g. the
//! Tauri desktop shell) can compose their own capture/encode/transport
//! pipelines without depending on the binary's CLI orchestration.

pub mod audio;
pub mod capture;
pub mod config;
pub mod encode;
pub mod runner;
pub mod transport;
pub mod upnp;
pub mod vigem;
