//! In-process host streaming orchestration.
//!
//! Wraps capture + encode + UDP transport + audio + ViGEm into a single
//! `run_stream_blocking` entry point so callers (the host binary's
//! `main.rs` *and* the Tauri desktop shell) can drive the same
//! pipeline without copy-pasting the threading layout.
//!
//! The function builds its own multi-threaded tokio runtime, spawns
//! the encode/audio/ViGEm OS threads + the network task, and returns
//! when the caller-provided `shutdown` atomic flips. It does **not**
//! install a Ctrl+C handler — set `handle_ctrl_c = true` to opt into
//! that for the binary use case.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use mush_stream_common::protocol::{
    control::ControlMessage, input::InputPacket, video::VideoFramer,
};
use tokio::sync::mpsc;

use crate::audio;
use crate::capture::{CaptureError, CaptureRect, Capturer};
use crate::config::Config;
use crate::encode::VideoEncoder;
use crate::transport::{self, run_host_socket, PeerObserver, VIDEO_SEND_CHANNEL};
use crate::upnp::UpnpForward;
use crate::vigem::VirtualGamepad;

/// First-frame ramp: DXGI Desktop Duplication needs a few acquisitions
/// before delivering content (the compositor primes its state). 60
/// attempts × 16ms timeout each = ~1s, plenty.
const FIRST_FRAME_MAX_ATTEMPTS: u32 = 60;
/// Steady-state capture timeout for the streaming hot loop. Just one
/// attempt — the rate-limit at the bottom of the loop owns frame
/// pacing, and waiting up to ~1s here would freeze the stream during
/// any static-screen window.
const STREAM_ACQUIRE_ATTEMPTS: u32 = 1;
/// Reed-Solomon parity ratio applied per frame. 0.10 = ~10% extra parity
/// packets; any single-packet drop in a frame is recoverable on the
/// receive side without waiting for a re-keyframe.
const FEC_PARITY_RATIO: f32 = 0.10;

/// Drive the full host pipeline until `shutdown` flips to true.
///
/// Blocks the calling thread for the lifetime of the stream. Returns
/// once all worker threads have joined.
///
/// `peer_observer`, when supplied, is called from inside the recv loop
/// each time the bound peer changes (`Some(addr)` on first packet or
/// reconnect, `None` once at session end). The Tauri shell uses this
/// to forward `host:peer` events to the frontend; the binary entry
/// point passes `None` and just logs via the existing trace event.
#[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
// owns inputs for the runtime lifetime; single coherent setup block
pub fn run_stream_blocking(
    cfg: Config,
    rect: CaptureRect,
    shutdown: Arc<AtomicBool>,
    handle_ctrl_c: bool,
    peer_observer: Option<PeerObserver>,
) -> Result<()> {
    // UPnP guard outlives the runtime; Drop unmaps the port forward.
    let _upnp_guard = if cfg.network.enable_upnp {
        UpnpForward::try_forward_udp(cfg.network.listen_port, "app-host")
    } else {
        None
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;

    runtime.block_on(async move {
        let (datagram_tx, datagram_rx) = mpsc::channel(VIDEO_SEND_CHANNEL);
        let (inbound_tx, mut inbound_rx) = mpsc::channel(64);
        let (gamepad_tx, gamepad_rx) =
            std::sync::mpsc::sync_channel::<InputPacket>(256);
        let (control_tx, control_rx) =
            std::sync::mpsc::sync_channel::<ControlMessage>(64);

        let listen_port = cfg.network.listen_port;
        // 1.25× headroom over the encoder's target so the pacer doesn't
        // stall the queue if NVENC briefly overshoots.
        let pacer_bps = (u64::from(cfg.encode.bitrate_kbps) * 1000 / 8) * 5 / 4;

        let peer_observer_for_socket = peer_observer.clone();
        let host_sock = tokio::spawn(async move {
            match run_host_socket(
                listen_port,
                datagram_rx,
                inbound_tx,
                pacer_bps,
                peer_observer_for_socket,
            )
            .await
            {
                Ok(stats) => tracing::info!(?stats, "host socket stopped"),
                Err(e) => tracing::error!(error = %e, "host socket failed"),
            }
        });

        let shutdown_for_capture = shutdown.clone();
        let output_index = cfg.capture.output_index;
        let fps = cfg.encode.fps;
        let bitrate_bps = u64::from(cfg.encode.bitrate_kbps) * 1000;
        let datagram_tx_for_video = datagram_tx.clone();
        let encode_handle = std::thread::Builder::new()
            .name("mush-encode".into())
            .spawn(move || {
                if let Err(e) = run_capture_encode_loop(
                    output_index,
                    rect,
                    fps,
                    bitrate_bps,
                    datagram_tx_for_video,
                    control_rx,
                    shutdown_for_capture,
                ) {
                    tracing::error!(error = %e, "capture+encode loop exited with error");
                }
            })
            .context("spawning capture+encode thread")?;

        let audio_handle = if cfg.audio.enabled {
            let audio_cfg = cfg.audio.clone();
            let audio_tx = datagram_tx.clone();
            let audio_shutdown = shutdown.clone();
            Some(
                std::thread::Builder::new()
                    .name("mush-audio".into())
                    .spawn(move || {
                        if let Err(e) =
                            audio::run_audio_loop(audio_cfg, audio_tx, audio_shutdown)
                        {
                            tracing::error!(error = %e, "audio loop exited with error");
                        }
                    })
                    .context("spawning audio thread")?,
            )
        } else {
            tracing::info!("audio disabled in config; not capturing");
            None
        };

        let vigem_handle = std::thread::Builder::new()
            .name("mush-vigem".into())
            .spawn(move || {
                run_vigem_loop(gamepad_rx);
            })
            .context("spawning vigem thread")?;

        let inbound_handle = tokio::spawn(async move {
            while let Some(msg) = inbound_rx.recv().await {
                match msg {
                    transport::InboundFromClient::Control(c) => {
                        tracing::debug!(?c, "control message from client");
                        let _ = control_tx.try_send(c);
                    }
                    transport::InboundFromClient::Input(p) => {
                        let _ = gamepad_tx.try_send(p);
                    }
                }
            }
        });

        // Wait for shutdown — either the caller-set atomic, or Ctrl+C
        // when the runner is invoked from the binary main entry point.
        // 100ms poll is plenty: shutdown latency on Stop streaming is
        // imperceptible and we avoid pulling watch into the runtime.
        let shutdown_for_wait = shutdown.clone();
        if handle_ctrl_c {
            tokio::select! {
                () = poll_shutdown(shutdown_for_wait) => {}
                ctrl_c = tokio::signal::ctrl_c() => {
                    if let Err(e) = ctrl_c {
                        tracing::warn!(
                            error = %e,
                            "ctrl_c handler failed; running until external kill"
                        );
                    }
                }
            }
        } else {
            poll_shutdown(shutdown_for_wait).await;
        }
        tracing::info!("shutdown requested");
        shutdown.store(true, Ordering::Release);

        // Drop the channel senders the OS worker threads block on
        // *before* joining those threads. The vigem worker does a
        // synchronous `rx.recv()` and only exits once its sender drops,
        // and that sender (`gamepad_tx`) lives inside `inbound_handle`'s
        // future. Joining vigem before this abort would deadlock — and
        // since `host_stop` awaits this whole runtime, the frontend
        // would freeze on "stopping" forever.
        //
        // Aborting alone is fire-and-forget; await the handle so the
        // task is actually polled to completion and its captured
        // sender is dropped before we hand off to spawn_blocking.
        inbound_handle.abort();
        let _ = inbound_handle.await;

        let _ = tokio::task::spawn_blocking(move || {
            let _ = encode_handle.join();
            let _ = vigem_handle.join();
            if let Some(h) = audio_handle {
                let _ = h.join();
            }
        })
        .await;
        // Tear the socket down last so encoder-finish flush packets
        // produced during `encode_handle.join()` actually go out the
        // wire instead of bouncing off a closed datagram_rx.
        host_sock.abort();
        let _ = host_sock.await;

        anyhow::Ok(())
    })
}

async fn poll_shutdown(shutdown: Arc<AtomicBool>) {
    while !shutdown.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The synchronous capture+encode hot loop, run in a dedicated thread.
#[allow(clippy::needless_pass_by_value, clippy::too_many_lines, clippy::too_many_arguments)]
pub fn run_capture_encode_loop(
    output_index: u32,
    rect: CaptureRect,
    fps: u32,
    bitrate_bps: u64,
    datagram_tx: mpsc::Sender<Vec<u8>>,
    control_rx: std::sync::mpsc::Receiver<ControlMessage>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let mut capturer =
        Capturer::new(output_index, rect).context("initializing DXGI capturer")?;
    let mut encoder = VideoEncoder::new(rect.width, rect.height, fps, bitrate_bps, false)
        .context("initializing NVENC encoder")?;
    let mut framer = VideoFramer::new();

    let frame_size = (rect.width as usize) * (rect.height as usize) * 4;
    let mut last_frame = vec![0u8; frame_size];
    let mut have_first_frame = false;
    let mut pts: i64 = 0;
    let mut dropped_full_channel: u64 = 0;
    let mut keyframes_forced: u64 = 0;
    let mut frames_this_sec: u64 = 0;
    let mut nal_bytes_this_sec: u64 = 0;
    let mut last_log = std::time::Instant::now();
    let mut max_iter_us: u64 = 0;

    let frame_interval = std::time::Duration::from_micros(1_000_000 / u64::from(fps));
    let mut next_deadline = std::time::Instant::now() + frame_interval;

    while !shutdown.load(Ordering::Acquire) {
        let iter_start = std::time::Instant::now();
        while let Ok(msg) = control_rx.try_recv() {
            match msg {
                ControlMessage::RequestKeyframe => {
                    encoder.request_keyframe();
                    keyframes_forced += 1;
                }
                // `Disconnect` is intercepted at the transport layer
                // (clears the bound peer without killing the session)
                // and never reaches this channel. Match it here only
                // for exhaustiveness — treat as a no-op if a future
                // refactor reroutes the message.
                ControlMessage::Disconnect => {}
            }
        }

        let attempts = if have_first_frame {
            STREAM_ACQUIRE_ATTEMPTS
        } else {
            FIRST_FRAME_MAX_ATTEMPTS
        };
        match capturer.next_frame_bgra(attempts) {
            Ok(bgra) => {
                last_frame.copy_from_slice(bgra);
                have_first_frame = true;
            }
            Err(CaptureError::FirstFrameTimeout) if have_first_frame => {
                // Repeat last frame.
            }
            Err(CaptureError::FirstFrameTimeout) => {
                continue;
            }
            Err(e) => return Err(e).context("capturing frame"),
        }

        let timestamp_us = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);

        encoder
            .push_bgra(&last_frame, pts, |packet| {
                let Some(nal) = packet.data() else {
                    return Ok(());
                };
                let is_keyframe = packet
                    .flags()
                    .contains(ffmpeg_the_third::codec::packet::Flags::KEY);
                nal_bytes_this_sec = nal_bytes_this_sec.saturating_add(nal.len() as u64);
                let emit = |datagram: &[u8]| match datagram_tx.try_send(datagram.to_vec()) {
                    Ok(())
                    | Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        dropped_full_channel += 1;
                    }
                };
                if FEC_PARITY_RATIO > 0.0 {
                    if let Err(e) =
                        framer.frame_with_fec(nal, timestamp_us, is_keyframe, FEC_PARITY_RATIO, emit)
                    {
                        tracing::warn!(error = %e, "FEC framing failed; frame dropped");
                    }
                } else {
                    framer.frame(nal, timestamp_us, is_keyframe, emit);
                }
                Ok(())
            })
            .with_context(|| format!("encoding frame pts={pts}"))?;
        frames_this_sec += 1;

        pts = pts.wrapping_add(1);

        let iter_us = u64::try_from(iter_start.elapsed().as_micros()).unwrap_or(u64::MAX);
        max_iter_us = max_iter_us.max(iter_us);

        if last_log.elapsed() >= std::time::Duration::from_secs(1) {
            tracing::info!(
                frames = frames_this_sec,
                bytes = nal_bytes_this_sec,
                dropped_full_channel,
                keyframes_forced,
                max_iter_us,
                "host throughput (1s)"
            );
            frames_this_sec = 0;
            nal_bytes_this_sec = 0;
            max_iter_us = 0;
            last_log = std::time::Instant::now();
        }

        let now = std::time::Instant::now();
        if now < next_deadline {
            std::thread::sleep(next_deadline - now);
        }
        next_deadline += frame_interval;
        if next_deadline < std::time::Instant::now() {
            next_deadline = std::time::Instant::now() + frame_interval;
        }
    }

    encoder
        .finish(|packet| {
            if let Some(nal) = packet.data() {
                let is_keyframe = packet
                    .flags()
                    .contains(ffmpeg_the_third::codec::packet::Flags::KEY);
                framer.frame(nal, 0, is_keyframe, |datagram| {
                    let _ = datagram_tx.try_send(datagram.to_vec());
                });
            }
            Ok(())
        })
        .context("flushing encoder on shutdown")?;

    tracing::info!(
        dropped_full_channel,
        keyframes_forced,
        "encode loop exiting"
    );
    Ok(())
}

/// Drains the gamepad input channel, applying each packet to the virtual
/// Xbox 360. Connects lazily; if ViGEm isn't available we log and exit.
#[allow(clippy::needless_pass_by_value)]
pub fn run_vigem_loop(rx: std::sync::mpsc::Receiver<InputPacket>) {
    let mut pad = match VirtualGamepad::connect() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "ViGEm connect failed; gamepad passthrough disabled \
                (install ViGEmBus and reconnect to enable)"
            );
            while rx.recv().is_ok() {}
            return;
        }
    };
    while let Ok(packet) = rx.recv() {
        if let Err(e) = pad.apply(packet) {
            tracing::warn!(error = %e, "ViGEm apply failed");
        }
    }
    tracing::info!(
        accepted = pad.accepted(),
        dropped_old = pad.dropped_old(),
        "ViGEm loop exiting"
    );
}
