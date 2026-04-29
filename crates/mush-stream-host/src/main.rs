//! `mush-stream-host` — captures a screen region, encodes it, and streams it
//! to a remote client over UDP.
//!
//! Three modes:
//! - default (M4+): capture → NVENC → UDP send to peer.
//! - `--mp4` (M2): capture → NVENC → MP4 file. Verification mode.
//! - `--png` (M1): capture one frame, write a PNG. Quick crop-rect check.

mod capture;
mod config;
mod encode;
mod transport;
mod vigem;

use std::{
    ffi::OsString,
    fs::File,
    io::BufWriter,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use mush_stream_common::protocol::{
    control::ControlMessage, input::InputPacket, video::VideoFramer,
};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

use crate::capture::{CaptureError, CaptureRect, Capturer};
use crate::config::Config;
use crate::encode::{Mp4Recorder, VideoEncoder};
use crate::transport::{run_input_receiver, run_video_sender, VIDEO_SEND_CHANNEL};
use crate::vigem::VirtualGamepad;

const DEFAULT_CONFIG_PATH: &str = "./host.toml";
const PNG_OUTPUT_PATH: &str = "./capture-debug.png";
const MP4_OUTPUT_PATH: &str = "./capture-debug.mp4";
const FIRST_FRAME_MAX_ATTEMPTS: u32 = 60;
const RECORD_SECONDS: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Stream,
    Mp4,
    Png,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let (mode, config_path) = parse_args(std::env::args_os());
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

fn parse_args<I, S>(args: I) -> (Mode, PathBuf)
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut mode = Mode::Stream;
    let mut config_path: Option<PathBuf> = None;
    for arg in args.into_iter().skip(1) {
        let arg: OsString = arg.into();
        if arg == "--png" {
            mode = Mode::Png;
        } else if arg == "--mp4" {
            mode = Mode::Mp4;
        } else if arg == "--stream" {
            mode = Mode::Stream;
        } else {
            config_path = Some(PathBuf::from(arg));
        }
    }
    (mode, config_path.unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH)))
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
fn run_stream(cfg: Config, rect: CaptureRect) -> Result<()> {
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

        let video_bind = cfg.network.video_bind;
        let peer = cfg.network.peer;
        let input_bind = cfg.network.input_bind;
        // 1.25× headroom over the encoder's target so the pacer doesn't
        // stall the queue if NVENC briefly overshoots.
        let pacer_bps = (u64::from(cfg.encode.bitrate_kbps) * 1000 / 8) * 5 / 4;

        // UDP send task.
        let sender = tokio::spawn(async move {
            match run_video_sender(video_bind, peer, datagram_rx, pacer_bps).await {
                Ok(stats) => tracing::info!(?stats, "video sender stopped"),
                Err(e) => tracing::error!(error = %e, "video sender failed"),
            }
        });

        // UDP input/control receive task.
        let receiver = tokio::spawn(async move {
            if let Err(e) = run_input_receiver(input_bind, inbound_tx).await {
                tracing::error!(error = %e, "input receiver failed");
            }
        });

        // Shared shutdown flag — Ctrl+C sets it; the encode thread reads it.
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_for_capture = shutdown.clone();

        // Encode/capture in a dedicated OS thread (DXGI + NVENC are sync).
        let output_index = cfg.capture.output_index;
        let fps = cfg.encode.fps;
        let bitrate_bps = u64::from(cfg.encode.bitrate_kbps) * 1000;
        let encode_handle = std::thread::Builder::new()
            .name("mush-encode".into())
            .spawn(move || {
                if let Err(e) = run_capture_encode_loop(
                    output_index,
                    rect,
                    fps,
                    bitrate_bps,
                    datagram_tx,
                    control_rx,
                    shutdown_for_capture,
                ) {
                    tracing::error!(error = %e, "capture+encode loop exited with error");
                }
            })
            .context("spawning capture+encode thread")?;

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
        })
        .await;
        sender.abort();
        receiver.abort();
        inbound_handle.abort();

        anyhow::Ok(())
    })
}

/// The synchronous capture+encode hot loop, run in a dedicated thread.
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

    while !shutdown.load(Ordering::Acquire) {
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

        match capturer.next_frame_bgra(FIRST_FRAME_MAX_ATTEMPTS) {
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
                let nal = match packet.data() {
                    Some(d) => d,
                    None => return Ok(()),
                };
                let is_keyframe = packet
                    .flags()
                    .contains(ffmpeg_the_third::codec::packet::Flags::KEY);
                framer.frame(nal, timestamp_us, is_keyframe, |datagram| {
                    match datagram_tx.try_send(datagram.to_vec()) {
                        Ok(()) => {}
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            dropped_full_channel += 1;
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                            // Network task is gone; nothing more we can do.
                        }
                    }
                });
                Ok(())
            })
            .with_context(|| format!("encoding frame pts={pts}"))?;

        pts = pts.wrapping_add(1);
        if pts > 0 && pts % i64::from(fps) == 0 {
            tracing::debug!(
                seconds = pts / i64::from(fps),
                dropped_full_channel,
                "..."
            );
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
