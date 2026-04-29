//! `mush-stream-host` — captures a screen region, encodes it (M2+), and streams
//! it to a remote client (M3+).
//!
//! Milestone 1: capture one cropped frame from the configured monitor and write
//! it to `capture-debug.png` so the user can verify the rectangle visually.

mod capture;
mod config;

use std::{fs::File, io::BufWriter, path::PathBuf};

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

use crate::capture::{CaptureRect, Capturer};
use crate::config::Config;

const DEFAULT_CONFIG_PATH: &str = "./host.toml";
const OUTPUT_PNG_PATH: &str = "./capture-debug.png";
const FIRST_FRAME_MAX_ATTEMPTS: u32 = 60;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let config_path = std::env::args_os()
        .nth(1)
        .map_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH), PathBuf::from);
    tracing::info!(path = %config_path.display(), "loading config");
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

    let mut capturer = Capturer::new(cfg.capture.output_index, rect)
        .context("initializing DXGI desktop duplication capturer")?;

    tracing::info!("waiting for first desktop frame (this may take a moment)");
    let bgra = capturer
        .next_frame_bgra(FIRST_FRAME_MAX_ATTEMPTS)
        .context("acquiring first desktop frame")?;

    write_bgra_as_png(rect.width, rect.height, bgra, OUTPUT_PNG_PATH)
        .with_context(|| format!("writing PNG to {OUTPUT_PNG_PATH}"))?;

    tracing::info!(
        path = OUTPUT_PNG_PATH,
        width = rect.width,
        height = rect.height,
        bytes = bgra.len(),
        "captured frame written; open the PNG to verify the crop region"
    );

    Ok(())
}

/// Convert tightly-packed BGRA → RGBA in place inside a fresh buffer and write
/// it as PNG. Milestone-1-only verification artifact.
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
