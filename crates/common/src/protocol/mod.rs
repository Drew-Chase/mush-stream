//! Wire protocol for `app`.
//!
//! The transport is UDP with two logical channels on separate ports:
//! - host→client: video frames, fragmented into `video::MAX_DATAGRAM`-byte
//!   datagrams and reassembled by the client.
//! - client→host: 16-byte input packets and 1-byte control messages,
//!   disambiguated by datagram size.

pub mod audio;
pub mod control;
pub mod error;
pub mod input;
pub mod video;

pub use error::ProtocolError;
