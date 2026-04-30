//! Video transport: framing a NAL byte slice into UDP datagrams and
//! reassembling them on the receive side.
//!
//! Wire format (little-endian, 20-byte header):
//! ```text
//!   offset  0  4  6  8  9 10  12         20
//!           |  |  |  |  |  |   |          |
//!           +--+--+--+--+--+---+----------+--- payload (≤ 1200 bytes) ---
//!           |fid|pi|pc|fl|pc'| ld|  ts_us  |
//! ```
//! - `fid` = frame_id (u32)
//! - `pi`  = packet_index (u16). For data packets in `[0, packet_count)`;
//!   for parity packets in `[0, parity_count)`.
//! - `pc`  = packet_count (u16): number of *data* packets.
//! - `fl`  = flags (u8): bit0 keyframe, bit1 last_in_data_frame, bit2 is_parity.
//! - `pc'` = parity_count (u8): number of parity packets after the data
//!   packets on the wire; 0 means no FEC.
//! - `ld`  = last_data_size (u16): actual byte size of the final data
//!   packet's payload (≤ `MAX_PAYLOAD`); needed when FEC is active
//!   because every shard must be `MAX_PAYLOAD` bytes for reed-solomon, so
//!   the last data packet is zero-padded on the wire and its real length
//!   lives here.
//! - `ts_us` = host capture timestamp in microseconds (u64).
//!
//! When FEC is inactive, both `pc'` and `ld` are zero — making the M3
//! header layout (3-byte pad) byte-equivalent to today's. So existing
//! senders/receivers interoperate with the M7 layout as long as they
//! don't try to *use* FEC.
//!
//! Total UDP payload (header + NAL fragment) is capped at 1400 bytes to stay
//! comfortably under typical 1500-byte path MTU.

use std::collections::{BTreeMap, HashMap};

use reed_solomon_erasure::{galois_8::Field, ReedSolomon};

use crate::protocol::error::ProtocolError;

/// Wire-format size of the video packet header.
pub const HEADER_SIZE: usize = 20;
/// Maximum NAL payload bytes per UDP datagram.
pub const MAX_PAYLOAD: usize = 1200;
/// Maximum total UDP datagram length for video.
pub const MAX_DATAGRAM: usize = HEADER_SIZE + MAX_PAYLOAD;

/// `flags` bit: this packet is part of an IDR (keyframe) frame.
pub const FLAG_KEYFRAME: u8 = 1 << 0;
/// `flags` bit: this is the last data packet in its frame_id (i.e.
/// `packet_index == packet_count - 1`). Set on data packets only.
pub const FLAG_LAST_IN_FRAME: u8 = 1 << 1;
/// `flags` bit: this is a parity (FEC) packet, not a data packet.
/// `packet_index` is in `[0, parity_count)` rather than `[0, packet_count)`.
pub const FLAG_IS_PARITY: u8 = 1 << 2;

/// Decoded video packet header, host-side representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoPacketHeader {
    pub frame_id: u32,
    pub packet_index: u16,
    pub packet_count: u16,
    pub flags: u8,
    /// Number of FEC parity packets that follow the data packets for this
    /// frame on the wire. `0` means FEC is not in use for this frame.
    pub parity_count: u8,
    /// Actual byte size of the *last* data packet's payload (≤
    /// `MAX_PAYLOAD`). Always written by the framer; only strictly needed
    /// when FEC is active, since FEC requires all data shards on the wire
    /// to be exactly `MAX_PAYLOAD`.
    pub last_data_size: u16,
    pub timestamp_us: u64,
}

impl VideoPacketHeader {
    pub fn is_keyframe(&self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }
    pub fn is_last_in_frame(&self) -> bool {
        self.flags & FLAG_LAST_IN_FRAME != 0
    }
    pub fn is_parity(&self) -> bool {
        self.flags & FLAG_IS_PARITY != 0
    }

    /// Serialize the header into the first `HEADER_SIZE` bytes of `out`.
    pub fn write_to(&self, out: &mut [u8; HEADER_SIZE]) {
        out[0..4].copy_from_slice(&self.frame_id.to_le_bytes());
        out[4..6].copy_from_slice(&self.packet_index.to_le_bytes());
        out[6..8].copy_from_slice(&self.packet_count.to_le_bytes());
        out[8] = self.flags;
        out[9] = self.parity_count;
        out[10..12].copy_from_slice(&self.last_data_size.to_le_bytes());
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
            parity_count: input[9],
            last_data_size: u16::from_le_bytes(input[10..12].try_into().expect("2 bytes")),
            timestamp_us: u64::from_le_bytes(input[12..20].try_into().expect("8 bytes")),
        })
    }
}

/// Stateful per-stream framer: maintains the monotonic frame_id counter and
/// splits each NAL into one-or-more datagrams, invoking a caller-provided
/// emit closure for each datagram so the caller can `socket.send(...)` it
/// directly without intermediate allocation.
///
/// Also caches Reed-Solomon encoders by (data_count, parity_count) for the
/// FEC path ([`Self::frame_with_fec`]).
#[derive(Default)]
pub struct VideoFramer {
    next_frame_id: u32,
    rs_cache: HashMap<(usize, usize), ReedSolomon<Field>>,
}

impl std::fmt::Debug for VideoFramer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoFramer")
            .field("next_frame_id", &self.next_frame_id)
            .field("rs_cache_keys", &self.rs_cache.keys().collect::<Vec<_>>())
            .finish()
    }
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

        // Size of the last data packet's payload — same value broadcast in
        // every packet's header so receivers know it without depending on
        // the last packet to arrive (and to have FEC reconstruct it later).
        let last_data_size = if nal.is_empty() {
            0
        } else {
            let last_len = nal.len() - (count - 1) * MAX_PAYLOAD;
            u16::try_from(last_len).unwrap_or(MAX_PAYLOAD as u16)
        };

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
                parity_count: 0,
                last_data_size,
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

    /// Like [`Self::frame`] but also computes Reed-Solomon parity packets
    /// at `parity_ratio` redundancy (e.g. `0.10` for 10%). Each shard
    /// (data and parity) is exactly [`MAX_PAYLOAD`] bytes on the wire so
    /// reed-solomon's matrix math works; the receiver uses
    /// `last_data_size` from the header to truncate the recovered NAL.
    ///
    /// Emits N data + K parity datagrams in that order. The receiver only
    /// needs *any* N of the N+K to reconstruct the frame.
    #[allow(clippy::too_many_lines)]
    pub fn frame_with_fec<F>(
        &mut self,
        nal: &[u8],
        timestamp_us: u64,
        is_keyframe: bool,
        parity_ratio: f32,
        mut emit: F,
    ) -> Result<u32, ProtocolError>
    where
        F: FnMut(&[u8]),
    {
        let data_count = nal.len().div_ceil(MAX_PAYLOAD).max(1);
        // Saturate parity_count at u8::MAX-1 (255 is a valid u8 but we want
        // some headroom; reed-solomon-erasure rejects total_shards > 256
        // with the default galois_8 field). f64 here so the f32→usize cast
        // doesn't lose precision for unusually large frames.
        #[allow(clippy::cast_precision_loss)]
        let raw_parity =
            ((data_count as f64 * f64::from(parity_ratio)).ceil() as usize).max(1);
        let parity_count = raw_parity.min(255_usize.saturating_sub(data_count.min(255)));

        // RS galois_8 caps total shards at 256. For NALs that exceed this
        // (large IDRs at high-resolution × high-bitrate), fall back to
        // plain framing. The frame still delivers; if any packet of it
        // drops the client's keyframe-on-loss flow re-requests. Better
        // than dropping the whole frame.
        if data_count + parity_count > 256 {
            return Ok(self.frame(nal, timestamp_us, is_keyframe, emit));
        }

        let frame_id = self.next_frame_id;
        self.next_frame_id = self.next_frame_id.wrapping_add(1);

        let last_data_size = if nal.is_empty() {
            0
        } else {
            let last_len = nal.len() - (data_count - 1) * MAX_PAYLOAD;
            u16::try_from(last_len).unwrap_or(MAX_PAYLOAD as u16)
        };

        // Build all shards as MAX_PAYLOAD-byte vectors (data + parity).
        let mut shards: Vec<Vec<u8>> = Vec::with_capacity(data_count + parity_count);
        for chunk in nal.chunks(MAX_PAYLOAD) {
            let mut shard = vec![0u8; MAX_PAYLOAD];
            shard[..chunk.len()].copy_from_slice(chunk);
            shards.push(shard);
        }
        // If `nal` is empty, chunks(MAX_PAYLOAD) yielded nothing — push one
        // empty data shard so we still have data_count == 1 == shards.len().
        if shards.is_empty() {
            shards.push(vec![0u8; MAX_PAYLOAD]);
        }
        for _ in 0..parity_count {
            shards.push(vec![0u8; MAX_PAYLOAD]);
        }

        let key = (data_count, parity_count);
        if let std::collections::hash_map::Entry::Vacant(e) = self.rs_cache.entry(key) {
            let rs = ReedSolomon::<Field>::new(data_count, parity_count)
                .map_err(|e| ProtocolError::Fec(format!("rs::new({data_count}, {parity_count}): {e:?}")))?;
            e.insert(rs);
        }
        let rs = self.rs_cache.get(&key).expect("just inserted");
        rs.encode(&mut shards)
            .map_err(|e| ProtocolError::Fec(format!("rs::encode: {e:?}")))?;

        let count_u16 = u16::try_from(data_count).unwrap_or(u16::MAX);
        let parity_u8 = u8::try_from(parity_count).unwrap_or(u8::MAX);
        let mut buf = [0u8; MAX_DATAGRAM];

        // Emit data packets at full MAX_PAYLOAD each.
        for (i, shard) in shards.iter().enumerate().take(data_count) {
            let mut flags = 0u8;
            if is_keyframe {
                flags |= FLAG_KEYFRAME;
            }
            if i + 1 == data_count {
                flags |= FLAG_LAST_IN_FRAME;
            }
            let header = VideoPacketHeader {
                frame_id,
                packet_index: u16::try_from(i).unwrap_or(u16::MAX),
                packet_count: count_u16,
                flags,
                parity_count: parity_u8,
                last_data_size,
                timestamp_us,
            };
            let header_buf: &mut [u8; HEADER_SIZE] = (&mut buf[..HEADER_SIZE])
                .try_into()
                .expect("HEADER_SIZE slice");
            header.write_to(header_buf);
            buf[HEADER_SIZE..HEADER_SIZE + MAX_PAYLOAD].copy_from_slice(shard);
            emit(&buf[..HEADER_SIZE + MAX_PAYLOAD]);
        }
        // Emit parity packets.
        for (j, shard) in shards.iter().enumerate().skip(data_count) {
            let mut flags = FLAG_IS_PARITY;
            if is_keyframe {
                flags |= FLAG_KEYFRAME;
            }
            let header = VideoPacketHeader {
                frame_id,
                packet_index: u16::try_from(j - data_count).unwrap_or(u16::MAX),
                packet_count: count_u16,
                flags,
                parity_count: parity_u8,
                last_data_size,
                timestamp_us,
            };
            let header_buf: &mut [u8; HEADER_SIZE] = (&mut buf[..HEADER_SIZE])
                .try_into()
                .expect("HEADER_SIZE slice");
            header.write_to(header_buf);
            buf[HEADER_SIZE..HEADER_SIZE + MAX_PAYLOAD].copy_from_slice(shard);
            emit(&buf[..HEADER_SIZE + MAX_PAYLOAD]);
        }
        Ok(frame_id)
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

/// Internal in-progress assembly slot for one frame_id. Unified shape for
/// both FEC and non-FEC paths: `parity_count == 0` means no FEC and
/// `parity_shards` is empty.
struct PendingFrame {
    packet_count: u16,
    parity_count: u8,
    last_data_size: u16,
    is_keyframe: bool,
    timestamp_us: u64,
    /// Length = `packet_count`. `Some(bytes)` when received. For FEC each
    /// is `MAX_PAYLOAD` bytes; for non-FEC the last one may be shorter.
    data_shards: Vec<Option<Vec<u8>>>,
    /// Length = `parity_count`. Empty when `parity_count == 0`.
    parity_shards: Vec<Option<Vec<u8>>>,
    received_data: u16,
    received_parity: u16,
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
    /// Cached Reed-Solomon decoders by (data_count, parity_count) — same
    /// keys the framer uses; both sides converge on the same set in
    /// practice.
    rs_cache: HashMap<(usize, usize), ReedSolomon<Field>>,
    /// Stats counters, useful for logging and tests.
    pub dropped_old: u64,
    pub dropped_evicted: u64,
    pub fec_recoveries: u64,
    pub fec_failures: u64,
}

impl VideoReassembler {
    pub fn new(max_pending: usize) -> Self {
        debug_assert!(max_pending >= 1);
        Self {
            pending: BTreeMap::new(),
            latest_completed: None,
            max_pending,
            rs_cache: HashMap::new(),
            dropped_old: 0,
            dropped_evicted: 0,
            fec_recoveries: 0,
            fec_failures: 0,
        }
    }

    /// Ingest one received UDP datagram. Returns `Ok(Some(frame))` if this
    /// packet completed a frame, `Ok(None)` if the frame is still pending or
    /// the packet was dropped intentionally (stale frame_id, duplicate),
    /// and `Err` if the datagram was malformed.
    #[allow(clippy::too_many_lines)]
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
        let max_index = if header.is_parity() {
            u16::from(header.parity_count)
        } else {
            header.packet_count
        };
        if header.packet_index >= max_index {
            return Err(ProtocolError::IndexOutOfRange {
                index: header.packet_index,
                count: max_index,
            });
        }

        // Discard packets belonging to a frame older than the most recent
        // completed one.
        if let Some(latest) = self.latest_completed {
            let diff = header.frame_id.wrapping_sub(latest);
            if diff == 0 || diff > u32::MAX / 2 {
                self.dropped_old += 1;
                return Ok(None);
            }
        }

        let payload = &datagram[HEADER_SIZE..];

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
                if p.packet_count != header.packet_count
                    || p.parity_count != header.parity_count
                {
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
                parity_count: header.parity_count,
                last_data_size: header.last_data_size,
                is_keyframe: header.is_keyframe(),
                timestamp_us: header.timestamp_us,
                data_shards: vec![None; header.packet_count as usize],
                parity_shards: vec![None; header.parity_count as usize],
                received_data: 0,
                received_parity: 0,
            }),
        };

        let idx = header.packet_index as usize;
        let is_fec = pending.parity_count > 0;

        if header.is_parity() {
            // Parity packets are only meaningful for FEC frames.
            if !is_fec {
                return Err(ProtocolError::Fec(
                    "received parity packet but parity_count = 0".into(),
                ));
            }
            if pending.parity_shards[idx].is_some() {
                return Ok(None); // duplicate
            }
            // Parity payloads are always MAX_PAYLOAD.
            if payload.len() != MAX_PAYLOAD {
                return Err(ProtocolError::Truncated {
                    expected: MAX_PAYLOAD,
                    got: payload.len(),
                });
            }
            pending.parity_shards[idx] = Some(payload.to_vec());
            pending.received_parity += 1;
        } else {
            if pending.data_shards[idx].is_some() {
                return Ok(None); // duplicate
            }
            // Data packet payload size constraints differ by mode.
            if is_fec {
                // Every data shard on the wire is MAX_PAYLOAD when FEC is
                // active; receiver truncates after reassembly.
                if payload.len() != MAX_PAYLOAD {
                    return Err(ProtocolError::Truncated {
                        expected: MAX_PAYLOAD,
                        got: payload.len(),
                    });
                }
            } else {
                // Non-FEC: every packet except the final one is MAX_PAYLOAD;
                // the final one may be shorter.
                let is_last = header.packet_index + 1 == header.packet_count;
                if !is_last && payload.len() != MAX_PAYLOAD {
                    return Err(ProtocolError::Truncated {
                        expected: MAX_PAYLOAD,
                        got: payload.len(),
                    });
                }
            }
            pending.data_shards[idx] = Some(payload.to_vec());
            pending.received_data += 1;
        }

        // Completion check.
        if is_fec {
            // FEC: any data_count packets out of (data_count + parity_count)
            // are sufficient.
            if u32::from(pending.received_data) + u32::from(pending.received_parity)
                >= u32::from(pending.packet_count)
            {
                return self.try_complete_fec(header.frame_id);
            }
        } else if pending.received_data == pending.packet_count {
            return Ok(Some(self.complete_non_fec(header.frame_id)));
        }
        Ok(None)
    }

    fn complete_non_fec(&mut self, frame_id: u32) -> ReassembledFrame {
        let pending = self
            .pending
            .remove(&frame_id)
            .expect("complete_non_fec called for missing frame");
        let mut nal = Vec::new();
        for shard in pending.data_shards {
            nal.extend_from_slice(&shard.expect("all data shards present"));
        }
        self.latest_completed = Some(frame_id);
        ReassembledFrame {
            frame_id,
            is_keyframe: pending.is_keyframe,
            timestamp_us: pending.timestamp_us,
            nal,
        }
    }

    fn try_complete_fec(
        &mut self,
        frame_id: u32,
    ) -> Result<Option<ReassembledFrame>, ProtocolError> {
        // Borrow scope: extract everything we need from the pending entry,
        // run RS reconstruct, then remove the entry.
        let pending = self
            .pending
            .get_mut(&frame_id)
            .expect("try_complete_fec called for missing frame");
        let data_count = pending.packet_count as usize;
        let parity_count = pending.parity_count as usize;
        let last_data_size = pending.last_data_size as usize;

        // If we have all data shards, no need to reconstruct.
        if pending.received_data == pending.packet_count {
            let mut nal = Vec::with_capacity(data_count * MAX_PAYLOAD);
            for shard in &pending.data_shards {
                nal.extend_from_slice(shard.as_ref().expect("all data shards"));
            }
            // Truncate to actual NAL length.
            let total = (data_count - 1) * MAX_PAYLOAD + last_data_size;
            nal.truncate(total);
            let pending = self.pending.remove(&frame_id).unwrap();
            self.latest_completed = Some(frame_id);
            return Ok(Some(ReassembledFrame {
                frame_id,
                is_keyframe: pending.is_keyframe,
                timestamp_us: pending.timestamp_us,
                nal,
            }));
        }

        // Otherwise, rebuild a `Vec<Option<Vec<u8>>>` of length data + parity,
        // run reed-solomon reconstruct.
        let mut shards: Vec<Option<Vec<u8>>> = Vec::with_capacity(data_count + parity_count);
        for s in &pending.data_shards {
            shards.push(s.clone());
        }
        for s in &pending.parity_shards {
            shards.push(s.clone());
        }

        let key = (data_count, parity_count);
        if let std::collections::hash_map::Entry::Vacant(e) = self.rs_cache.entry(key) {
            let rs = ReedSolomon::<Field>::new(data_count, parity_count).map_err(|err| {
                ProtocolError::Fec(format!("rs::new({data_count}, {parity_count}): {err:?}"))
            })?;
            e.insert(rs);
        }
        let rs = self.rs_cache.get(&key).expect("just inserted");

        match rs.reconstruct(&mut shards) {
            Ok(()) => {
                self.fec_recoveries += 1;
                let mut nal = Vec::with_capacity(data_count * MAX_PAYLOAD);
                for shard in shards.iter().take(data_count) {
                    nal.extend_from_slice(shard.as_ref().expect("reconstructed shard present"));
                }
                let total = (data_count - 1) * MAX_PAYLOAD + last_data_size;
                nal.truncate(total);
                let pending = self.pending.remove(&frame_id).unwrap();
                self.latest_completed = Some(frame_id);
                Ok(Some(ReassembledFrame {
                    frame_id,
                    is_keyframe: pending.is_keyframe,
                    timestamp_us: pending.timestamp_us,
                    nal,
                }))
            }
            Err(e) => {
                // Not enough shards to recover (shouldn't happen if our
                // received_data + received_parity >= packet_count check is
                // right). Log and drop.
                self.fec_failures += 1;
                self.pending.remove(&frame_id);
                Err(ProtocolError::Fec(format!("rs::reconstruct: {e:?}")))
            }
        }
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
            parity_count: 0,
            last_data_size: 0,
            timestamp_us: 0x0102_0304_0506_0708,
        };
        let mut buf = [0u8; HEADER_SIZE];
        h.write_to(&mut buf);
        // With FEC inactive (parity_count=0, last_data_size=0), the M7
        // header is byte-identical to M3's 3-byte-pad layout.
        assert_eq!(&buf[9..12], &[0u8, 0, 0]);
        let parsed = VideoPacketHeader::read_from(&buf).unwrap();
        assert_eq!(parsed, h);
    }

    #[test]
    fn header_roundtrip_with_fec_fields() {
        let h = VideoPacketHeader {
            frame_id: 7,
            packet_index: 3,
            packet_count: 10,
            flags: FLAG_KEYFRAME | FLAG_IS_PARITY,
            parity_count: 2,
            last_data_size: 537,
            timestamp_us: 999,
        };
        let mut buf = [0u8; HEADER_SIZE];
        h.write_to(&mut buf);
        assert_eq!(buf[9], 2);
        assert_eq!(u16::from_le_bytes([buf[10], buf[11]]), 537);
        let parsed = VideoPacketHeader::read_from(&buf).unwrap();
        assert_eq!(parsed, h);
        assert!(parsed.is_parity());
        assert!(parsed.is_keyframe());
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
            parity_count: 0,
            last_data_size: 0,
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
            parity_count: 0,
            last_data_size: 0,
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

    fn frame_with_fec_collect(
        framer: &mut VideoFramer,
        nal: &[u8],
        ts: u64,
        keyframe: bool,
        ratio: f32,
    ) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        framer
            .frame_with_fec(nal, ts, keyframe, ratio, |dg| out.push(dg.to_vec()))
            .expect("frame_with_fec");
        out
    }

    #[test]
    fn fec_roundtrip_no_loss() {
        let mut framer = VideoFramer::new();
        let nal = make_nal(0xa0, MAX_PAYLOAD * 4 + 73);
        let datagrams = frame_with_fec_collect(&mut framer, &nal, 1234, true, 0.10);
        // 5 data packets (4 full + 1 short, padded on wire to MAX_PAYLOAD)
        // + ceil(5 * 0.10) = 1 parity = 6 datagrams.
        assert_eq!(datagrams.len(), 6);

        let mut reasm = VideoReassembler::new(8);
        let mut completed = None;
        for dg in &datagrams {
            if let Some(f) = reasm.ingest(dg).unwrap() {
                completed = Some(f);
            }
        }
        let frame = completed.expect("frame should complete");
        assert_eq!(frame.nal, nal);
        assert!(frame.is_keyframe);
        assert_eq!(frame.timestamp_us, 1234);
    }

    #[test]
    fn fec_recovers_from_one_data_drop() {
        let mut framer = VideoFramer::new();
        let nal = make_nal(0xb0, MAX_PAYLOAD * 4 + 100);
        // ratio 0.50 -> 5 data + 3 parity = 8 packets; we drop 2 data
        // and rely on RS to recover.
        let datagrams = frame_with_fec_collect(&mut framer, &nal, 99, false, 0.50);
        assert_eq!(datagrams.len(), 8);

        let mut reasm = VideoReassembler::new(8);
        let mut completed = None;
        // Skip data packets at indices 1 and 3 (two losses < 3 parity).
        for (i, dg) in datagrams.iter().enumerate() {
            if i == 1 || i == 3 {
                continue;
            }
            if let Some(f) = reasm.ingest(dg).unwrap() {
                completed = Some(f);
            }
        }
        let frame = completed.expect("RS should reconstruct lost data shards");
        assert_eq!(frame.nal, nal);
        assert_eq!(reasm.fec_recoveries, 1);
    }

    #[test]
    fn fec_recovers_from_one_parity_drop() {
        let mut framer = VideoFramer::new();
        let nal = make_nal(0xc0, MAX_PAYLOAD * 3);
        // 3 data + 1 parity = 4 datagrams; drop the parity, completes
        // via the all-data fast path (no RS needed).
        let datagrams = frame_with_fec_collect(&mut framer, &nal, 0, false, 0.10);
        assert_eq!(datagrams.len(), 4);

        let mut reasm = VideoReassembler::new(8);
        let mut completed = None;
        for (i, dg) in datagrams.iter().enumerate() {
            // Last packet is the parity (data first, then parity).
            if i == datagrams.len() - 1 {
                continue;
            }
            if let Some(f) = reasm.ingest(dg).unwrap() {
                completed = Some(f);
            }
        }
        let frame = completed.expect("frame should complete from data alone");
        assert_eq!(frame.nal, nal);
        // No RS reconstruct needed since all data shards arrived.
        assert_eq!(reasm.fec_recoveries, 0);
    }

    #[test]
    fn fec_too_much_loss_does_not_complete() {
        let mut framer = VideoFramer::new();
        let nal = make_nal(0xd0, MAX_PAYLOAD * 4);
        // 4 data + 1 parity (10%). Dropping 2 packets exceeds parity.
        let datagrams = frame_with_fec_collect(&mut framer, &nal, 0, false, 0.10);
        assert_eq!(datagrams.len(), 5);

        let mut reasm = VideoReassembler::new(8);
        for (i, dg) in datagrams.iter().enumerate() {
            if i == 0 || i == 2 {
                continue;
            }
            assert!(reasm.ingest(dg).unwrap().is_none());
        }
        // Still pending; RS can't recover with 1 parity but 2 data
        // missing.
        assert!(reasm.pending_frames().any(|id| id == 0));
        assert_eq!(reasm.fec_recoveries, 0);
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
