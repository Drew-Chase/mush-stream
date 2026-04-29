//! Video transport: framing a NAL byte slice into UDP datagrams and
//! reassembling them on the receive side.
//!
//! Wire format per project spec (little-endian, 20-byte header):
//! ```text
//!   offset  0  4  6  8  9 12         20
//!           |  |  |  |  |  |          |
//!           +--+--+--+--+--+----------+--- payload (≤ 1200 bytes) ---
//!           |fid|pi|pc|fl| pad |  ts_us |
//! ```
//! `fid` = frame_id (u32), `pi` = packet_index (u16), `pc` = packet_count (u16),
//! `fl` = flags (u8: bit0 keyframe, bit1 last_in_frame), `pad` = 3 zero bytes,
//! `ts_us` = host capture timestamp in microseconds (u64).
//!
//! Total UDP payload (header + NAL fragment) is capped at 1400 bytes to stay
//! comfortably under typical 1500-byte path MTU.

use std::collections::BTreeMap;

use crate::protocol::error::ProtocolError;

/// Wire-format size of the video packet header.
pub const HEADER_SIZE: usize = 20;
/// Maximum NAL payload bytes per UDP datagram.
pub const MAX_PAYLOAD: usize = 1200;
/// Maximum total UDP datagram length for video.
pub const MAX_DATAGRAM: usize = HEADER_SIZE + MAX_PAYLOAD;

/// `flags` bit: this packet is part of an IDR (keyframe) frame.
pub const FLAG_KEYFRAME: u8 = 1 << 0;
/// `flags` bit: this is the last packet in its frame_id (i.e. `packet_index == packet_count - 1`).
pub const FLAG_LAST_IN_FRAME: u8 = 1 << 1;

/// Decoded video packet header, host-side representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoPacketHeader {
    pub frame_id: u32,
    pub packet_index: u16,
    pub packet_count: u16,
    pub flags: u8,
    pub timestamp_us: u64,
}

impl VideoPacketHeader {
    pub fn is_keyframe(&self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }
    pub fn is_last_in_frame(&self) -> bool {
        self.flags & FLAG_LAST_IN_FRAME != 0
    }

    /// Serialize the header into the first `HEADER_SIZE` bytes of `out`.
    pub fn write_to(&self, out: &mut [u8; HEADER_SIZE]) {
        out[0..4].copy_from_slice(&self.frame_id.to_le_bytes());
        out[4..6].copy_from_slice(&self.packet_index.to_le_bytes());
        out[6..8].copy_from_slice(&self.packet_count.to_le_bytes());
        out[8] = self.flags;
        out[9..12].copy_from_slice(&[0u8; 3]); // _pad
        out[12..20].copy_from_slice(&self.timestamp_us.to_le_bytes());
    }

    /// Parse a header from the start of `input`. `input` must be at least
    /// `HEADER_SIZE` bytes long.
    pub fn read_from(input: &[u8]) -> Result<Self, ProtocolError> {
        if input.len() < HEADER_SIZE {
            return Err(ProtocolError::Truncated {
                expected: HEADER_SIZE,
                got: input.len(),
            });
        }
        Ok(Self {
            frame_id: u32::from_le_bytes(input[0..4].try_into().expect("4 bytes")),
            packet_index: u16::from_le_bytes(input[4..6].try_into().expect("2 bytes")),
            packet_count: u16::from_le_bytes(input[6..8].try_into().expect("2 bytes")),
            flags: input[8],
            // bytes 9..12 are reserved padding; ignored on read
            timestamp_us: u64::from_le_bytes(input[12..20].try_into().expect("8 bytes")),
        })
    }
}

/// Stateful per-stream framer: maintains the monotonic frame_id counter and
/// splits each NAL into one-or-more datagrams, invoking a caller-provided
/// emit closure for each datagram so the caller can `socket.send(...)` it
/// directly without intermediate allocation.
#[derive(Debug, Default)]
pub struct VideoFramer {
    next_frame_id: u32,
}

impl VideoFramer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the frame_id that will be assigned to the *next* call to
    /// [`Self::frame`]. Useful for callers that want to log it before send.
    pub fn next_frame_id(&self) -> u32 {
        self.next_frame_id
    }

    /// Splits `nal` into one or more UDP datagrams (header + payload),
    /// invoking `emit` once per datagram with a slice into a reusable
    /// internal buffer. The closure must process or send the slice before
    /// returning, since the buffer is overwritten on the next iteration.
    ///
    /// Returns the assigned frame_id.
    pub fn frame<F>(
        &mut self,
        nal: &[u8],
        timestamp_us: u64,
        is_keyframe: bool,
        mut emit: F,
    ) -> u32
    where
        F: FnMut(&[u8]),
    {
        let frame_id = self.next_frame_id;
        self.next_frame_id = self.next_frame_id.wrapping_add(1);

        // Even an empty NAL produces one packet, so reassemblers see frame
        // boundaries even if (theoretically) the encoder emitted nothing.
        let count = nal.len().div_ceil(MAX_PAYLOAD).max(1);
        // 1400 max means we can never legitimately produce more than 65535
        // packets from a NAL of any reasonable size, but a hostile caller
        // passing a 78MB+ NAL would overflow u16. We saturate so the
        // resulting datagrams are still parseable but truncated.
        let count_u16 = u16::try_from(count).unwrap_or(u16::MAX);

        let mut buf = [0u8; MAX_DATAGRAM];
        for i in 0..count {
            let off = i * MAX_PAYLOAD;
            let payload_end = (off + MAX_PAYLOAD).min(nal.len());
            let payload = &nal[off..payload_end];

            let mut flags = 0u8;
            if is_keyframe {
                flags |= FLAG_KEYFRAME;
            }
            if i + 1 == count {
                flags |= FLAG_LAST_IN_FRAME;
            }

            let header = VideoPacketHeader {
                frame_id,
                packet_index: u16::try_from(i).unwrap_or(u16::MAX),
                packet_count: count_u16,
                flags,
                timestamp_us,
            };
            let header_buf: &mut [u8; HEADER_SIZE] = (&mut buf[..HEADER_SIZE])
                .try_into()
                .expect("buf is MAX_DATAGRAM, slice is HEADER_SIZE");
            header.write_to(header_buf);
            buf[HEADER_SIZE..HEADER_SIZE + payload.len()].copy_from_slice(payload);
            emit(&buf[..HEADER_SIZE + payload.len()]);
        }

        frame_id
    }
}

/// A frame the reassembler has fully reconstructed from its constituent UDP
/// packets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReassembledFrame {
    pub frame_id: u32,
    pub is_keyframe: bool,
    pub timestamp_us: u64,
    /// The original NAL bytes the framer was given.
    pub nal: Vec<u8>,
}

/// Internal in-progress assembly slot for one frame_id.
struct PendingFrame {
    packet_count: u16,
    received_count: u16,
    received: Vec<bool>,
    is_keyframe: bool,
    timestamp_us: u64,
    /// Pre-allocated to `packet_count * MAX_PAYLOAD`. Each packet writes its
    /// payload at offset `packet_index * MAX_PAYLOAD`; on completion the
    /// buffer is truncated to the actual NAL length.
    buffer: Vec<u8>,
    /// Size in bytes of the final packet's payload, captured when the
    /// `last_in_frame` packet arrives. Until it does, we don't know the
    /// total NAL length.
    last_payload_size: Option<usize>,
}

/// Per-stream reassembler. Accumulates received UDP datagrams into complete
/// frames, dropping packets for stale frames and gating against malformed
/// inputs.
///
/// **Not thread-safe.** Use one reassembler per receiver task.
pub struct VideoReassembler {
    /// Frames currently being assembled, keyed by frame_id. BTreeMap so we
    /// can cheaply find/evict the oldest.
    pending: BTreeMap<u32, PendingFrame>,
    /// frame_id of the most recently *completed* frame, used to discard
    /// late-arriving packets that belong to an older frame.
    latest_completed: Option<u32>,
    /// Cap on `pending.len()` — when we exceed this, the oldest pending
    /// frame is evicted (its packets all arrive too late for it to ever
    /// complete, which on UDP can happen if any of them were dropped).
    max_pending: usize,
    /// Stats counters, useful for logging and tests.
    pub dropped_old: u64,
    pub dropped_evicted: u64,
}

impl VideoReassembler {
    pub fn new(max_pending: usize) -> Self {
        debug_assert!(max_pending >= 1);
        Self {
            pending: BTreeMap::new(),
            latest_completed: None,
            max_pending,
            dropped_old: 0,
            dropped_evicted: 0,
        }
    }

    /// Ingest one received UDP datagram. Returns `Ok(Some(frame))` if this
    /// packet completed a frame, `Ok(None)` if the frame is still pending or
    /// the packet was dropped intentionally (stale frame_id, duplicate),
    /// and `Err` if the datagram was malformed.
    pub fn ingest(&mut self, datagram: &[u8]) -> Result<Option<ReassembledFrame>, ProtocolError> {
        if datagram.len() > MAX_DATAGRAM {
            return Err(ProtocolError::Oversize {
                max: MAX_DATAGRAM,
                got: datagram.len(),
            });
        }
        let header = VideoPacketHeader::read_from(datagram)?;
        if header.packet_count == 0 {
            return Err(ProtocolError::ZeroPacketCount);
        }
        if header.packet_index >= header.packet_count {
            return Err(ProtocolError::IndexOutOfRange {
                index: header.packet_index,
                count: header.packet_count,
            });
        }

        // Discard packets belonging to a frame older than the most recent
        // completed one. Using wrapping subtraction gives us the right
        // behaviour at the u32 wraparound (every ~14 hours at 60fps).
        if let Some(latest) = self.latest_completed {
            // diff > 0 if header.frame_id is newer than latest in modular
            // arithmetic, in the recent half of the u32 space.
            let diff = header.frame_id.wrapping_sub(latest);
            if diff == 0 || diff > u32::MAX / 2 {
                self.dropped_old += 1;
                return Ok(None);
            }
        }

        let payload = &datagram[HEADER_SIZE..];

        // If this is a new frame_id and we're at capacity, evict the oldest
        // pending entry before inserting. Done before entering the Entry API
        // so the borrows don't overlap.
        if !self.pending.contains_key(&header.frame_id)
            && self.pending.len() >= self.max_pending
            && let Some((&oldest_id, _)) = self.pending.iter().next()
        {
            self.pending.remove(&oldest_id);
            self.dropped_evicted += 1;
        }

        let pending = match self.pending.entry(header.frame_id) {
            std::collections::btree_map::Entry::Occupied(o) => {
                let p = o.into_mut();
                if p.packet_count != header.packet_count {
                    return Err(ProtocolError::InconsistentPacketCount {
                        frame_id: header.frame_id,
                        previous: p.packet_count,
                        now: header.packet_count,
                    });
                }
                p
            }
            std::collections::btree_map::Entry::Vacant(v) => v.insert(PendingFrame {
                packet_count: header.packet_count,
                received_count: 0,
                received: vec![false; header.packet_count as usize],
                is_keyframe: header.is_keyframe(),
                timestamp_us: header.timestamp_us,
                buffer: vec![0u8; (header.packet_count as usize) * MAX_PAYLOAD],
                last_payload_size: None,
            }),
        };

        let idx = header.packet_index as usize;
        // Duplicate packet — silently ignore. Common after retransmission
        // schemes or upstream reorder.
        if pending.received[idx] {
            return Ok(None);
        }

        // Validate that non-final packets carry exactly MAX_PAYLOAD bytes.
        // The final packet may be shorter.
        if header.packet_index + 1 < header.packet_count && payload.len() != MAX_PAYLOAD {
            return Err(ProtocolError::Truncated {
                expected: MAX_PAYLOAD,
                got: payload.len(),
            });
        }

        let off = idx * MAX_PAYLOAD;
        pending.buffer[off..off + payload.len()].copy_from_slice(payload);
        pending.received[idx] = true;
        pending.received_count += 1;

        if header.is_last_in_frame() || header.packet_index + 1 == header.packet_count {
            pending.last_payload_size = Some(payload.len());
        }

        if pending.received_count == pending.packet_count {
            // We have every packet; finalize.
            let last_size = pending
                .last_payload_size
                .expect("last packet must have arrived if every packet has");
            let total = (pending.packet_count as usize - 1) * MAX_PAYLOAD + last_size;
            let mut buffer = std::mem::take(&mut pending.buffer);
            buffer.truncate(total);
            let is_keyframe = pending.is_keyframe;
            let timestamp_us = pending.timestamp_us;
            self.pending.remove(&header.frame_id);
            self.latest_completed = Some(header.frame_id);

            return Ok(Some(ReassembledFrame {
                frame_id: header.frame_id,
                is_keyframe,
                timestamp_us,
                nal: buffer,
            }));
        }

        Ok(None)
    }

    /// Frames currently in flight (not yet completed or evicted).
    pub fn pending_frames(&self) -> impl Iterator<Item = u32> + '_ {
        self.pending.keys().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_nal(seed: u8, len: usize) -> Vec<u8> {
        // Deterministic but not constant — easy to spot reassembly errors.
        (0..len).map(|i| seed.wrapping_add(i as u8)).collect()
    }

    #[test]
    fn header_roundtrip() {
        let h = VideoPacketHeader {
            frame_id: 0xdead_beef,
            packet_index: 0xabcd,
            packet_count: 0x1234,
            flags: FLAG_KEYFRAME | FLAG_LAST_IN_FRAME,
            timestamp_us: 0x0102_0304_0506_0708,
        };
        let mut buf = [0u8; HEADER_SIZE];
        h.write_to(&mut buf);
        // Padding bytes must be zero.
        assert_eq!(&buf[9..12], &[0u8, 0, 0]);
        let parsed = VideoPacketHeader::read_from(&buf).unwrap();
        assert_eq!(parsed, h);
    }

    #[test]
    fn header_truncated() {
        let buf = [0u8; HEADER_SIZE - 1];
        assert!(matches!(
            VideoPacketHeader::read_from(&buf),
            Err(ProtocolError::Truncated { .. })
        ));
    }

    #[test]
    fn single_packet_roundtrip() {
        let mut framer = VideoFramer::new();
        let nal = make_nal(0x10, 800); // < MAX_PAYLOAD, fits in one packet
        let mut datagrams: Vec<Vec<u8>> = Vec::new();
        framer.frame(&nal, 12345, true, |dg| datagrams.push(dg.to_vec()));
        assert_eq!(datagrams.len(), 1);

        let mut reasm = VideoReassembler::new(8);
        let result = reasm.ingest(&datagrams[0]).unwrap().unwrap();
        assert_eq!(result.frame_id, 0);
        assert!(result.is_keyframe);
        assert_eq!(result.timestamp_us, 12345);
        assert_eq!(result.nal, nal);
    }

    #[test]
    fn multi_packet_roundtrip_in_order() {
        let mut framer = VideoFramer::new();
        // 3 full + 1 short = 4 packets
        let nal = make_nal(0x20, MAX_PAYLOAD * 3 + 137);
        let mut datagrams: Vec<Vec<u8>> = Vec::new();
        framer.frame(&nal, 99, false, |dg| datagrams.push(dg.to_vec()));
        assert_eq!(datagrams.len(), 4);

        let mut reasm = VideoReassembler::new(8);
        for (i, dg) in datagrams.iter().enumerate() {
            let r = reasm.ingest(dg).unwrap();
            if i + 1 == datagrams.len() {
                let frame = r.unwrap();
                assert!(!frame.is_keyframe);
                assert_eq!(frame.timestamp_us, 99);
                assert_eq!(frame.nal, nal);
            } else {
                assert!(r.is_none(), "incomplete frame should not yield");
            }
        }
    }

    #[test]
    fn multi_packet_reorder_completes() {
        let mut framer = VideoFramer::new();
        let nal = make_nal(0x30, MAX_PAYLOAD * 4 + 1);
        let mut datagrams: Vec<Vec<u8>> = Vec::new();
        framer.frame(&nal, 1, true, |dg| datagrams.push(dg.to_vec()));
        assert_eq!(datagrams.len(), 5);

        // Reverse arrival order — last packet first.
        datagrams.reverse();

        let mut reasm = VideoReassembler::new(8);
        let mut completed = None;
        for dg in &datagrams {
            if let Some(f) = reasm.ingest(dg).unwrap() {
                completed = Some(f);
            }
        }
        let frame = completed.expect("frame should complete despite reorder");
        assert_eq!(frame.nal, nal);
        assert!(frame.is_keyframe);
    }

    #[test]
    fn drop_one_packet_does_not_complete() {
        let mut framer = VideoFramer::new();
        let nal = make_nal(0x40, MAX_PAYLOAD * 2 + 10);
        let mut datagrams: Vec<Vec<u8>> = Vec::new();
        framer.frame(&nal, 0, false, |dg| datagrams.push(dg.to_vec()));
        assert_eq!(datagrams.len(), 3);

        // Drop the middle packet.
        let mut reasm = VideoReassembler::new(8);
        for (i, dg) in datagrams.iter().enumerate() {
            if i == 1 {
                continue;
            }
            assert!(reasm.ingest(dg).unwrap().is_none());
        }
        // Frame is still pending, never completed.
        assert!(reasm.pending_frames().any(|id| id == 0));
    }

    #[test]
    fn duplicate_packet_is_ignored() {
        let mut framer = VideoFramer::new();
        let nal = make_nal(0x50, MAX_PAYLOAD * 2);
        let mut datagrams: Vec<Vec<u8>> = Vec::new();
        framer.frame(&nal, 7, false, |dg| datagrams.push(dg.to_vec()));

        let mut reasm = VideoReassembler::new(8);
        // Send packet 0, then packet 0 again (duplicate), then packet 1.
        assert!(reasm.ingest(&datagrams[0]).unwrap().is_none());
        assert!(reasm.ingest(&datagrams[0]).unwrap().is_none()); // duplicate
        let frame = reasm.ingest(&datagrams[1]).unwrap().unwrap();
        assert_eq!(frame.nal, nal);
    }

    #[test]
    fn stale_frame_id_dropped_after_newer_completes() {
        let mut framer = VideoFramer::new();
        let nal_a = make_nal(0x60, 100);
        let nal_b = make_nal(0x70, 100);
        let mut a = Vec::new();
        let mut b = Vec::new();
        framer.frame(&nal_a, 1, false, |dg| a.push(dg.to_vec()));
        framer.frame(&nal_b, 2, false, |dg| b.push(dg.to_vec()));

        let mut reasm = VideoReassembler::new(8);
        // Receive frame 1 (newer arriving first is fine here, only one each)
        let f_b = reasm.ingest(&b[0]).unwrap().unwrap();
        assert_eq!(f_b.frame_id, 1);
        // Now a late packet for frame 0 arrives — must be dropped.
        let r = reasm.ingest(&a[0]).unwrap();
        assert!(r.is_none());
        assert_eq!(reasm.dropped_old, 1);
    }

    #[test]
    fn malformed_packet_count_zero_rejected() {
        let mut buf = [0u8; HEADER_SIZE];
        let h = VideoPacketHeader {
            frame_id: 0,
            packet_index: 0,
            packet_count: 0,
            flags: 0,
            timestamp_us: 0,
        };
        h.write_to(&mut buf);
        let mut reasm = VideoReassembler::new(8);
        assert_eq!(reasm.ingest(&buf), Err(ProtocolError::ZeroPacketCount));
    }

    #[test]
    fn malformed_index_out_of_range_rejected() {
        let mut buf = [0u8; HEADER_SIZE];
        let h = VideoPacketHeader {
            frame_id: 0,
            packet_index: 5,
            packet_count: 3,
            flags: 0,
            timestamp_us: 0,
        };
        h.write_to(&mut buf);
        let mut reasm = VideoReassembler::new(8);
        assert!(matches!(
            reasm.ingest(&buf),
            Err(ProtocolError::IndexOutOfRange { .. })
        ));
    }

    #[test]
    fn eviction_when_max_pending_exceeded() {
        let mut framer = VideoFramer::new();
        // Each NAL is 2 packets; we only feed packet 0 of each, so each
        // remains pending. With max_pending = 2, the third frame's first
        // packet should evict frame 0.
        let mut all_first_packets: Vec<Vec<u8>> = Vec::new();
        for seed in 0..3 {
            let nal = make_nal(0x80 + seed, MAX_PAYLOAD * 2);
            let mut dgs: Vec<Vec<u8>> = Vec::new();
            framer.frame(&nal, 0, false, |dg| dgs.push(dg.to_vec()));
            all_first_packets.push(dgs.remove(0));
        }

        let mut reasm = VideoReassembler::new(2);
        reasm.ingest(&all_first_packets[0]).unwrap();
        reasm.ingest(&all_first_packets[1]).unwrap();
        reasm.ingest(&all_first_packets[2]).unwrap();
        assert_eq!(reasm.dropped_evicted, 1);
        // Frame 0 evicted; 1 and 2 still pending.
        let pending: Vec<u32> = reasm.pending_frames().collect();
        assert_eq!(pending, vec![1, 2]);
    }

    #[test]
    fn frame_id_increments_per_call() {
        let mut framer = VideoFramer::new();
        assert_eq!(framer.next_frame_id(), 0);
        let mut datagrams: Vec<Vec<u8>> = Vec::new();
        let f0 = framer.frame(&[1, 2, 3], 0, false, |dg| datagrams.push(dg.to_vec()));
        assert_eq!(f0, 0);
        let f1 = framer.frame(&[4, 5, 6], 0, false, |dg| datagrams.push(dg.to_vec()));
        assert_eq!(f1, 1);
        assert_eq!(framer.next_frame_id(), 2);
    }
}
