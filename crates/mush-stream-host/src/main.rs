//! `mush-stream-host` — captures a screen region, encodes it (M2+), and streams
//! it to a remote client (M3+).
//!
//! Two modes:
//! - default: record 5 seconds of video to `./capture-debug.mp4` via NVENC
//!   for milestone-2 verification.
//! - `--png`: capture one frame and write it to `./capture-debug.png`. The
//!   M1 verification path, retained for quick crop-rect debugging.

mod capture;
mod config;
mod encode;

use std::{
    ffi::OsString,
    fs::File,
    io::BufWriter,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

use crate::capture::{CaptureError, CaptureRect, Capturer};
use crate::config::Config;
use crate::encode::Mp4Recorder;

const DEFAULT_CONFIG_PATH: &str = "./host.toml";
const PNG_OUTPUT_PATH: &str = "./capture-debug.png";
const MP4_OUTPUT_PATH: &str = "./capture-debug.mp4";
const FIRST_FRAME_MAX_ATTEMPTS: u32 = 60;
const RECORD_SECONDS: u32 = 5;

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
    }
}

#[derive(Debug, Clone, Copy)]
enum Mode {
    Mp4,
    Png,
}

fn parse_args<I, S>(args: I) -> (Mode, PathBuf)
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut mode = Mode::Mp4;
    let mut config_path: Option<PathBuf> = None;
    for arg in args.into_iter().skip(1) {
        let arg: OsString = arg.into();
        if arg == "--png" {
            mode = Mode::Png;
        } else {
            config_path = Some(PathBuf::from(arg));
        }
    }
    (mode, config_path.unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH)))
}

/// M1: capture one frame, save it as PNG. Useful for verifying the crop rect.
fn capture_to_png(output_index: u32, rect: CaptureRect) -> Result<()> {
    let mut capturer = Capturer::new(output_index, rect)
        .context("initializing DXGI desktop duplication capturer")?;

    tracing::info!("waiting for first desktop frame");
    let bgra = capturer
        .next_frame_bgra(FIRST_FRAME_MAX_ATTEMPTS)
        .context("acquiring first desktop frame")?;

    write_bgra_as_png(rect.width, rect.height, bgra, PNG_OUTPUT_PATH)
        .with_context(|| format!("writing PNG to {PNG_OUTPUT_PATH}"))?;

    tracing::info!(
        path = PNG_OUTPUT_PATH,
        width = rect.width,
        height = rect.height,
        "PNG written; open it to verify the crop region"
    );
    Ok(())
}

/// M2: capture+encode 5 seconds of video to MP4 via NVENC.
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
        "recording {RECORD_SECONDS} seconds; if the screen is static, frames may be duplicated"
    );

    // Reused frame buffer so capture timeouts can repeat the last good frame
    // (DXGI returns no frame when the screen hasn't changed).
    let frame_size = (rect.width as usize) * (rect.height as usize) * 4;
    let mut last_frame = vec![0u8; frame_size];
    let mut have_first_frame = false;
    let mut duplicate_count = 0u32;

    for pts in 0..total_frames {
        match capturer.next_frame_bgra(FIRST_FRAME_MAX_ATTEMPTS) {
            Ok(bgra) => {
                last_frame.copy_from_slice(bgra);
                have_first_frame = true;
            }
            Err(CaptureError::FirstFrameTimeout) if have_first_frame => {
                duplicate_count += 1;
            }
            Err(e) => {
                return Err(e).context(format!("capturing frame {pts}"));
            }
        }
        recorder
            .push_bgra(&last_frame, pts)
            .with_context(|| format!("encoding frame {pts}"))?;
        if pts > 0 && pts % i64::from(fps) == 0 {
            tracing::debug!(seconds_elapsed = pts / i64::from(fps), "...");
        }
    }

    recorder.finish().context("finalizing MP4")?;

    tracing::info!(
        path = MP4_OUTPUT_PATH,
        total_frames,
        duplicate_count,
        "recording complete; verify with VLC or ffprobe"
    );
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
