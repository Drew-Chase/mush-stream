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
mod transport;

use std::{
    ffi::OsString,
    path::PathBuf,
    sync::Arc,
};

use anyhow::{Context, Result};
use mush_stream_common::protocol::video::ReassembledFrame;
use tracing_subscriber::EnvFilter;
use winit::event_loop::EventLoopProxy;

use crate::config::{Config, DecodeConfig, DisplayConfig};
use crate::decode::VideoDecoder;
use crate::display::{DisplayApp, UserEvent};
use crate::transport::run_video_receiver;

const DEFAULT_CONFIG_PATH: &str = "./client.toml";

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config_path = parse_config_path(std::env::args_os());
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

    // Network thread (own a tokio runtime so async UDP works).
    let video_bind = cfg.network.video_bind;
    let net_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building network runtime")?;
    let net_thread = std::thread::Builder::new()
        .name("mush-net".into())
        .spawn(move || {
            net_runtime.block_on(async move {
                match run_video_receiver(video_bind, frame_tx).await {
                    Ok(stats) => tracing::info!(?stats, "video receiver stopped"),
                    Err(e) => tracing::error!(error = %e, "video receiver failed"),
                }
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
    let _ = decode_thread; // detach
    let _ = net_thread; // detach

    tracing::info!(
        frames_presented = app.stats.frames_presented,
        last_glass_to_glass_us = ?app.stats.last_glass_to_glass_us,
        "client exiting"
    );
    Ok(())
}

fn parse_config_path<I, S>(args: I) -> PathBuf
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    args.into_iter()
        .skip(1)
        .map(|a| PathBuf::from(a.into()))
        .next()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH))
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
