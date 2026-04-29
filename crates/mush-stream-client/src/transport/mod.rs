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

/// Drives the video receive loop. Owns the UDP socket and reassembler;
/// every full frame goes to `out`. Runs until `out` is dropped.
pub async fn run_video_receiver(
    bind: SocketAddr,
    out: mpsc::Sender<ReassembledFrame>,
) -> std::io::Result<ReceiverStats> {
    let socket = UdpSocket::bind(bind).await?;
    tracing::info!(%bind, "video receiver ready");
    let mut reasm = VideoReassembler::new(REASM_MAX_PENDING);
    let mut buf = vec![0u8; video::MAX_DATAGRAM];
    let mut stats = ReceiverStats::default();

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
