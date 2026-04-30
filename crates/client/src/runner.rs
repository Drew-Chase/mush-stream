//! In-process client session orchestration.
//!
//! Wraps connect + receive + decode + audio playback + gamepad input +
//! winit/pixels presentation into a single `start_client_session`
//! entry point, so callers (the client binary's `main.rs` and the
//! Tauri desktop shell) can drive the same pipeline without copy-
//! pasting the threading layout.
//!
//! Threading layout:
//! - `start_client_session` spawns one OS thread named
//!   `mush-client-runner`. That thread owns the winit event loop,
//!   the network tokio runtime, and the supervisor logic. The caller
//!   gets back a `ClientSessionHandle` it can use to ask the loop to
//!   exit and to join the thread.
//! - Inside the runner thread the layout mirrors `crates/client/src/main.rs`:
//!   one `mush-decode` thread, one `mush-audio` thread (optional), one
//!   `mush-input` thread, and the network tasks running on a dedicated
//!   tokio runtime. The runner thread itself drives `event_loop.run_app`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::{anyhow, Context, Result};
use mush_stream_common::protocol::audio::AudioPacket;
use tokio::sync::mpsc;
use winit::event_loop::EventLoopProxy;

use crate::audio;
use crate::config::{Config, DecodeConfig};
use crate::decode::VideoDecoder;
use crate::display::{self, DisplayApp, UserEvent};
use crate::input::{run_gamepad_loop, InputCommand};
use crate::transport::{
    connect_to_host, run_input_sender, run_video_receiver, DeliveredFrame,
};

/// Cloneable shutdown token. Hands out the ability to request the
/// runner thread to exit without exposing winit internals — callers
/// (e.g. the Tauri shell) don't need to depend on `winit` directly.
#[derive(Clone)]
pub struct ShutdownHandle {
    proxy: EventLoopProxy<UserEvent>,
}

impl ShutdownHandle {
    /// Ask the runner to exit. Idempotent — multiple sends are fine.
    pub fn shutdown(&self) {
        let _ = self.proxy.send_event(UserEvent::WorkerExited);
    }
}

/// Handle for an active client session. Hold this for the duration of
/// the connection; call [`Self::shutdown`] to ask the runner thread
/// to wind down, then [`Self::join`] to wait for it to finish.
pub struct ClientSessionHandle {
    proxy: EventLoopProxy<UserEvent>,
    thread: JoinHandle<Result<()>>,
}

impl ClientSessionHandle {
    /// Cheap, cloneable shutdown token. Useful when the caller wants
    /// to keep a "stop" capability independent from the join handle
    /// (e.g. Tauri stores it in app state and joins the thread from
    /// a watchdog task).
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle {
            proxy: self.proxy.clone(),
        }
    }

    /// Ask the runner to exit. Idempotent — multiple sends are fine.
    pub fn shutdown(&self) {
        let _ = self.proxy.send_event(UserEvent::WorkerExited);
    }

    /// Block on the runner thread; consumes the handle.
    pub fn join(self) -> Result<()> {
        match self.thread.join() {
            Ok(result) => result,
            Err(_) => Err(anyhow!("client runner thread panicked")),
        }
    }
}

/// Spawn the client session and return a handle. Returns once the
/// runner thread has built the event loop and surrendered its proxy
/// — the caller can immediately call `shutdown()` on the handle even
/// if the thread is still wiring up workers.
pub fn start_client_session(cfg: Config) -> Result<ClientSessionHandle> {
    let (proxy_tx, proxy_rx) = std::sync::mpsc::sync_channel::<EventLoopProxy<UserEvent>>(1);
    let thread = std::thread::Builder::new()
        .name("mush-client-runner".into())
        .spawn(move || run_session(cfg, proxy_tx))
        .context("spawning client runner thread")?;

    let proxy = proxy_rx
        .recv()
        .map_err(|_| anyhow!("client runner thread exited before sending proxy"))?;
    Ok(ClientSessionHandle { proxy, thread })
}

/// The body of the runner thread: build the event loop, fan out
/// workers, run the loop, then tear everything down.
#[allow(clippy::too_many_lines)] // single coherent setup mirrors main.rs
fn run_session(
    cfg: Config,
    proxy_tx: std::sync::mpsc::SyncSender<EventLoopProxy<UserEvent>>,
) -> Result<()> {
    let (event_loop, proxy) =
        display::build_event_loop().context("building event loop on runner thread")?;

    // Hand the proxy back to the caller before we start spawning
    // workers. If this fails the caller is gone — abort.
    if proxy_tx.send(proxy.clone()).is_err() {
        anyhow::bail!("client runner caller dropped its receiver before session started");
    }
    drop(proxy_tx);

    // 4 frames ≈ 67 ms slack at 60 fps; matches main.rs.
    let (frame_tx, frame_rx) = mpsc::channel::<DeliveredFrame>(4);
    let (input_tx, input_rx) = mpsc::channel::<InputCommand>(64);
    let keyframe_tx = input_tx.clone();

    let audio_enabled = cfg.audio.enabled;
    let (audio_tx, audio_rx) = mpsc::channel::<AudioPacket>(32);

    // Audio thread (optional).
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

    // Decode thread.
    let proxy_for_decode = proxy.clone();
    let decode_cfg = cfg.decode.clone();
    let decode_thread = std::thread::Builder::new()
        .name("mush-decode".into())
        .spawn(move || {
            if let Err(e) = run_decode_loop(decode_cfg, frame_rx, proxy_for_decode) {
                tracing::error!(error = %e, "decode loop exited with error");
            }
        })
        .context("spawning decode thread")?;

    // Gamepad thread (250 Hz). Always spawn; logs + exits cleanly if
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

    // Network runtime. Owned here on the runner thread; tasks run on
    // its worker pool. Dropping the runtime on shutdown aborts the
    // tasks and lets the consumer threads exit naturally.
    let host_addr = cfg.network.host;
    let net_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("building network runtime")?;

    net_runtime.spawn(async move {
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

    // Run the event loop on this thread until the user closes the
    // window (or the caller signals shutdown via the proxy).
    let mut app = DisplayApp::new(cfg.display);
    if let Err(e) = event_loop.run_app(&mut app) {
        tracing::error!(error = %e, "event loop exited with error");
    }

    // Cleanup. Order matters:
    // 1. Drop our proxy so the decoder's send_event(WorkerExited)
    //    doesn't keep the proxy reference count alive.
    // 2. Drop the network runtime to abort outstanding net tasks;
    //    that closes frame_tx + audio_tx, so the decode + audio
    //    threads see channel-closed and exit.
    // 3. Flip the gamepad shutdown atomic.
    // 4. Join all threads so the runner doesn't return while
    //    workers are still touching shared state.
    drop(proxy);
    drop(net_runtime);
    gamepad_shutdown.store(true, Ordering::Release);

    let _ = decode_thread.join();
    let _ = gamepad_thread.join();
    if let Some(h) = audio_handle {
        let _ = h.join();
    }

    tracing::info!(
        frames_presented = app.stats.frames_presented,
        cumulative_avg_us = ?app.stats.cumulative_avg_us(),
        cumulative_samples = app.stats.cumulative_samples,
        "client session exiting"
    );
    Ok(())
}

/// Decodes reassembled NAL frames and forwards decoded RGBA frames to
/// the winit event loop. Runs until `frame_rx` is closed (network
/// task gone) or the proxy returns `EventLoopClosed`.
#[allow(clippy::needless_pass_by_value)] // long-running thread entry; owns its inputs
fn run_decode_loop(
    decode_cfg: DecodeConfig,
    mut frame_rx: tokio::sync::mpsc::Receiver<DeliveredFrame>,
    proxy: EventLoopProxy<UserEvent>,
) -> Result<()> {
    let mut decoder = VideoDecoder::new(decode_cfg.prefer_hardware)
        .context("initializing video decoder")?;
    let backend = decoder.backend();
    tracing::info!(backend, "decoder ready");

    let proxy = Arc::new(proxy);
    let _ = proxy.send_event(UserEvent::DecoderReady { backend });
    let mut event_loop_alive = true;
    let mut decoded_frames: u64 = 0;
    let mut decode_errors: u64 = 0;
    let mut fast_forward_events: u64 = 0;
    let mut fast_forward_frames: u64 = 0;

    while let Some(first) = frame_rx.blocking_recv() {
        if !event_loop_alive {
            break;
        }

        let mut latest = first;
        let mut skipped_bytes: usize = 0;
        let mut skipped = 0u32;
        while let Ok(extra) = frame_rx.try_recv() {
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
