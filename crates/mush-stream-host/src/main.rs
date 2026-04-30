//! `mush-stream-host` — captures a screen region, encodes it, and streams it
//! to a remote client over UDP.
//!
//! Three modes:
//! - default (M4+): capture → NVENC → UDP send to peer.
//! - `--mp4` (M2): capture → NVENC → MP4 file. Verification mode.
//! - `--png` (M1): capture one frame, write a PNG. Quick crop-rect check.

mod audio;
mod capture;
mod config;
mod encode;
mod transport;
mod upnp;
mod vigem;

use std::{
    fs::File,
    io::BufWriter,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use clap::Parser;
use mush_stream_common::protocol::{
    control::ControlMessage, input::InputPacket, video::VideoFramer,
};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

use crate::capture::{CaptureError, CaptureRect, Capturer};
use crate::config::Config;
use crate::encode::{Mp4Recorder, VideoEncoder};
use crate::transport::{run_host_socket, VIDEO_SEND_CHANNEL};
use crate::upnp::UpnpForward;
use crate::vigem::VirtualGamepad;

const PNG_OUTPUT_PATH: &str = "./capture-debug.png";
const MP4_OUTPUT_PATH: &str = "./capture-debug.mp4";
/// First-frame ramp: DXGI Desktop Duplication needs a few acquisitions
/// before delivering content (the compositor primes its state). 60
/// attempts × 16ms timeout each = ~1s, plenty.
const FIRST_FRAME_MAX_ATTEMPTS: u32 = 60;
/// Steady-state capture timeout for the streaming hot loop. Just one
/// attempt — the rate-limit at the bottom of the loop owns frame
/// pacing, and waiting up to ~1s here would freeze the stream during
/// any static-screen window.
const STREAM_ACQUIRE_ATTEMPTS: u32 = 1;
const RECORD_SECONDS: u32 = 5;

/// Reed-Solomon parity ratio applied per frame. 0.10 = ~10% extra parity
/// packets; any single-packet drop in a frame is recoverable on the
/// receive side without waiting for a re-keyframe. The wire overhead is
/// higher than 10% (parity pads every data packet to `MAX_PAYLOAD`), but
/// for low-latency streaming the bandwidth cost beats the visible
/// corruption from un-recovered loss. Set to 0.0 here to disable.
const FEC_PARITY_RATIO: f32 = 0.10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Stream,
    Mp4,
    Png,
}

/// `mush-stream-host` — desktop capture, NVENC encode, UDP stream to client.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Stream video over UDP to the configured peer (default).
    #[arg(long, group = "mode")]
    stream: bool,
    /// Record 5 seconds of capture to ./capture-debug.mp4 (M2 verification).
    #[arg(long, group = "mode")]
    mp4: bool,
    /// Capture one frame to ./capture-debug.png (M1 verification of the
    /// crop rectangle).
    #[arg(long, group = "mode")]
    png: bool,
    /// Path to the host TOML config.
    #[arg(default_value = "./host.toml")]
    config: PathBuf,
}

impl Cli {
    fn mode(&self) -> Mode {
        if self.png {
            Mode::Png
        } else if self.mp4 {
            Mode::Mp4
        } else {
            // Default and explicit --stream.
            Mode::Stream
        }
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let mode = cli.mode();
    let config_path = cli.config;
    tracing::info!(path = %config_path.display(), ?mode, "loading config");
    let cfg = Config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    let rect = CaptureRect {
        x: cfg.capture.x,
        y: cfg.capture.y,
        width: cfg.capture.width,
        height: cfg.capture.height,
    };
    tracing::info!(
        output_index = cfg.capture.output_index,
        x = rect.x,
        y = rect.y,
        width = rect.width,
        height = rect.height,
        "initializing DXGI capture"
    );

    match mode {
        Mode::Png => capture_to_png(cfg.capture.output_index, rect),
        Mode::Mp4 => record_to_mp4(cfg.capture.output_index, rect, &cfg.encode),
        Mode::Stream => run_stream(cfg, rect),
    }
}

/// M1: capture one frame, save as PNG.
fn capture_to_png(output_index: u32, rect: CaptureRect) -> Result<()> {
    let mut capturer = Capturer::new(output_index, rect)
        .context("initializing DXGI desktop duplication capturer")?;
    let bgra = capturer
        .next_frame_bgra(FIRST_FRAME_MAX_ATTEMPTS)
        .context("acquiring first desktop frame")?;
    write_bgra_as_png(rect.width, rect.height, bgra, PNG_OUTPUT_PATH)
        .with_context(|| format!("writing PNG to {PNG_OUTPUT_PATH}"))?;
    tracing::info!(path = PNG_OUTPUT_PATH, "PNG written; verify the crop region");
    Ok(())
}

/// M2: capture+encode 5 seconds of video to MP4.
fn record_to_mp4(
    output_index: u32,
    rect: CaptureRect,
    enc_cfg: &crate::config::EncodeConfig,
) -> Result<()> {
    let fps = enc_cfg.fps;
    let bitrate_bps = u64::from(enc_cfg.bitrate_kbps) * 1000;
    let total_frames = i64::from(fps) * i64::from(RECORD_SECONDS);

    let mut capturer = Capturer::new(output_index, rect)
        .context("initializing DXGI desktop duplication capturer")?;
    let mut recorder = Mp4Recorder::new(
        Path::new(MP4_OUTPUT_PATH),
        rect.width,
        rect.height,
        fps,
        bitrate_bps,
    )
    .context("initializing NVENC encoder + MP4 muxer")?;

    tracing::info!(
        fps,
        bitrate_bps,
        total_frames,
        path = MP4_OUTPUT_PATH,
        "recording {RECORD_SECONDS} seconds"
    );

    let frame_size = (rect.width as usize) * (rect.height as usize) * 4;
    let mut last_frame = vec![0u8; frame_size];
    let mut have_first_frame = false;

    for pts in 0..total_frames {
        match capturer.next_frame_bgra(FIRST_FRAME_MAX_ATTEMPTS) {
            Ok(bgra) => {
                last_frame.copy_from_slice(bgra);
                have_first_frame = true;
            }
            Err(CaptureError::FirstFrameTimeout) if have_first_frame => {}
            Err(e) => return Err(e).context(format!("capturing frame {pts}")),
        }
        recorder
            .push_bgra(&last_frame, pts)
            .with_context(|| format!("encoding frame {pts}"))?;
    }
    recorder.finish().context("finalizing MP4")?;
    tracing::info!(path = MP4_OUTPUT_PATH, "recording complete; verify in VLC");
    Ok(())
}

/// M4+: capture+encode → UDP stream to peer. Runs until Ctrl+C.
#[allow(clippy::needless_pass_by_value)] // owns config + rect for the runtime lifetime
fn run_stream(cfg: Config, rect: CaptureRect) -> Result<()> {
    // Optional UPnP port forwarding for the listen socket so a remote
    // client behind NAT can reach the host without manual port
    // forwarding. Held for the lifetime of `run_stream`; Drop unmaps.
    let _upnp_guard = if cfg.network.enable_upnp {
        UpnpForward::try_forward_udp(cfg.network.listen_port, "mush-stream-host")
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
        // Channels for the inbound dispatcher to forward to the right
        // worker. Bounded so a stalled worker doesn't grow memory.
        let (gamepad_tx, gamepad_rx) = std::sync::mpsc::sync_channel::<InputPacket>(256);
        let (control_tx, control_rx) = std::sync::mpsc::sync_channel::<ControlMessage>(64);

        let listen_port = cfg.network.listen_port;
        // 1.25× headroom over the encoder's target so the pacer doesn't
        // stall the queue if NVENC briefly overshoots.
        let pacer_bps = (u64::from(cfg.encode.bitrate_kbps) * 1000 / 8) * 5 / 4;

        // Unified host socket: send video to discovered peer, receive
        // input/control from same socket.
        let host_sock = tokio::spawn(async move {
            match run_host_socket(listen_port, datagram_rx, inbound_tx, pacer_bps).await {
                Ok(stats) => tracing::info!(?stats, "host socket stopped"),
                Err(e) => tracing::error!(error = %e, "host socket failed"),
            }
        });

        // Shared shutdown flag — Ctrl+C sets it; the encode thread reads it.
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_for_capture = shutdown.clone();

        // Encode/capture in a dedicated OS thread (DXGI + NVENC are sync).
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

        // Audio thread: WASAPI loopback → Opus → datagram_tx. Off by
        // default-true config; opt out via [audio] enabled = false.
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

        // ViGEm thread: applies received InputPackets to a virtual Xbox 360.
        // Connects lazily — if the driver is missing, log and skip without
        // breaking video streaming.
        let vigem_handle = std::thread::Builder::new()
            .name("mush-vigem".into())
            .spawn(move || {
                run_vigem_loop(gamepad_rx);
            })
            .context("spawning vigem thread")?;

        // Inbound dispatcher: route Input packets to the ViGEm thread and
        // ControlMessages to the encode thread (request_keyframe / shutdown).
        let inbound_handle = tokio::spawn(async move {
            while let Some(msg) = inbound_rx.recv().await {
                match msg {
                    transport::InboundFromClient::Control(c) => {
                        tracing::debug!(?c, "control message from client");
                        let _ = control_tx.try_send(c);
                    }
                    transport::InboundFromClient::Input(p) => {
                        // try_send so a stalled ViGEm doesn't back-pressure
                        // the UDP receiver. Drop on full is acceptable for
                        // 250Hz polling — next packet supersedes anyway.
                        let _ = gamepad_tx.try_send(p);
                    }
                }
            }
        });

        // Wait for Ctrl+C, then signal shutdown.
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!(error = %e, "ctrl_c handler failed; running until external kill");
        }
        tracing::info!("shutdown requested");
        shutdown.store(true, Ordering::Release);

        // Wait for the encode thread to drop the channel sender (closes the
        // network task naturally). spawn_blocking lets us await std::thread.
        let _ = tokio::task::spawn_blocking(move || {
            let _ = encode_handle.join();
            let _ = vigem_handle.join();
            if let Some(h) = audio_handle {
                let _ = h.join();
            }
        })
        .await;
        host_sock.abort();
        inbound_handle.abort();

        anyhow::Ok(())
    })
}

/// The synchronous capture+encode hot loop, run in a dedicated thread.
#[allow(clippy::needless_pass_by_value)] // long-running thread entry; owns its inputs
#[allow(clippy::too_many_lines)] // single coherent loop; refactor would scatter state
fn run_capture_encode_loop(
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
    // Per-second throughput counters (reset each tick) so info-level
    // logs can attribute pauses without enabling debug.
    let mut frames_this_sec: u64 = 0;
    let mut nal_bytes_this_sec: u64 = 0;
    let mut last_log = std::time::Instant::now();
    let mut max_iter_us: u64 = 0;

    // Cap the capture+encode rate to the configured fps. DXGI Desktop
    // Duplication delivers frames at the host's monitor refresh (often
    // 144/165/240 Hz on a gaming rig); without this the encoder is fed
    // 2-4x faster than its time_base assumes, which scrambles NVENC's
    // bit-budget controller and the decoder produces broken references.
    let frame_interval = std::time::Duration::from_micros(1_000_000 / u64::from(fps));
    let mut next_deadline = std::time::Instant::now() + frame_interval;

    while !shutdown.load(Ordering::Acquire) {
        let iter_start = std::time::Instant::now();
        // Drain pending control messages before encoding the next frame.
        // Coalesce: multiple RequestKeyframes between frames just flag
        // keyframe once.
        while let Ok(msg) = control_rx.try_recv() {
            match msg {
                ControlMessage::RequestKeyframe => {
                    encoder.request_keyframe();
                    keyframes_forced += 1;
                }
                ControlMessage::Disconnect => {
                    tracing::info!("client requested disconnect; encode loop exiting");
                    shutdown.store(true, Ordering::Release);
                }
            }
        }

        // First call gets the long ramp timeout; subsequent calls only
        // wait one frame interval. If DXGI has nothing new (static
        // screen) we fall through with `last_frame` unchanged and the
        // encoder repeats the previous BGRA — no multi-second freeze.
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
                // Repeat last frame; keeps PTS monotonic for clients that
                // assume a steady cadence.
            }
            Err(CaptureError::FirstFrameTimeout) => {
                continue; // Still waiting for the very first frame; loop.
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
                let emit = |datagram: &[u8]| {
                    match datagram_tx.try_send(datagram.to_vec()) {
                        // Closed: network task gone, nothing more we can
                        // do — same path as Ok at this layer.
                        Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            dropped_full_channel += 1;
                        }
                    }
                };
                if FEC_PARITY_RATIO > 0.0 {
                    if let Err(e) = framer.frame_with_fec(
                        nal,
                        timestamp_us,
                        is_keyframe,
                        FEC_PARITY_RATIO,
                        emit,
                    ) {
                        // RS rejects N+K > 256 (galois_8). Falling back
                        // would need framer to be re-borrowed in this
                        // closure — easier to just log and let the next
                        // frame come through; the client will request a
                        // keyframe on the resulting gap.
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

        // Per-iteration cost: useful for spotting stalls.
        let iter_us = u64::try_from(iter_start.elapsed().as_micros()).unwrap_or(u64::MAX);
        max_iter_us = max_iter_us.max(iter_us);

        // Once per second emit a throughput line. If a pause shows up at
        // the user, this lets us see whether the host stopped producing
        // (counter goes to zero) or the host kept producing and the
        // pause is downstream.
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

        // Pace to the configured fps. Sleep until the next frame's
        // deadline; if we fell behind (long encode, GC, scheduling), reset
        // so we don't burst-catch-up.
        let now = std::time::Instant::now();
        if now < next_deadline {
            std::thread::sleep(next_deadline - now);
        }
        next_deadline += frame_interval;
        if next_deadline < std::time::Instant::now() {
            next_deadline = std::time::Instant::now() + frame_interval;
        }
    }

    // Drain encoder on shutdown.
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
#[allow(clippy::needless_pass_by_value)] // long-running thread entry; owns its receiver
fn run_vigem_loop(rx: std::sync::mpsc::Receiver<InputPacket>) {
    let mut pad = match VirtualGamepad::connect() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "ViGEm connect failed; gamepad passthrough disabled \
                (install ViGEmBus and reconnect to enable)"
            );
            // Drain the channel so the inbound dispatcher's try_send doesn't
            // back up, but otherwise do nothing.
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

/// Convert tightly-packed BGRA → RGBA into a fresh buffer and write as PNG.
fn write_bgra_as_png(width: u32, height: u32, bgra: &[u8], path: &str) -> Result<()> {
    let pixels = (width as usize) * (height as usize);
    anyhow::ensure!(
        bgra.len() == pixels * 4,
        "BGRA buffer length {} does not match {}x{}*4 = {}",
        bgra.len(),
        width,
        height,
        pixels * 4
    );
    let mut rgba = Vec::with_capacity(pixels * 4);
    for chunk in bgra.chunks_exact(4) {
        rgba.push(chunk[2]); // R
        rgba.push(chunk[1]); // G
        rgba.push(chunk[0]); // B
        rgba.push(chunk[3]); // A
    }

    let file = File::create(path).with_context(|| format!("creating {path}"))?;
    let writer = BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().context("writing PNG header")?;
    writer.write_image_data(&rgba).context("writing PNG data")?;
    Ok(())
}
