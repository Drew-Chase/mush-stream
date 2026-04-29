//! UDP transport for the client.
//!
//! Mirrors the host transport in reverse:
//! - **video receive** ([`run_video_receiver`]): listens on the configured
//!   port, drives a [`VideoReassembler`], pushes complete reassembled
//!   frames onto an mpsc channel for the decoder.
//! - **input + control send** ([`InputSender`]): tokio UDP socket pre-
//!   connected to the host. Provides typed `send_input` / `send_control`
//!   helpers so M6/M7 don't worry about wire formatting at the call site.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use mush_stream_common::protocol::{
    control::{self, ControlMessage},
    input::{self, InputPacket},
    video::{self, ReassembledFrame, VideoReassembler},
};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

/// How many in-flight pending frames the reassembler will track before
/// evicting the oldest. ~250ms of headroom at 60fps.
pub const REASM_MAX_PENDING: usize = 16;

/// Don't spam keyframe requests on a sustained drop streak — one per this
/// interval is enough to ask the encoder for an IDR.
const KEYFRAME_REQUEST_DEBOUNCE: Duration = Duration::from_millis(200);

/// Drives the video receive loop. Owns the UDP socket and reassembler;
/// every full frame goes to `out`. When a gap in `frame_id` is detected
/// (a missed frame), or when the very first frame arrives without being
/// a keyframe (the client joined an in-flight stream), pushes a
/// `RequestKeyframe` onto `keyframe_tx` so the host's encoder will emit
/// an IDR. Rate-limited per [`KEYFRAME_REQUEST_DEBOUNCE`].
pub async fn run_video_receiver(
    bind: SocketAddr,
    out: mpsc::Sender<ReassembledFrame>,
    keyframe_tx: Option<mpsc::Sender<crate::input::InputCommand>>,
) -> std::io::Result<ReceiverStats> {
    use crate::input::InputCommand;
    let socket = UdpSocket::bind(bind).await?;
    tracing::info!(%bind, "video receiver ready");
    let mut reasm = VideoReassembler::new(REASM_MAX_PENDING);
    let mut buf = vec![0u8; video::MAX_DATAGRAM];
    let mut stats = ReceiverStats::default();
    let mut last_complete_id: Option<u32> = None;
    let mut last_request = Instant::now()
        .checked_sub(KEYFRAME_REQUEST_DEBOUNCE * 2)
        .unwrap_or_else(Instant::now);

    let request_keyframe = |reason: &'static str,
                            last_request: &mut Instant,
                            stats: &mut ReceiverStats| {
        let now = Instant::now();
        if now.duration_since(*last_request) < KEYFRAME_REQUEST_DEBOUNCE {
            return;
        }
        *last_request = now;
        stats.keyframe_requests_sent += 1;
        if let Some(tx) = keyframe_tx.as_ref() {
            let _ = tx.try_send(InputCommand::Control(ControlMessage::RequestKeyframe));
        }
        tracing::info!(reason, "requested keyframe from host");
    };

    loop {
        let (len, _from) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                stats.recv_errors += 1;
                tracing::warn!(error = %e, "recv_from failed");
                continue;
            }
        };
        stats.packets_received += 1;
        stats.bytes_received += len as u64;
        match reasm.ingest(&buf[..len]) {
            Ok(Some(frame)) => {
                stats.frames_completed += 1;
                // Detect loss / mid-stream join.
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
                        // Forward gap: more than one frame_id ahead.
                        // diff in (1, MAX/2) means ahead-and-recent;
                        // exactly 1 means consecutive (no gap).
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
                if out.send(frame).await.is_err() {
                    break;
                }
            }
            Ok(None) => {}
            Err(e) => {
                stats.malformed += 1;
                tracing::debug!(error = %e, "ignoring malformed video packet");
            }
        }
        // Sync per-loop stats from the reassembler so callers can observe
        // them on shutdown without poking internals.
        stats.dropped_old = reasm.dropped_old;
        stats.dropped_evicted = reasm.dropped_evicted;
    }
    Ok(stats)
}

/// Stats reported by the video receiver on shutdown.
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

/// UDP socket pre-connected to the host's input/control listener. Cheap to
/// clone-share via tokio's internal Arc; spawn one per client task.
pub struct InputSender {
    socket: UdpSocket,
}

impl InputSender {
    /// Bind a local ephemeral UDP port and `connect` it to the host so we
    /// can use `socket.send` (no per-call sockaddr).
    pub async fn connect(host_input_addr: SocketAddr) -> std::io::Result<Self> {
        let local: SocketAddr = match host_input_addr {
            SocketAddr::V4(_) => "0.0.0.0:0".parse().unwrap(),
            SocketAddr::V6(_) => "[::]:0".parse().unwrap(),
        };
        let socket = UdpSocket::bind(local).await?;
        socket.connect(host_input_addr).await?;
        tracing::info!(host = %host_input_addr, "input/control sender ready");
        Ok(Self { socket })
    }

    pub async fn send_input(&self, packet: InputPacket) -> std::io::Result<()> {
        let mut buf = [0u8; input::SIZE];
        packet.write_to(&mut buf);
        self.socket.send(&buf).await.map(|_| ())
    }

    pub async fn send_control(&self, msg: ControlMessage) -> std::io::Result<()> {
        let mut buf = [0u8; control::SIZE];
        msg.write_to(&mut buf);
        self.socket.send(&buf).await.map(|_| ())
    }
}

/// Drains the input channel and sends each command to the host. Owns the
/// `InputSender` so the underlying UDP socket lives for the task's
/// lifetime.
pub async fn run_input_sender(
    sender: InputSender,
    mut rx: mpsc::Receiver<crate::input::InputCommand>,
) {
    use crate::input::InputCommand;
    let mut input_sent = 0u64;
    let mut control_sent = 0u64;
    let mut send_errors = 0u64;
    while let Some(cmd) = rx.recv().await {
        let res = match cmd {
            InputCommand::Input(p) => sender.send_input(p).await.map(|_| input_sent += 1),
            InputCommand::Control(c) => sender.send_control(c).await.map(|_| control_sent += 1),
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
