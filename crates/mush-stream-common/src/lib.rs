//! Shared types for `mush-stream`.
//!
//! Currently exposes the wire [`protocol`] (video framing/reassembly, input
//! packets, control messages) used by the host and client crates.

pub mod protocol;
