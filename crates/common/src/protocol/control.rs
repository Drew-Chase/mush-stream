//! Client→host control messages.
//!
//! Sent on the same UDP port as input packets. The wire format is a single
//! tag byte; receivers disambiguate from input packets by datagram size
//! (input is exactly [`super::input::SIZE`] = 16 bytes; control is 1 byte).

use crate::protocol::error::ProtocolError;

/// Wire size of a control message in bytes.
pub const SIZE: usize = 1;

const TAG_REQUEST_KEYFRAME: u8 = 0x01;
const TAG_DISCONNECT: u8 = 0x02;

/// A control message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlMessage {
    /// Client asks the host to emit an IDR (keyframe) on the next encoded
    /// frame. Sent after detected packet loss so the client can resync
    /// without waiting for the next scheduled keyframe.
    RequestKeyframe,
    /// Client is gracefully shutting down. Host should release any
    /// per-client state (virtual gamepad, encoder session) and resume
    /// listening for new connections.
    Disconnect,
}

impl ControlMessage {
    pub fn write_to(&self, buf: &mut [u8; SIZE]) {
        buf[0] = match self {
            Self::RequestKeyframe => TAG_REQUEST_KEYFRAME,
            Self::Disconnect => TAG_DISCONNECT,
        };
    }

    pub fn read_from(buf: &[u8]) -> Result<Self, ProtocolError> {
        if buf.is_empty() {
            return Err(ProtocolError::Truncated {
                expected: SIZE,
                got: buf.len(),
            });
        }
        match buf[0] {
            TAG_REQUEST_KEYFRAME => Ok(Self::RequestKeyframe),
            TAG_DISCONNECT => Ok(Self::Disconnect),
            other => Err(ProtocolError::UnknownControlTag(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_request_keyframe() {
        let mut buf = [0u8; SIZE];
        ControlMessage::RequestKeyframe.write_to(&mut buf);
        assert_eq!(buf, [TAG_REQUEST_KEYFRAME]);
        let parsed = ControlMessage::read_from(&buf).unwrap();
        assert_eq!(parsed, ControlMessage::RequestKeyframe);
    }

    #[test]
    fn roundtrip_disconnect() {
        let mut buf = [0u8; SIZE];
        ControlMessage::Disconnect.write_to(&mut buf);
        assert_eq!(buf, [TAG_DISCONNECT]);
        let parsed = ControlMessage::read_from(&buf).unwrap();
        assert_eq!(parsed, ControlMessage::Disconnect);
    }

    #[test]
    fn unknown_tag_rejected() {
        assert!(matches!(
            ControlMessage::read_from(&[0xff]),
            Err(ProtocolError::UnknownControlTag(0xff))
        ));
    }

    #[test]
    fn empty_buffer_rejected() {
        assert!(matches!(
            ControlMessage::read_from(&[]),
            Err(ProtocolError::Truncated { .. })
        ));
    }
}
