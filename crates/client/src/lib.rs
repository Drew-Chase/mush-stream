//! Library surface for `mush-stream-client`.
//!
//! Re-exports the client crate's internal modules so other crates (e.g.
//! the Tauri desktop shell) can compose their own receive/decode/present
//! pipelines without depending on the binary's CLI orchestration.

pub mod audio;
pub mod config;
pub mod decode;
pub mod display;
pub mod input;
pub mod transport;
