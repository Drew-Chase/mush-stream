//! `client` — receives video over UDP, decodes via ffmpeg
//! (h264_cuvid → sw fallback), and presents via winit + pixels.
//!
//! The orchestration lives in `mush_stream_client::runner` so the
//! Tauri desktop shell can drive the same pipeline in-process.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use mush_stream_client::config::Config;
use mush_stream_client::runner;
use tracing_subscriber::EnvFilter;

/// `client` — receives streamed video over UDP, decodes via
/// ffmpeg, presents via winit + pixels, and forwards gamepad input to the
/// host.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Path to the client TOML config.
    #[arg(default_value = "./client.toml")]
    config: PathBuf,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let config_path = cli.config;
    tracing::info!(path = %config_path.display(), "loading config");
    let cfg = Config::load(&config_path)
        .with_context(|| format!("loading client config from {}", config_path.display()))?;

    // Spawn the runner thread + block on it. The binary runs everything
    // through the same path the Tauri shell uses; the dedicated runner
    // thread (with `with_any_thread(true)` on Windows) means the main
    // thread is free of winit's traditional "main-thread-only" rule.
    let session = runner::start_client_session(cfg, None)?;
    session.join()?;
    tracing::info!("client exiting");
    Ok(())
}
