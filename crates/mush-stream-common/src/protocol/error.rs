//! Wire-format parsing errors shared across packet kinds.

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("datagram too small: expected at least {expected} bytes, got {got}")]
    Truncated { expected: usize, got: usize },
    #[error(
        "datagram too large for one video packet: expected at most {max} bytes, got {got}"
    )]
    Oversize { max: usize, got: usize },
    #[error("packet_index {index} >= packet_count {count}")]
    IndexOutOfRange { index: u16, count: u16 },
    #[error("packet_count must be >= 1, got 0")]
    ZeroPacketCount,
    #[error(
        "packet_count for frame {frame_id} disagreed with previously seen value: \
        was {previous}, now {now}"
    )]
    InconsistentPacketCount {
        frame_id: u32,
        previous: u16,
        now: u16,
    },
    #[error("unknown control message tag: 0x{0:02x}")]
    UnknownControlTag(u8),
    #[error("FEC error: {0}")]
    Fec(String),
}
