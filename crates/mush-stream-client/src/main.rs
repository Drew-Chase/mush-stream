//! `mush-stream-client` — receives video over UDP, decodes via ffmpeg
//! (h264_cuvid → sw fallback), and presents via winit + pixels.
//!
//! M6 will add gamepad capture and an input-send loop on the same client
//! process; the transport layer already exposes `InputSender` for that.
//!
//! Threading layout:
//! - main thread runs the winit event loop (winit requires it).
//! - one std::thread hosts a current-thread tokio runtime and runs the UDP
//!   receiver (reassembly happens in-task).
//! - one std::thread runs the ffmpeg decoder (sync API), draining
//!   reassembled frames via tokio mpsc::Receiver::blocking_recv and pushing
//!   decoded RGBA via `EventLoopProxy::send_event` to the main thread.

mod config;
mod decode;
mod display;
mod input;
mod transport;

use std::{path::PathBuf, sync::atomic::AtomicBool, sync::Arc};

use anyhow::{Context, Result};
use clap::Parser;
use mush_stream_common::protocol::video::ReassembledFrame;
use tracing_subscriber::EnvFilter;
use winit::event_loop::EventLoopProxy;

use crate::config::{Config, DecodeConfig, DisplayConfig};
use crate::decode::VideoDecoder;
use crate::display::{DisplayApp, UserEvent};
use crate::input::{run_gamepad_loop, InputCommand};
use crate::transport::{run_input_sender, run_video_receiver, InputSender};

/// `mush-stream-client` — receives streamed video over UDP, decodes via
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

    let (event_loop, proxy) = display::build_event_loop()?;
    let (frame_tx, frame_rx) =
        tokio::sync::mpsc::channel::<ReassembledFrame>(transport::REASM_MAX_PENDING * 2);

    // Decode thread.
    let proxy_for_decode = proxy.clone();
    let decode_cfg = cfg.decode.clone();
    let display_cfg_for_decode = cfg.display.clone();
    let decode_thread = std::thread::Builder::new()
        .name("mush-decode".into())
        .spawn(move || {
            if let Err(e) =
                run_decode_loop(decode_cfg, display_cfg_for_decode, frame_rx, proxy_for_decode)
            {
                tracing::error!(error = %e, "decode loop exited with error");
            }
        })
        .context("spawning decode thread")?;

    // Channel: input producers (gamepad thread, video receiver via
    // detected-loss → keyframe request) → network thread.
    let (input_tx, input_rx) = tokio::sync::mpsc::channel::<InputCommand>(64);
    let keyframe_tx = input_tx.clone();

    // Gamepad thread (250 Hz). Always spawn; it'll log + exit cleanly if
    // gilrs init fails or no pad is attached.
    let gamepad_shutdown = Arc::new(AtomicBool::new(false));
    let gamepad_shutdown_for_thread = gamepad_shutdown.clone();
    let gamepad_thread = std::thread::Builder::new()
        .name("mush-input".into())
        .spawn(move || {
            if let Err(e) = run_gamepad_loop(input_tx, gamepad_shutdown_for_thread) {
                tracing::error!(error = %e, "gamepad loop exited with error");
            }
        })
        .context("spawning gamepad thread")?;

    // Network thread (own a tokio runtime so async UDP works).
    let video_bind = cfg.network.video_bind;
    let host_input_addr = cfg.network.host_input_addr;
    let net_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("building network runtime")?;
    let net_thread = std::thread::Builder::new()
        .name("mush-net".into())
        .spawn(move || {
            net_runtime.block_on(async move {
                let input_sender = match InputSender::connect(host_input_addr).await {
                    Ok(s) => Some(s),
                    Err(e) => {
                        tracing::error!(error = %e, "input sender bind failed");
                        None
                    }
                };
                let receive = tokio::spawn(async move {
                    match run_video_receiver(video_bind, frame_tx, Some(keyframe_tx)).await {
                        Ok(stats) => tracing::info!(?stats, "video receiver stopped"),
                        Err(e) => tracing::error!(error = %e, "video receiver failed"),
                    }
                });
                let send = tokio::spawn(async move {
                    if let Some(sender) = input_sender {
                        run_input_sender(sender, input_rx).await;
                    }
                });
                let _ = tokio::join!(receive, send);
            });
        })
        .context("spawning network thread")?;

    // Run winit on the main thread.
    let mut app = DisplayApp::new(cfg.display);
    if let Err(e) = event_loop.run_app(&mut app) {
        tracing::error!(error = %e, "event loop exited with error");
    }

    // Window closed. Process exit will tear down the worker threads. Joining
    // them cleanly requires plumbing a shutdown signal through the network
    // runtime — left as M7 hardening since today the OS reclaims everything
    // on process exit anyway.
    drop(proxy);
    gamepad_shutdown.store(true, std::sync::atomic::Ordering::Release);
    let _ = decode_thread; // detach
    let _ = net_thread; // detach
    let _ = gamepad_thread; // detach

    tracing::info!(
        frames_presented = app.stats.frames_presented,
        last_glass_to_glass_us = ?app.stats.last_glass_to_glass_us,
        cumulative_min_us = if app.stats.cumulative_samples > 0 {
            app.stats.cumulative_min_us
        } else {
            0
        },
        cumulative_max_us = app.stats.cumulative_max_us,
        cumulative_avg_us = ?app.stats.cumulative_avg_us(),
        cumulative_samples = app.stats.cumulative_samples,
        "client exiting"
    );
    Ok(())
}

/// Decodes reassembled NAL frames and forwards decoded RGBA frames to the
/// winit event loop. Runs until `frame_rx` is closed (network task gone) or
/// the proxy returns `EventLoopClosed`.
fn run_decode_loop(
    decode_cfg: DecodeConfig,
    display_cfg: DisplayConfig,
    mut frame_rx: tokio::sync::mpsc::Receiver<ReassembledFrame>,
    proxy: EventLoopProxy<UserEvent>,
) -> Result<()> {
    let mut decoder = VideoDecoder::new(
        decode_cfg.prefer_hardware,
        display_cfg.width,
        display_cfg.height,
    )
    .context("initializing video decoder")?;
    tracing::info!(backend = decoder.backend(), "decoder ready");

    let proxy = Arc::new(proxy);
    let mut event_loop_alive = true;
    let mut decoded_frames: u64 = 0;
    let mut decode_errors: u64 = 0;

    while let Some(reassembled) = frame_rx.blocking_recv() {
        if !event_loop_alive {
            // No point decoding if there's no one to draw it.
            break;
        }
        let proxy_clone = proxy.clone();
        let mut alive = event_loop_alive;
        let res = decoder.push_nal(&reassembled.nal, reassembled.timestamp_us, |frame| {
            if alive && proxy_clone.send_event(UserEvent::Frame(frame)).is_err() {
                alive = false;
            }
        });
        event_loop_alive = alive;
        match res {
            Ok(()) => decoded_frames += 1,
            Err(e) => {
                decode_errors += 1;
                tracing::warn!(error = %e, "decode failed");
            }
        }
    }

    // Tell the event loop the worker is going away (no-op if already exited).
    let _ = proxy.send_event(UserEvent::WorkerExited);
    tracing::info!(decoded_frames, decode_errors, "decode loop exiting");
    Ok(())
}
