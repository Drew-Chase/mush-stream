//! Host streaming session.
//!
//! Drives the host pipeline in-process via the `mush_stream_host`
//! library's runner. The runner builds its own multi-thread tokio
//! runtime and owns the encode/audio/ViGEm threads + UDP transport;
//! we keep the shutdown atomic + the joined OS thread that is
//! blocking on it, so the user's "Stop streaming" button can flip
//! the atomic without touching any of the worker threads directly.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use mush_stream_host::capture::CaptureRect;
use mush_stream_host::runner;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::configs;
use crate::state::AppState;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HostState {
    Idle,
    Starting,
    Broadcasting,
    Stopping,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostStateEvent {
    pub state: HostState,
    pub error: Option<String>,
}

/// Push notification fired when the bound client peer changes during
/// a host session. `address` is `Some("ip:port")` once the host has
/// observed the first inbound packet (or the peer rotates to a new
/// ephemeral port), and `None` at session end so the frontend can
/// clear any "currently connected" UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostPeerEvent {
    pub address: Option<String>,
}

pub struct HostSession {
    /// Flips to true to ask the runner thread to wind down.
    shutdown: Arc<AtomicBool>,
    /// The OS thread blocking inside `runner::run_stream_blocking`.
    /// Kept around so `host_stop` can join it cleanly.
    thread: Option<JoinHandle<()>>,
}

impl HostSession {
    fn shutdown_and_join(mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(handle) = self.thread.take() {
            // Worker may take a couple hundred ms to drain encoder +
            // join its sub-threads; do this on a blocking task so the
            // calling tokio runtime doesn't stall.
            let _ = handle.join();
        }
    }
}

impl Drop for HostSession {
    fn drop(&mut self) {
        // Defensive: if a HostSession is dropped without an explicit
        // stop (e.g. the AppState is being torn down), still flip
        // shutdown so the worker thread doesn't outlive the process.
        self.shutdown.store(true, Ordering::Release);
    }
}

#[tauri::command]
pub async fn host_start(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    {
        let session = state
            .host
            .lock()
            .map_err(|e| format!("host lock poisoned: {e}"))?;
        if session.is_some() {
            return Err("host is already broadcasting".into());
        }
    }

    let cfg = configs::read_host_config(&state.paths.host_toml)
        .map_err(|e| format!("loading host config: {e}"))?;
    let rect = CaptureRect {
        x: cfg.capture.x,
        y: cfg.capture.y,
        width: cfg.capture.width,
        height: cfg.capture.height,
    };
    let output_index = cfg.capture.output_index;

    emit_state(&app, HostState::Starting, None);

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_for_thread = shutdown.clone();
    let app_for_thread = app.clone();

    // Build the peer-change observer the host runner will invoke from
    // its UDP recv loop. The closure must be `'static + Send + Sync`,
    // so we capture an `AppHandle` (cheaply cloneable) and resolve the
    // managed `AppState` per-call via the handle. That keeps the live
    // UI updated (via `host:peer` event) and the shared mirror fresh
    // for the `host_peer` pull command after a page reload.
    let app_for_peer = app.clone();
    let peer_observer: mush_stream_host::transport::PeerObserver =
        Arc::new(move |peer: Option<std::net::SocketAddr>| {
            let formatted = peer.map(|addr| addr.to_string());
            if let Some(state) = app_for_peer.try_state::<AppState>()
                && let Ok(mut guard) = state.host_peer.lock()
            {
                (*guard).clone_from(&formatted);
            }
            let _ = app_for_peer.emit(
                "host:peer",
                HostPeerEvent {
                    address: formatted,
                },
            );
        });

    let thread = std::thread::Builder::new()
        .name("mush-host-runner".into())
        .spawn(move || {
            tracing::info!(
                output_index,
                width = rect.width,
                height = rect.height,
                "host streaming session starting"
            );
            // `handle_ctrl_c = false`: the Tauri parent process owns
            // its own signal handling; we don't want the runner
            // installing a competing handler.
            let result = runner::run_stream_blocking(
                cfg,
                rect,
                shutdown_for_thread,
                false,
                Some(peer_observer),
            );
            match result {
                Ok(()) => {
                    tracing::info!("host streaming session stopped cleanly");
                    emit_state(&app_for_thread, HostState::Idle, None);
                }
                Err(e) => {
                    tracing::error!(error = %e, "host streaming session failed");
                    emit_state(
                        &app_for_thread,
                        HostState::Idle,
                        Some(e.to_string()),
                    );
                }
            }
        })
        .map_err(|e| format!("spawning host runner thread: {e}"))?;

    {
        let mut session = state
            .host
            .lock()
            .map_err(|e| format!("host lock poisoned: {e}"))?;
        *session = Some(HostSession {
            shutdown,
            thread: Some(thread),
        });
    }

    emit_state(&app, HostState::Broadcasting, None);
    Ok(())
}

#[tauri::command]
pub async fn host_stop(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let session = {
        let mut guard = state
            .host
            .lock()
            .map_err(|e| format!("host lock poisoned: {e}"))?;
        guard.take()
    };
    let Some(session) = session else {
        return Ok(());
    };
    emit_state(&app, HostState::Stopping, None);

    // Joining the runner thread blocks while it drains the encoder +
    // joins its own sub-threads. Push that to a blocking task so we
    // don't hang the tokio command worker.
    tauri::async_runtime::spawn_blocking(move || session.shutdown_and_join())
        .await
        .map_err(|e| format!("joining host runner: {e}"))?;

    emit_state(&app, HostState::Idle, None);
    Ok(())
}

#[tauri::command]
pub async fn host_status(state: State<'_, AppState>) -> Result<HostState, String> {
    let session = state
        .host
        .lock()
        .map_err(|e| format!("host lock poisoned: {e}"))?;
    Ok(if session.is_some() {
        HostState::Broadcasting
    } else {
        HostState::Idle
    })
}

/// Query the most recently observed peer for the active host session.
/// Returns `None` when no session is running, or when a session is
/// running but no client has sent a packet yet. Used by the frontend
/// on mount to recover the connected-client UI after a page reload.
#[tauri::command]
pub async fn host_peer(state: State<'_, AppState>) -> Result<Option<String>, String> {
    let guard = state
        .host_peer
        .lock()
        .map_err(|e| format!("host_peer lock poisoned: {e}"))?;
    Ok(guard.clone())
}

fn emit_state(app: &AppHandle, state: HostState, error: Option<String>) {
    let _ = app.emit("host:state", HostStateEvent { state, error });
}
