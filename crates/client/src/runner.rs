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

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use mush_stream_common::protocol::{
    audio::AudioPacket,
    control::{self, ControlMessage},
};
use tokio::sync::mpsc;
use winit::event_loop::EventLoopProxy;

use crate::audio;
use crate::config::{Config, DecodeConfig};
use crate::decode::VideoDecoder;
use crate::display::{self, DisplayApp, UserEvent};
use crate::input::{run_gamepad_loop, InputCommand};
use crate::transport::{
    connect_to_host, run_video_receiver, DeliveredFrame, ReceiverExit,
};

/// Delay between reconnect attempts when the host went silent or the
/// initial connect failed. Picked to match the receiver's silence
/// timeout: the user sees "Reconnecting…" for roughly 5 s after the
/// host went away, then a quick recovery once it's back.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Lifecycle event the runner emits to the (optional) caller-supplied
/// observer. The Tauri shell maps these onto `client:state` events for
/// the React UI; the binary entry point just logs.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// First connection attempt.
    Connecting,
    /// Socket connected and we're now in the receive/send loop.
    Connected,
    /// The previous connection failed (host went silent, network
    /// glitch, initial connect refused). We'll wait `RECONNECT_DELAY`
    /// then try again.
    Reconnecting,
    /// The runner is winding down — either the user closed the
    /// window, the Tauri shell asked to disconnect, or the decode
    /// pipeline closed its end of the frame channel.
    Disconnected,
}

/// Type alias for the lifecycle observer. `Arc<dyn Fn ...>` so the
/// Tauri shell can capture an `AppHandle` cheaply and emit events
/// without re-building the closure on each callback.
pub type SessionEventCallback = Arc<dyn Fn(SessionEvent) + Send + Sync>;

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
pub fn start_client_session(
    cfg: Config,
    on_event: Option<SessionEventCallback>,
) -> Result<ClientSessionHandle> {
    let (proxy_tx, proxy_rx) = std::sync::mpsc::sync_channel::<EventLoopProxy<UserEvent>>(1);
    let thread = std::thread::Builder::new()
        .name("mush-client-runner".into())
        .spawn(move || run_session(cfg, proxy_tx, on_event))
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
    on_event: Option<SessionEventCallback>,
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

    // Gamepad thread (250 Hz). Skipped entirely when `input.forward_pad`
    // is false — saves the gilrs init cost and makes "no input
    // forwarding" actually mean what it says rather than silently
    // emitting packets the host then accepts as a virtual pad.
    let gamepad_shutdown = Arc::new(AtomicBool::new(false));
    let gamepad_thread: Option<std::thread::JoinHandle<()>> =
        if cfg.input.forward_pad {
            let gamepad_shutdown_for_thread = gamepad_shutdown.clone();
            let selected_id = cfg.input.gamepad_id;
            Some(
                std::thread::Builder::new()
                    .name("mush-input".into())
                    .spawn(move || {
                        if let Err(e) = run_gamepad_loop(
                            input_tx,
                            gamepad_shutdown_for_thread,
                            selected_id,
                        ) {
                            tracing::error!(
                                error = %e,
                                "gamepad loop exited with error"
                            );
                        }
                    })
                    .context("spawning gamepad thread")?,
            )
        } else {
            tracing::info!(
                "input.forward_pad = false; gamepad polling disabled"
            );
            // Drop the input channel sender so the network task's
            // input sender exits cleanly when the runtime shuts down.
            drop(input_tx);
            None
        };

    // Network runtime. Owned here on the runner thread; tasks run on
    // its worker pool. The orchestrator below loops on connect →
    // serve-one-connection → wait-and-retry until `net_shutdown`
    // flips, so a host that goes silent doesn't tear down the rest of
    // the pipeline (display window stays open, decoder keeps waiting
    // on its channel).
    let host_addr = cfg.network.host;
    let net_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("building network runtime")?;

    let net_shutdown = Arc::new(AtomicBool::new(false));
    let net_shutdown_for_task = net_shutdown.clone();
    let audio_out = if audio_enabled { Some(audio_tx) } else { None };
    let net_handle = net_runtime.spawn(async move {
        run_network_with_reconnect(
            host_addr,
            input_rx,
            frame_tx,
            keyframe_tx,
            audio_out,
            net_shutdown_for_task,
            on_event,
        )
        .await;
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
    // 2. Tell the network orchestrator to wind down, then block on
    //    its task with a short timeout so it has a chance to send a
    //    `ControlMessage::Disconnect` to the host before we tear the
    //    runtime down. Bounded so a stuck task can't pin the UI.
    // 3. Drop the network runtime (aborts anything that overran the
    //    timeout); that closes frame_tx + audio_tx, so the decode +
    //    audio threads see channel-closed and exit.
    // 4. Flip the gamepad shutdown atomic.
    // 5. Join all threads so the runner doesn't return while workers
    //    are still touching shared state.
    drop(proxy);
    net_shutdown.store(true, Ordering::Release);
    let _ = net_runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), net_handle).await
    });
    drop(net_runtime);
    gamepad_shutdown.store(true, Ordering::Release);

    let _ = decode_thread.join();
    if let Some(h) = gamepad_thread {
        let _ = h.join();
    }
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

/// What ended a single connection attempt — feeds the orchestrator's
/// "retry vs bail out" decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionExit {
    /// Caller asked us to stop (`net_shutdown` flipped). The
    /// orchestrator emits `Disconnected` and exits.
    Shutdown,
    /// Receiver returned `HostSilent` — no packets in 5 s. Wait
    /// `RECONNECT_DELAY` and try a fresh connect.
    HostSilent,
    /// Receiver's downstream channel closed (decoder gone). Treated
    /// like a shutdown: the rest of the client is winding down, so
    /// reconnecting would be pointless.
    DownstreamClosed,
}

/// Long-lived network orchestrator. Owns a single `input_rx` (so the
/// gamepad thread keeps producing into the same channel across
/// reconnects), and re-creates the UDP socket on each attempt.
///
/// Surfacing `Connecting` / `Reconnecting` / `Connected` /
/// `Disconnected` to the optional callback gives the Tauri shell a
/// per-state push without it needing to peek at the runner thread's
/// state. The display window stays open across reconnects; the
/// decoder simply blocks on its empty frame channel.
async fn run_network_with_reconnect(
    host_addr: SocketAddr,
    mut input_rx: mpsc::Receiver<InputCommand>,
    frame_tx: mpsc::Sender<DeliveredFrame>,
    keyframe_tx: mpsc::Sender<InputCommand>,
    audio_out: Option<mpsc::Sender<AudioPacket>>,
    shutdown: Arc<AtomicBool>,
    on_event: Option<SessionEventCallback>,
) {
    let emit = |ev: SessionEvent| {
        if let Some(cb) = on_event.as_ref() {
            cb(ev);
        }
    };

    let mut first_attempt = true;
    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        emit(if first_attempt {
            SessionEvent::Connecting
        } else {
            SessionEvent::Reconnecting
        });
        first_attempt = false;

        let socket = match connect_to_host(host_addr).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    delay_secs = RECONNECT_DELAY.as_secs(),
                    "client socket connect failed; retrying"
                );
                if wait_or_shutdown(&shutdown, RECONNECT_DELAY).await {
                    break;
                }
                continue;
            }
        };

        emit(SessionEvent::Connected);

        let outcome = run_one_connection(
            socket,
            &mut input_rx,
            frame_tx.clone(),
            keyframe_tx.clone(),
            audio_out.clone(),
            shutdown.clone(),
        )
        .await;

        match outcome {
            ConnectionExit::Shutdown | ConnectionExit::DownstreamClosed => break,
            ConnectionExit::HostSilent => {
                tracing::info!(
                    delay_secs = RECONNECT_DELAY.as_secs(),
                    "host went silent; will retry connection"
                );
                if wait_or_shutdown(&shutdown, RECONNECT_DELAY).await {
                    break;
                }
            }
        }
    }

    emit(SessionEvent::Disconnected);
}

/// Sleep for `delay`, but break early if `shutdown` flips. Returns
/// true when shutdown was observed (caller should exit its retry
/// loop), false when the full delay elapsed.
async fn wait_or_shutdown(shutdown: &Arc<AtomicBool>, delay: Duration) -> bool {
    // Poll the atomic at 100 ms granularity. Cheap, and an idle
    // reconnecting client doesn't need millisecond-accurate timing.
    let start = std::time::Instant::now();
    while start.elapsed() < delay {
        if shutdown.load(Ordering::Acquire) {
            return true;
        }
        let remaining = delay.saturating_sub(start.elapsed());
        let chunk = remaining.min(Duration::from_millis(100));
        tokio::time::sleep(chunk).await;
    }
    shutdown.load(Ordering::Acquire)
}

/// Drive one UDP session: video receiver task plus inline input
/// forwarder and shutdown poll, all racing in a single `select!`.
/// Returns when any of them finishes. On `Shutdown` we send a
/// best-effort `ControlMessage::Disconnect` to the host so its
/// connected-client UI clears immediately rather than waiting for the
/// receive loop's own silence timeout.
async fn run_one_connection(
    socket: Arc<tokio::net::UdpSocket>,
    input_rx: &mut mpsc::Receiver<InputCommand>,
    frame_tx: mpsc::Sender<DeliveredFrame>,
    keyframe_tx: mpsc::Sender<InputCommand>,
    audio_out: Option<mpsc::Sender<AudioPacket>>,
    shutdown: Arc<AtomicBool>,
) -> ConnectionExit {
    let recv_socket = socket.clone();
    let mut recv_handle = tokio::spawn(async move {
        run_video_receiver(recv_socket, frame_tx, Some(keyframe_tx), audio_out)
            .await
    });

    // Buffer reused for every outbound packet on this socket.
    // InputPacket and ControlMessage have different sizes; we serialize
    // into the right-sized prefix per call.
    let mut input_buf = [0u8; mush_stream_common::protocol::input::SIZE];
    let mut ctrl_buf = [0u8; control::SIZE];

    let mut shutdown_tick =
        tokio::time::interval(Duration::from_millis(100));
    // First tick fires immediately; eat it so we don't no-op the first
    // loop iteration.
    shutdown_tick.tick().await;

    let exit = loop {
        tokio::select! {
            // Drain input commands → socket. Returns None when the
            // gamepad thread + keyframe sender both drop their
            // halves, which the runner only does on shutdown.
            cmd = input_rx.recv() => {
                let Some(cmd) = cmd else {
                    break ConnectionExit::Shutdown;
                };
                match cmd {
                    InputCommand::Input(p) => {
                        p.write_to(&mut input_buf);
                        if let Err(e) = socket.send(&input_buf).await {
                            tracing::warn!(error = %e, "input send failed");
                        }
                    }
                    InputCommand::Control(c) => {
                        c.write_to(&mut ctrl_buf);
                        if let Err(e) = socket.send(&ctrl_buf).await {
                            tracing::warn!(error = %e, "control send failed");
                        }
                    }
                }
            }

            // Receiver finished. Either the host went silent (retry)
            // or the decoder closed its end (terminal shutdown).
            res = &mut recv_handle => {
                break match res {
                    Ok(Ok((ReceiverExit::HostSilent, stats))) => {
                        tracing::info!(?stats, "video receiver stopped: host silent");
                        ConnectionExit::HostSilent
                    }
                    Ok(Ok((ReceiverExit::DownstreamClosed, stats))) => {
                        tracing::info!(?stats, "video receiver stopped: decoder gone");
                        ConnectionExit::DownstreamClosed
                    }
                    Ok(Err(e)) => {
                        tracing::error!(error = %e, "video receiver failed");
                        ConnectionExit::HostSilent
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "video receiver task panicked");
                        ConnectionExit::HostSilent
                    }
                };
            }

            // Cooperative shutdown poll. Periodic rather than a
            // dedicated channel since the runner already has the
            // atomic and the latency cost is negligible.
            _ = shutdown_tick.tick() => {
                if shutdown.load(Ordering::Acquire) {
                    break ConnectionExit::Shutdown;
                }
            }
        }
    };

    // Best-effort disconnect notification on graceful shutdown only.
    // For HostSilent / DownstreamClosed there's no live host to tell,
    // and the send would just block until the kernel buffer routes it
    // to /dev/null.
    if matches!(exit, ConnectionExit::Shutdown) {
        ControlMessage::Disconnect.write_to(&mut ctrl_buf);
        let send_fut = socket.send(&ctrl_buf);
        match tokio::time::timeout(Duration::from_millis(200), send_fut).await {
            Ok(Ok(_)) => {
                tracing::info!("sent ControlMessage::Disconnect to host");
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "Disconnect send failed");
            }
            Err(_) => {
                tracing::warn!("Disconnect send timed out (200ms)");
            }
        }
    }

    if !recv_handle.is_finished() {
        recv_handle.abort();
    }

    exit
}
