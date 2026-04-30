//! `client` — receives video over UDP, decodes via ffmpeg
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

use std::{path::PathBuf, sync::atomic::AtomicBool, sync::Arc};

use anyhow::{Context, Result};
use clap::Parser;
use mush_stream_client::audio;
use mush_stream_client::config::{Config, DecodeConfig, DisplayConfig};
use mush_stream_client::decode::VideoDecoder;
use mush_stream_client::display::{self, DisplayApp, UserEvent};
use mush_stream_client::input::{run_gamepad_loop, InputCommand};
use mush_stream_client::transport::{
    connect_to_host, run_input_sender, run_video_receiver, DeliveredFrame,
};
use tracing_subscriber::EnvFilter;
use winit::event_loop::EventLoopProxy;

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

#[allow(clippy::too_many_lines)] // main wires several subsystems together
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
    // 4 frames ≈ 67 ms slack at 60 fps. Just enough to absorb a tokio
    // scheduling hiccup; tighter than this and even a single missed
    // wake-up triggers a stall, looser and the worst-case queueing
    // latency starts pulling p95/p99 lag up. The decoder's
    // fast-forward path (decode_without_present on backlog) means we
    // don't need this to be larger to hide load — it's purely a
    // scheduling cushion. The 8 MiB kernel UDP recv buffer absorbs
    // network bursts; this channel must NOT double-buffer them.
    let (frame_tx, frame_rx) = tokio::sync::mpsc::channel::<DeliveredFrame>(4);

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

    // Audio path: receiver → audio decode/playback thread.
    let audio_enabled = cfg.audio.enabled;
    let (audio_tx, audio_rx) = tokio::sync::mpsc::channel::<
        mush_stream_common::protocol::audio::AudioPacket,
    >(32);
    let audio_handle = if audio_enabled {
        Some(
            std::thread::Builder::new()
                .name("mush-audio".into())
                .spawn(move || {
                    if let Err(e) = audio::run_audio_loop(audio_rx) {
                        tracing::error!(error = %e, "audio loop exited with error");
                    }
                })
                .context("spawning audio thread")?,
        )
    } else {
        tracing::info!("audio disabled in config; not playing");
        None
    };

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
    let host_addr = cfg.network.host;
    let net_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("building network runtime")?;
    let net_thread = std::thread::Builder::new()
        .name("mush-net".into())
        .spawn(move || {
            net_runtime.block_on(async move {
                let socket = match connect_to_host(host_addr).await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(error = %e, "client socket connect failed");
                        return;
                    }
                };
                let send_socket = socket.clone();
                let audio_out = if audio_enabled { Some(audio_tx) } else { None };
                let receive = tokio::spawn(async move {
                    match run_video_receiver(socket, frame_tx, Some(keyframe_tx), audio_out).await {
                        Ok(stats) => tracing::info!(?stats, "video receiver stopped"),
                        Err(e) => tracing::error!(error = %e, "video receiver failed"),
                    }
                });
                let send = tokio::spawn(async move {
                    run_input_sender(send_socket, input_rx).await;
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
    let _ = audio_handle; // detach

    tracing::info!(
        frames_presented = app.stats.frames_presented,
        last_lag_us = ?app.stats.last_lag_us,
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
#[allow(clippy::needless_pass_by_value)] // long-running thread entry; owns its inputs
fn run_decode_loop(
    decode_cfg: DecodeConfig,
    display_cfg: DisplayConfig,
    mut frame_rx: tokio::sync::mpsc::Receiver<DeliveredFrame>,
    proxy: EventLoopProxy<UserEvent>,
) -> Result<()> {
    let mut decoder = VideoDecoder::new(
        decode_cfg.prefer_hardware,
        display_cfg.width,
        display_cfg.height,
    )
    .context("initializing video decoder")?;
    let backend = decoder.backend();
    tracing::info!(backend, "decoder ready");

    let proxy = Arc::new(proxy);
    // Tell the display thread which backend was selected so the debug
    // overlay (Ctrl+Alt+D) can show it.
    let _ = proxy.send_event(UserEvent::DecoderReady { backend });
    let mut event_loop_alive = true;
    let mut decoded_frames: u64 = 0;
    let mut decode_errors: u64 = 0;
    let mut fast_forward_events: u64 = 0;
    let mut fast_forward_frames: u64 = 0;

    while let Some(first) = frame_rx.blocking_recv() {
        if !event_loop_alive {
            // No point decoding if there's no one to draw it.
            break;
        }

        // Drain anything else already pending. If the decoder/render side
        // briefly stalled (GPU contention, OS scheduling, wgpu present
        // hiccup) the channel can have multiple frames queued — we'd
        // otherwise present them at vsync rate, which reads as slow-mo
        // for the time it takes to drain. Instead: advance the reference
        // chain by decode-without-present for everything except the
        // newest frame, then full decode + scale + present that one.
        let mut latest = first;
        let mut skipped_bytes: usize = 0;
        let mut skipped = 0u32;
        while let Ok(extra) = frame_rx.try_recv() {
            // Advance reference chain on the previous `latest`. Cheap:
            // hardware decode without the YUV→RGBA scale_to_rgba step.
            match decoder.decode_without_present(&latest.reassembled.nal) {
                Ok(bytes) => skipped_bytes += bytes,
                Err(e) => tracing::warn!(error = %e, "decode-without-present failed"),
            }
            latest = extra;
            skipped += 1;
        }
        if skipped > 0 {
            fast_forward_events += 1;
            fast_forward_frames += u64::from(skipped);
            tracing::debug!(skipped, "fast-forwarded backlog");
        }

        let DeliveredFrame {
            reassembled,
            first_packet_instant,
        } = latest;
        let proxy_clone = proxy.clone();
        let mut alive = event_loop_alive;
        let res = decoder.push_nal(&reassembled.nal, first_packet_instant, |mut frame| {
            // Lump the skipped frames' bytes into the presented frame so
            // the overlay's bitrate readout stays accurate.
            frame.encoded_bytes = frame.encoded_bytes.saturating_add(skipped_bytes);
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
    tracing::info!(
        decoded_frames,
        decode_errors,
        fast_forward_events,
        fast_forward_frames,
        "decode loop exiting"
    );
    Ok(())
}
