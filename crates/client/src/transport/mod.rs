//! UDP transport for the client.
//!
//! One socket, `connect()`-ed to the host, used for both directions:
//! - **video receive**: video packets stream from the host; the receiver
//!   drives a [`VideoReassembler`] and pushes complete frames (each with
//!   the local `Instant` of its first received packet) to the decoder.
//! - **input + control send**: gamepad input + control messages are
//!   sendable on the same socket back to the host.
//!
//! Because the socket is `connect()`-ed, the kernel only delivers
//! datagrams from the host (any return path) and `socket.send` always
//! targets the host. Since the client initiated, NAT routers along the
//! path open a return-path mapping for free — typical UDP hole-punching.
//!
//! On startup the client sends one `RequestKeyframe` control message
//! immediately as a "discovery probe": the host's `recv_from` learns the
//! client's address from this packet and only then knows where to send
//! video. The probe doubles as the IDR request a fresh client always
//! needs.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mush_stream_common::protocol::{
    audio::{self, AudioPacket},
    control::{self, ControlMessage},
    input,
    video::{self, ReassembledFrame, VideoReassembler, VideoPacketHeader, HEADER_SIZE},
};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

/// How many in-flight pending frames the reassembler will track before
/// evicting the oldest. ~500ms of headroom at 60fps.
pub const REASM_MAX_PENDING: usize = 32;

/// 8 MiB UDP receive buffer. The Windows default is ~64 KiB, which is
/// dramatically too small for keyframes at 20 Mbps that arrive as a tight
/// burst — packets get dropped before the tokio receiver has a chance to
/// drain them.
pub const UDP_RECV_BUFFER_BYTES: usize = 8 * 1024 * 1024;

/// Don't spam keyframe requests on a sustained drop streak — one per this
/// interval is enough to ask the encoder for an IDR.
const KEYFRAME_REQUEST_DEBOUNCE: Duration = Duration::from_millis(200);

/// A reassembled frame plus the local `Instant` at which its *first*
/// packet arrived. The display layer uses this `Instant` to compute the
/// network-arrival → present lag without needing synchronized clocks.
#[derive(Debug)]
pub struct DeliveredFrame {
    pub reassembled: ReassembledFrame,
    pub first_packet_instant: Instant,
}

/// Bind a UDP socket with a generously sized receive buffer.
fn bind_udp_with_recv_buffer(bind: SocketAddr) -> std::io::Result<UdpSocket> {
    let domain = match bind {
        SocketAddr::V4(_) => Domain::IPV4,
        SocketAddr::V6(_) => Domain::IPV6,
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_nonblocking(true)?;
    if let Err(e) = socket.set_recv_buffer_size(UDP_RECV_BUFFER_BYTES) {
        tracing::warn!(
            error = %e,
            requested = UDP_RECV_BUFFER_BYTES,
            "failed to enlarge UDP recv buffer; using OS default"
        );
    }
    socket.bind(&bind.into())?;
    let std_socket: std::net::UdpSocket = socket.into();
    UdpSocket::from_std(std_socket)
}

/// Stats reported by the receiver loop on shutdown.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReceiverStats {
    pub packets_received: u64,
    pub bytes_received: u64,
    pub frames_completed: u64,
    pub recv_errors: u64,
    pub malformed: u64,
    pub dropped_old: u64,
    pub dropped_evicted: u64,
    pub detected_gaps: u64,
    pub keyframe_requests_sent: u64,
}

/// Build the client UDP socket, connect()ed to the host. Sends a startup
/// `RequestKeyframe` so the host learns our address (UDP hole-punch +
/// peer discovery) and immediately emits an IDR.
pub async fn connect_to_host(host: SocketAddr) -> std::io::Result<Arc<UdpSocket>> {
    let local: SocketAddr = match host {
        SocketAddr::V4(_) => "0.0.0.0:0".parse().unwrap(),
        SocketAddr::V6(_) => "[::]:0".parse().unwrap(),
    };
    let socket = bind_udp_with_recv_buffer(local)?;
    socket.connect(host).await?;
    tracing::info!(%host, "client socket connected to host");

    // Discovery probe: tells the host our address (it learns from
    // recv_from's source) and asks for an immediate IDR so we don't
    // wait for the next scheduled keyframe.
    let mut buf = [0u8; control::SIZE];
    ControlMessage::RequestKeyframe.write_to(&mut buf);
    if let Err(e) = socket.send(&buf).await {
        tracing::warn!(error = %e, "discovery probe failed; continuing");
    }
    Ok(Arc::new(socket))
}

/// Receive loop: drains the connected socket, drives the reassembler,
/// pushes [`DeliveredFrame`]s to `out`. Detects forward gaps in
/// `frame_id` (or a non-IDR first frame) and sends `RequestKeyframe`
/// back through the same socket via `request_keyframe_send`.
#[allow(clippy::too_many_lines)] // single coherent loop; refactor would scatter state
pub async fn run_video_receiver(
    socket: Arc<UdpSocket>,
    out: mpsc::Sender<DeliveredFrame>,
    request_keyframe_send: Option<mpsc::Sender<crate::input::InputCommand>>,
    audio_out: Option<mpsc::Sender<AudioPacket>>,
) -> std::io::Result<ReceiverStats> {
    use crate::input::InputCommand;

    let mut reasm = VideoReassembler::new(REASM_MAX_PENDING);
    let mut buf = vec![0u8; video::MAX_DATAGRAM];
    let mut stats = ReceiverStats::default();
    let mut last_complete_id: Option<u32> = None;
    let mut last_request = Instant::now()
        .checked_sub(KEYFRAME_REQUEST_DEBOUNCE * 2)
        .unwrap_or_else(Instant::now);
    // Per-second throughput counters (reset each tick) so info-level
    // logs can show whether the receive side stops getting packets
    // during a perceived stall.
    let mut packets_this_sec: u64 = 0;
    let mut bytes_this_sec: u64 = 0;
    let mut frames_this_sec: u64 = 0;
    let mut last_log = Instant::now();

    // Per-frame-id arrival time of the first packet seen, kept until the
    // frame either completes or gets cleaned up.
    let mut first_packet: HashMap<u32, Instant> = HashMap::new();

    let request_keyframe = |reason: &'static str,
                            last_request: &mut Instant,
                            stats: &mut ReceiverStats| {
        let now = Instant::now();
        if now.duration_since(*last_request) < KEYFRAME_REQUEST_DEBOUNCE {
            return;
        }
        *last_request = now;
        stats.keyframe_requests_sent += 1;
        if let Some(tx) = request_keyframe_send.as_ref() {
            let _ = tx.try_send(InputCommand::Control(ControlMessage::RequestKeyframe));
        }
        tracing::info!(reason, "requested keyframe from host");
    };

    loop {
        let len = match socket.recv(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                stats.recv_errors += 1;
                tracing::warn!(error = %e, "recv failed");
                continue;
            }
        };
        stats.packets_received += 1;
        stats.bytes_received += len as u64;
        packets_this_sec += 1;
        bytes_this_sec = bytes_this_sec.saturating_add(len as u64);

        // Peek at the header to dispatch audio vs video and to grab
        // `frame_id` for the video first-seen tracking. Reassembler
        // will re-parse video bytes; audio short-circuits here.
        if len >= HEADER_SIZE
            && let Ok(header) = VideoPacketHeader::read_from(&buf[..len])
        {
            if header.is_audio() {
                if let Some(tx) = audio_out.as_ref() {
                    match audio::read_packet(&buf[..len]) {
                        Ok(pkt) => {
                            // try_send so audio backpressure doesn't
                            // back up UDP recv into the kernel buffer.
                            let _ = tx.try_send(pkt);
                        }
                        Err(e) => {
                            stats.malformed += 1;
                            tracing::debug!(error = %e, "malformed audio packet");
                        }
                    }
                }
                continue;
            }
            first_packet
                .entry(header.frame_id)
                .or_insert_with(Instant::now);
        }

        match reasm.ingest(&buf[..len]) {
            Ok(Some(frame)) => {
                stats.frames_completed += 1;
                frames_this_sec += 1;
                match (last_complete_id, frame.is_keyframe) {
                    (None, false) => {
                        request_keyframe(
                            "joined mid-stream (first frame is not IDR)",
                            &mut last_request,
                            &mut stats,
                        );
                    }
                    (Some(prev), _) => {
                        let diff = frame.frame_id.wrapping_sub(prev);
                        if diff > 1 && diff < u32::MAX / 2 {
                            stats.detected_gaps += 1;
                            request_keyframe(
                                "frame_id gap detected",
                                &mut last_request,
                                &mut stats,
                            );
                        }
                    }
                    _ => {}
                }
                last_complete_id = Some(frame.frame_id);

                let first = first_packet
                    .remove(&frame.frame_id)
                    .unwrap_or_else(Instant::now);
                let delivered = DeliveredFrame {
                    reassembled: frame,
                    first_packet_instant: first,
                };
                if out.send(delivered).await.is_err() {
                    break;
                }
            }
            Ok(None) => {}
            Err(e) => {
                stats.malformed += 1;
                tracing::debug!(error = %e, "ignoring malformed video packet");
            }
        }
        stats.dropped_old = reasm.dropped_old;
        stats.dropped_evicted = reasm.dropped_evicted;

        // Once per second emit a throughput line. If the user reports a
        // stall, the counter going to zero here pinpoints it as a
        // receive-side or upstream issue (no packets arriving); if
        // counters stay healthy the stall is in decode/render.
        if last_log.elapsed() >= Duration::from_secs(1) {
            tracing::info!(
                packets = packets_this_sec,
                bytes = bytes_this_sec,
                frames = frames_this_sec,
                gaps = stats.detected_gaps,
                keyframe_requests = stats.keyframe_requests_sent,
                "client recv throughput (1s)"
            );
            packets_this_sec = 0;
            bytes_this_sec = 0;
            frames_this_sec = 0;
            last_log = Instant::now();
        }

        // Bound the first_packet map: anything older than the latest
        // completed-or-evicted frame_id is stale.
        if first_packet.len() > REASM_MAX_PENDING * 4
            && let Some(latest) = last_complete_id
        {
            first_packet.retain(|&fid, _| {
                let diff = fid.wrapping_sub(latest);
                diff < u32::MAX / 2
            });
        }
    }
    Ok(stats)
}

/// Send loop: drains an `InputCommand` channel and sends each command on
/// the connected socket back to the host.
pub async fn run_input_sender(
    socket: Arc<UdpSocket>,
    mut rx: mpsc::Receiver<crate::input::InputCommand>,
) {
    use crate::input::InputCommand;
    let mut input_sent = 0u64;
    let mut control_sent = 0u64;
    let mut send_errors = 0u64;
    let mut input_buf = [0u8; input::SIZE];
    let mut ctrl_buf = [0u8; control::SIZE];
    while let Some(cmd) = rx.recv().await {
        let res = match cmd {
            InputCommand::Input(p) => {
                p.write_to(&mut input_buf);
                socket.send(&input_buf).await.map(|_| input_sent += 1)
            }
            InputCommand::Control(c) => {
                c.write_to(&mut ctrl_buf);
                socket.send(&ctrl_buf).await.map(|_| control_sent += 1)
            }
        };
        if let Err(e) = res {
            send_errors += 1;
            tracing::warn!(error = %e, "input/control send failed");
        }
    }
    tracing::info!(
        input_sent,
        control_sent,
        send_errors,
        "input sender stopped"
    );
}
