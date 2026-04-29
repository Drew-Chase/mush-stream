//! Gamepad input packets sent client→host.
//!
//! Wire format per project spec (16 bytes total, little-endian):
//! ```text
//!   offset  0       4   6   8  10 12 13 14   16
//!           |       |   |   |  |  |  |  |    |
//!           +-------+---+---+--+--+--+--+----+
//!           |buttons|lsx|lsy|rsx|rsy|lt|rt|seq|
//! ```
//!
//! Sent at 250 Hz. Receivers drop packets whose `sequence` is older than the
//! latest accepted, using 16-bit wrapping comparison so the half-second of
//! sequence space we'd plausibly retain handles wraparound correctly.

use crate::protocol::error::ProtocolError;

/// Wire size of an input packet in bytes.
pub const SIZE: usize = 16;

/// Decoded gamepad input packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputPacket {
    pub buttons: u32,
    pub left_stick: (i16, i16),
    pub right_stick: (i16, i16),
    pub triggers: (u8, u8),
    pub sequence: u16,
}

impl InputPacket {
    pub fn write_to(&self, buf: &mut [u8; SIZE]) {
        buf[0..4].copy_from_slice(&self.buttons.to_le_bytes());
        buf[4..6].copy_from_slice(&self.left_stick.0.to_le_bytes());
        buf[6..8].copy_from_slice(&self.left_stick.1.to_le_bytes());
        buf[8..10].copy_from_slice(&self.right_stick.0.to_le_bytes());
        buf[10..12].copy_from_slice(&self.right_stick.1.to_le_bytes());
        buf[12] = self.triggers.0;
        buf[13] = self.triggers.1;
        buf[14..16].copy_from_slice(&self.sequence.to_le_bytes());
    }

    pub fn read_from(buf: &[u8]) -> Result<Self, ProtocolError> {
        if buf.len() < SIZE {
            return Err(ProtocolError::Truncated {
                expected: SIZE,
                got: buf.len(),
            });
        }
        Ok(Self {
            buttons: u32::from_le_bytes(buf[0..4].try_into().expect("4 bytes")),
            left_stick: (
                i16::from_le_bytes(buf[4..6].try_into().expect("2 bytes")),
                i16::from_le_bytes(buf[6..8].try_into().expect("2 bytes")),
            ),
            right_stick: (
                i16::from_le_bytes(buf[8..10].try_into().expect("2 bytes")),
                i16::from_le_bytes(buf[10..12].try_into().expect("2 bytes")),
            ),
            triggers: (buf[12], buf[13]),
            sequence: u16::from_le_bytes(buf[14..16].try_into().expect("2 bytes")),
        })
    }
}

/// `true` if `new` is "newer than" `latest` under 16-bit wrapping
/// comparison: i.e. the forward distance from `latest` to `new` is in
/// `(0, 0x8000)`. Equal sequences are not newer (duplicates dropped).
pub fn is_newer_seq(new: u16, latest: u16) -> bool {
    let diff = new.wrapping_sub(latest);
    (1..0x8000).contains(&diff)
}

/// Stateful receiver that enforces the "drop on receive if sequence is older
/// than latest seen" rule from the project spec.
#[derive(Debug, Default)]
pub struct InputReceiver {
    latest: Option<u16>,
    pub accepted: u64,
    pub dropped_old: u64,
}

impl InputReceiver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Latest accepted sequence number, or None if no packet has been
    /// accepted yet.
    pub fn latest(&self) -> Option<u16> {
        self.latest
    }

    /// Decide whether to accept `packet`. Returns `Some(packet)` if accepted
    /// (caller should apply it); returns `None` if dropped as stale or
    /// duplicate.
    pub fn ingest(&mut self, packet: InputPacket) -> Option<InputPacket> {
        let accept = match self.latest {
            None => true,
            Some(latest) => is_newer_seq(packet.sequence, latest),
        };
        if accept {
            self.latest = Some(packet.sequence);
            self.accepted += 1;
            Some(packet)
        } else {
            self.dropped_old += 1;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(seq: u16) -> InputPacket {
        InputPacket {
            buttons: 0x1234_5678,
            left_stick: (-32768, 32767),
            right_stick: (1, -1),
            triggers: (200, 50),
            sequence: seq,
        }
    }

    #[test]
    fn input_roundtrip() {
        let p = sample(0xabcd);
        let mut buf = [0u8; SIZE];
        p.write_to(&mut buf);
        let parsed = InputPacket::read_from(&buf).unwrap();
        assert_eq!(parsed, p);
    }

    #[test]
    fn input_truncated() {
        let buf = [0u8; SIZE - 1];
        assert!(matches!(
            InputPacket::read_from(&buf),
            Err(ProtocolError::Truncated { .. })
        ));
    }

    #[test]
    fn newer_seq_basic() {
        assert!(is_newer_seq(1, 0));
        assert!(is_newer_seq(100, 50));
        // Just inside the wrap window.
        assert!(is_newer_seq(0, 0xffff));
        // Exactly half — by convention, not "newer".
        assert!(!is_newer_seq(0x8000, 0));
        // Equal — not newer.
        assert!(!is_newer_seq(42, 42));
        // Strictly older.
        assert!(!is_newer_seq(0, 1));
        assert!(!is_newer_seq(0xffff, 0));
    }

    #[test]
    fn receiver_accepts_in_order_drops_old() {
        let mut rx = InputReceiver::new();
        assert!(rx.ingest(sample(10)).is_some());
        assert!(rx.ingest(sample(11)).is_some());
        assert!(rx.ingest(sample(15)).is_some());
        // Out-of-order arrival — older than latest.
        assert!(rx.ingest(sample(12)).is_none());
        assert_eq!(rx.dropped_old, 1);
        assert_eq!(rx.accepted, 3);
        assert_eq!(rx.latest(), Some(15));
    }

    #[test]
    fn receiver_handles_seq_wraparound() {
        let mut rx = InputReceiver::new();
        assert!(rx.ingest(sample(0xfffe)).is_some());
        assert!(rx.ingest(sample(0xffff)).is_some());
        assert!(rx.ingest(sample(0)).is_some(), "wrap to 0 must be accepted");
        assert!(rx.ingest(sample(1)).is_some());
        // Now a stale packet from before the wrap.
        assert!(rx.ingest(sample(0xfffd)).is_none());
        assert_eq!(rx.latest(), Some(1));
    }

    #[test]
    fn receiver_drops_duplicates() {
        let mut rx = InputReceiver::new();
        assert!(rx.ingest(sample(7)).is_some());
        assert!(rx.ingest(sample(7)).is_none());
    }
}
