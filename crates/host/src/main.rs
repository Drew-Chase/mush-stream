//! `app-host` — captures a screen region, encodes it, and streams it
//! to a remote client over UDP.
//!
//! Three modes:
//! - default (M4+): capture → NVENC → UDP send to peer.
//! - `--mp4` (M2): capture → NVENC → MP4 file. Verification mode.
//! - `--png` (M1): capture one frame, write a PNG. Quick crop-rect check.
//!
//! The streaming orchestration lives in `mush_stream_host::runner` so the
//! Tauri desktop shell can drive the same pipeline in-process.

use std::{
    fs::File,
    io::BufWriter,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    sync::Arc,
};

use anyhow::{Context, Result};
use clap::Parser;
use mush_stream_host::capture::{CaptureError, CaptureRect, Capturer};
use mush_stream_host::config::Config;
use mush_stream_host::encode::Mp4Recorder;
use mush_stream_host::{audio, runner};
use tracing_subscriber::EnvFilter;

const PNG_OUTPUT_PATH: &str = "./capture-debug.png";
const MP4_OUTPUT_PATH: &str = "./capture-debug.mp4";
/// First-frame ramp: DXGI Desktop Duplication needs a few acquisitions
/// before delivering content (the compositor primes its state). 60
/// attempts × 16ms timeout each = ~1s, plenty.
const FIRST_FRAME_MAX_ATTEMPTS: u32 = 60;
const RECORD_SECONDS: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Stream,
    Mp4,
    Png,
    ListAudioSessions,
}

/// `app-host` — desktop capture, NVENC encode, UDP stream to client.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
#[allow(clippy::struct_excessive_bools)] // mode flags are mutually exclusive via `group = "mode"`
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
    /// List the audio sessions on the default render endpoint and exit.
    /// Use this to discover the right process name to add under
    /// `[audio].blacklist` in `host.toml`.
    #[arg(long, group = "mode")]
    list_audio_sessions: bool,
    /// Path to the host TOML config.
    #[arg(default_value = "./host.toml")]
    config: PathBuf,
}

impl Cli {
    fn mode(&self) -> Mode {
        if self.list_audio_sessions {
            Mode::ListAudioSessions
        } else if self.png {
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

    // ListAudioSessions doesn't need the config — it's a discovery tool
    // users run before they've authored host.toml.
    if mode == Mode::ListAudioSessions {
        return audio::list_audio_sessions().context("enumerating audio sessions");
    }

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
        Mode::Stream => {
            // Binary entry point: ask the runner to install Ctrl+C
            // handling for us. We still own the shutdown atomic so
            // upstream callers (or future callers using this same
            // pattern) can flip it externally.
            let shutdown = Arc::new(AtomicBool::new(false));
            runner::run_stream_blocking(cfg, rect, shutdown, true)
        }
        Mode::ListAudioSessions => unreachable!("handled above before config load"),
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
    enc_cfg: &mush_stream_host::config::EncodeConfig,
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
