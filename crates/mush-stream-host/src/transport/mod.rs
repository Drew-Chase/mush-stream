//! UDP transport for the host.
//!
//! Bidirectional channels:
//! - **video host→client** ([`run_video_sender`]): receives finished UDP
//!   datagrams via an mpsc channel (the encode thread fragments NAL units
//!   into datagrams via [`mush_stream_common::protocol::video::VideoFramer`]
//!   and pushes them in) and sends them to the configured peer address.
//! - **input + control client→host** ([`run_input_receiver`]): listens on
//!   the input port, dispatches by datagram size — 16 bytes is an
//!   [`InputPacket`], 1 byte is a [`ControlMessage`] — and forwards parsed
//!   events to the rest of the host via an mpsc channel.

use std::net::SocketAddr;

use mush_stream_common::protocol::{
    self,
    control::{self, ControlMessage},
    input::{self, InputPacket},
};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

/// Finished video datagram (header + NAL fragment), ready for `socket.send`.
pub type VideoDatagram = Vec<u8>;

/// Channel size for video send: ~150ms of headroom at 60fps×8 packets/frame.
pub const VIDEO_SEND_CHANNEL: usize = 256;

/// Drives the video send loop. Owns the UDP socket; reads datagrams from
/// `rx` and `socket.send`s each one to `peer`. Runs until `rx` closes.
pub async fn run_video_sender(
    bind: SocketAddr,
    peer: SocketAddr,
    mut rx: mpsc::Receiver<VideoDatagram>,
) -> std::io::Result<TransportStats> {
    let socket = UdpSocket::bind(bind).await?;
    socket.connect(peer).await?;
    tracing::info!(%bind, %peer, "video sender ready");

    let mut stats = TransportStats::default();
    while let Some(datagram) = rx.recv().await {
        match socket.send(&datagram).await {
            Ok(n) => {
                stats.packets_sent += 1;
                stats.bytes_sent += n as u64;
            }
            Err(e) => {
                stats.send_errors += 1;
                tracing::warn!(error = %e, "UDP send_to failed");
            }
        }
    }
    Ok(stats)
}

/// One parsed message coming up from the input/control port.
#[derive(Debug)]
pub enum InboundFromClient {
    Input(InputPacket),
    Control(ControlMessage),
}

/// Drives the input + control receive loop. Binds `bind`, reads datagrams,
/// dispatches by size, and forwards parsed events to `tx`. Malformed
/// datagrams are logged and dropped.
pub async fn run_input_receiver(
    bind: SocketAddr,
    tx: mpsc::Sender<InboundFromClient>,
) -> std::io::Result<()> {
    let socket = UdpSocket::bind(bind).await?;
    tracing::info!(%bind, "input/control receiver ready");

    // Sized to fit either an InputPacket or any conceivable ControlMessage.
    // 64 bytes is generous; control messages are 1 byte today.
    let mut buf = [0u8; 64];
    loop {
        let (len, peer) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "recv_from failed");
                continue;
            }
        };
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
                if tx.send(msg).await.is_err() {
                    // Receiver dropped — we're shutting down.
                    break;
                }
            }
            Err(reason) => {
                tracing::debug!(%peer, len, %reason, "ignoring malformed inbound datagram");
            }
        }
    }
    Ok(())
}

/// Stats reported by the video sender on shutdown.
#[derive(Debug, Default, Clone, Copy)]
pub struct TransportStats {
    pub packets_sent: u64,
    pub bytes_sent: u64,
    pub send_errors: u64,
}

// Keep the protocol re-export visible so this module is self-documenting in
// rustdoc — users land here looking for "the host transport" and find the
// wire-format types one click away.
#[doc(no_inline)]
pub use protocol::{control::ControlMessage as _, input::InputPacket as _};
