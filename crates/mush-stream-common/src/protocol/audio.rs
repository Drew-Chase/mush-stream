//! Audio wire format. Multiplexed onto the same UDP socket as video by
//! reusing the [`super::video::VideoPacketHeader`] layout with the
//! `FLAG_IS_AUDIO` bit set; receivers branch on that bit before
//! attempting video reassembly.
//!
//! Each audio packet is self-contained: one Opus frame per UDP datagram.
//! At 96 kbps × 20 ms frames Opus packets are typically 100–200 bytes,
//! comfortably under the [`MAX_OPUS_PAYLOAD`] cap. Header layout reuse:
//!
//! - `frame_id` is the audio sequence number (u32, monotonic).
//! - `packet_index` is 0 (audio is always single-packet).
//! - `packet_count` is 1.
//! - `flags` has `FLAG_IS_AUDIO` set; other flag bits are ignored.
//! - `parity_count` is 0 (no FEC for audio in v1; loss is inaudible
//!   compared to a video glitch).
//! - `last_data_size` is the Opus payload length in bytes.
//! - `timestamp_us` is the capture wallclock at the host, microseconds.
//! - payload (variable) is the Opus packet bytes, up to `MAX_OPUS_PAYLOAD`.

use crate::protocol::error::ProtocolError;
use crate::protocol::video::{
    self, FLAG_IS_AUDIO, HEADER_SIZE, VideoPacketHeader,
};

/// Cap on the Opus payload we'll put on the wire. Chosen so the total
/// UDP datagram stays under the same `MAX_DATAGRAM` we use for video.
pub const MAX_OPUS_PAYLOAD: usize = video::MAX_PAYLOAD;

/// One decoded audio packet pulled off the wire.
#[derive(Debug, Clone)]
pub struct AudioPacket {
    /// Monotonic sequence number on the audio stream. Receivers use this
    /// to detect drops and reorder.
    pub sequence: u32,
    /// Host-side capture wallclock in microseconds. Same semantic as
    /// `VideoPacketHeader::timestamp_us`.
    pub timestamp_us: u64,
    /// Opus packet bytes.
    pub payload: Vec<u8>,
}

/// Build the 20-byte header bytes for an audio packet. The caller
/// concatenates the Opus payload after the header to form the full
/// datagram.
pub fn write_header(
    sequence: u32,
    timestamp_us: u64,
    payload_len: u16,
    out: &mut [u8; HEADER_SIZE],
) {
    let header = VideoPacketHeader {
        frame_id: sequence,
        packet_index: 0,
        packet_count: 1,
        flags: FLAG_IS_AUDIO,
        parity_count: 0,
        last_data_size: payload_len,
        timestamp_us,
    };
    header.write_to(out);
}

/// Parse an audio datagram (header + Opus payload) into an
/// [`AudioPacket`].
pub fn read_packet(datagram: &[u8]) -> Result<AudioPacket, ProtocolError> {
    let header = VideoPacketHeader::read_from(datagram)?;
    if !header.is_audio() {
        return Err(ProtocolError::Fec(
            "read_packet called on non-audio datagram".into(),
        ));
    }
    let total = HEADER_SIZE + header.last_data_size as usize;
    if datagram.len() < total {
        return Err(ProtocolError::Truncated {
            expected: total,
            got: datagram.len(),
        });
    }
    if header.last_data_size as usize > MAX_OPUS_PAYLOAD {
        return Err(ProtocolError::Oversize {
            max: MAX_OPUS_PAYLOAD,
            got: header.last_data_size as usize,
        });
    }
    Ok(AudioPacket {
        sequence: header.frame_id,
        timestamp_us: header.timestamp_us,
        payload: datagram[HEADER_SIZE..total].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_header_roundtrip() {
        let payload = vec![0xAB; 137];
        let mut buf = vec![0u8; HEADER_SIZE + payload.len()];
        let hdr_slice: &mut [u8; HEADER_SIZE] =
            (&mut buf[..HEADER_SIZE]).try_into().unwrap();
        write_header(42, 1_234_567, payload.len() as u16, hdr_slice);
        buf[HEADER_SIZE..].copy_from_slice(&payload);

        let packet = read_packet(&buf).expect("parse");
        assert_eq!(packet.sequence, 42);
        assert_eq!(packet.timestamp_us, 1_234_567);
        assert_eq!(packet.payload, payload);
    }

    #[test]
    fn read_packet_rejects_non_audio() {
        // Build a video header (no FLAG_IS_AUDIO).
        let header = VideoPacketHeader {
            frame_id: 7,
            packet_index: 0,
            packet_count: 1,
            flags: 0,
            parity_count: 0,
            last_data_size: 5,
            timestamp_us: 0,
        };
        let mut buf = vec![0u8; HEADER_SIZE + 5];
        let hdr_slice: &mut [u8; HEADER_SIZE] =
            (&mut buf[..HEADER_SIZE]).try_into().unwrap();
        header.write_to(hdr_slice);
        assert!(matches!(read_packet(&buf), Err(ProtocolError::Fec(_))));
    }

    #[test]
    fn read_packet_rejects_truncated_payload() {
        let mut buf = vec![0u8; HEADER_SIZE + 5]; // claim 100 bytes payload
        let hdr_slice: &mut [u8; HEADER_SIZE] =
            (&mut buf[..HEADER_SIZE]).try_into().unwrap();
        write_header(0, 0, 100, hdr_slice);
        assert!(matches!(
            read_packet(&buf),
            Err(ProtocolError::Truncated { .. })
        ));
    }
}
