//! UDP transport for the host.
//!
//! One socket. The host doesn't pre-configure where to send video; it
//! learns the client's address from `recv_from`'s source field on the
//! first incoming packet (input or control), and then sends video back
//! to that address. UDP hole-punching takes care of the NAT return path
//! whenever the client initiated, so a host-side UPnP forward of the
//! listen port is the only thing needed for cross-NAT play.
//!
//! Dispatched on the receive side by datagram size:
//! - 16 bytes → [`InputPacket`]
//! - 1 byte  → [`ControlMessage`]
//! - anything else → logged + dropped
//!
//! Sends are paced through a [`TokenBucket`] so encoder bursts don't
//! micro-flood the receiver.

pub mod pacer;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

pub use self::pacer::TokenBucket;

use mush_stream_common::protocol::{
    control::{self, ControlMessage},
    input::{self, InputPacket},
};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};

/// Finished video datagram (header + NAL fragment), ready for `socket.send_to`.
pub type VideoDatagram = Vec<u8>;

/// Channel size for video send: ~150ms of headroom at 60fps×8 packets/frame.
pub const VIDEO_SEND_CHANNEL: usize = 256;

/// 8 MiB UDP receive buffer for the host's listening socket. Inbound
/// volume is small (input + control) but mirroring the client's setting
/// keeps either side from being a surprise bottleneck.
pub const UDP_RECV_BUFFER_BYTES: usize = 8 * 1024 * 1024;

/// One parsed message coming up from the listen socket.
#[derive(Debug)]
pub enum InboundFromClient {
    Input(InputPacket),
    Control(ControlMessage),
}

/// Stats reported by the host transport on shutdown.
#[derive(Debug, Default, Clone, Copy)]
pub struct TransportStats {
    pub packets_sent: u64,
    pub bytes_sent: u64,
    pub send_errors: u64,
    pub packets_received: u64,
    pub bytes_received: u64,
    pub recv_errors: u64,
    pub malformed: u64,
}

fn bind_listen_socket(listen_port: u16) -> std::io::Result<UdpSocket> {
    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), listen_port);
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
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

/// Drives the unified host socket: binds `listen_port`, runs an inbound
/// dispatcher (input/control), runs the paced video send loop targeting
/// the most-recently-seen client address. Returns when either the
/// `datagram_rx` closes (encoder shut down) or `inbound_tx` closes.
pub async fn run_host_socket(
    listen_port: u16,
    datagram_rx: mpsc::Receiver<VideoDatagram>,
    inbound_tx: mpsc::Sender<InboundFromClient>,
    target_bps: u64,
) -> std::io::Result<TransportStats> {
    let socket = Arc::new(bind_listen_socket(listen_port)?);
    let (peer_tx, peer_rx) = watch::channel::<Option<SocketAddr>>(None);
    tracing::info!(
        listen_port,
        recv_buffer = UDP_RECV_BUFFER_BYTES,
        target_bps,
        "host socket bound; waiting for first client packet to learn peer"
    );

    let recv_socket = socket.clone();
    let recv_handle = tokio::spawn(run_recv_loop(recv_socket, peer_tx, inbound_tx));

    let send_handle = tokio::spawn(run_send_loop(socket, peer_rx, datagram_rx, target_bps));

    // First task to finish wins. Both eventually return TransportStats
    // halves — merge them.
    let (recv_res, send_res) = tokio::join!(recv_handle, send_handle);
    let mut stats = TransportStats::default();
    match recv_res {
        Ok(Ok(s)) => {
            stats.packets_received = s.packets_received;
            stats.bytes_received = s.bytes_received;
            stats.recv_errors = s.recv_errors;
            stats.malformed = s.malformed;
        }
        Ok(Err(e)) => return Err(e),
        Err(e) => tracing::warn!(error = %e, "recv loop panicked"),
    }
    match send_res {
        Ok(Ok(s)) => {
            stats.packets_sent = s.packets_sent;
            stats.bytes_sent = s.bytes_sent;
            stats.send_errors = s.send_errors;
        }
        Ok(Err(e)) => return Err(e),
        Err(e) => tracing::warn!(error = %e, "send loop panicked"),
    }
    Ok(stats)
}

async fn run_recv_loop(
    socket: Arc<UdpSocket>,
    peer_tx: watch::Sender<Option<SocketAddr>>,
    inbound_tx: mpsc::Sender<InboundFromClient>,
) -> std::io::Result<TransportStats> {
    let mut stats = TransportStats::default();
    let mut buf = [0u8; 64];
    let mut last_peer: Option<SocketAddr> = None;
    loop {
        let (len, src) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                stats.recv_errors += 1;
                tracing::warn!(error = %e, "recv_from failed");
                continue;
            }
        };
        stats.packets_received += 1;
        stats.bytes_received += len as u64;

        // Track peer changes: usually one client per host, but if the
        // client reconnects from a new ephemeral port we should switch.
        if last_peer != Some(src) {
            tracing::info!(peer = %src, "client peer registered");
            last_peer = Some(src);
            // Best-effort: if no receivers, nothing to do.
            let _ = peer_tx.send(Some(src));
        }

        let datagram = &buf[..len];
        let parsed = match len {
            input::SIZE => InputPacket::read_from(datagram)
                .map(InboundFromClient::Input)
                .map_err(|e| e.to_string()),
            control::SIZE => ControlMessage::read_from(datagram)
                .map(InboundFromClient::Control)
                .map_err(|e| e.to_string()),
            other => Err(format!("unexpected datagram size {other}")),
        };
        match parsed {
            Ok(msg) => {
                if inbound_tx.send(msg).await.is_err() {
                    break;
                }
            }
            Err(reason) => {
                stats.malformed += 1;
                tracing::debug!(%src, len, %reason, "ignoring malformed inbound datagram");
            }
        }
    }
    Ok(stats)
}

async fn run_send_loop(
    socket: Arc<UdpSocket>,
    peer_rx: watch::Receiver<Option<SocketAddr>>,
    mut datagram_rx: mpsc::Receiver<VideoDatagram>,
    target_bps: u64,
) -> std::io::Result<TransportStats> {
    let mut stats = TransportStats::default();
    // ~12 packets at 1400 bytes each = 16 800 bytes. Lets short encoder
    // bursts go out at line rate while still smoothing the average.
    let mut pacer = if target_bps > 0 {
        Some(TokenBucket::new(16_800, target_bps))
    } else {
        None
    };
    let mut warned_no_peer = false;
    while let Some(datagram) = datagram_rx.recv().await {
        let peer = *peer_rx.borrow();
        let Some(peer) = peer else {
            if !warned_no_peer {
                tracing::info!(
                    "video frame produced before client connected; \
                    dropping until client sends its first packet"
                );
                warned_no_peer = true;
            }
            continue;
        };
        if let Some(b) = pacer.as_mut() {
            b.take(datagram.len() as u64).await;
        }
        match socket.send_to(&datagram, peer).await {
            Ok(n) => {
                stats.packets_sent += 1;
                stats.bytes_sent += n as u64;
            }
            Err(e) => {
                stats.send_errors += 1;
                tracing::warn!(error = %e, %peer, "UDP send_to failed");
            }
        }
    }
    Ok(stats)
}
