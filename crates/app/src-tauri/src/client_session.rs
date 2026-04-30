//! Client connection session.
//!
//! Drives the client pipeline in-process via the
//! `mush_stream_client::runner` library. The runner spawns its own
//! `mush-client-runner` OS thread that owns the winit event loop +
//! the network/audio/decode/gamepad workers, and hands us back a
//! proxy we use to ask it to wind down. The native viewer window
//! still appears (it's a real winit/wgpu surface), but it lives in
//! the same process as the Tauri webview — so the app ships as one
//! executable.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

use mush_stream_client::runner::{
    self, ClientSessionHandle, SessionEvent, SessionEventCallback, ShutdownHandle,
};

use crate::configs::{self, ClientConfig};
use crate::state::AppState;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)] // Idle is the implicit default emitted from the frontend
pub enum ClientState {
    Idle,
    Connecting,
    Connected,
    /// We were connected, the host went silent (or the initial
    /// connect failed), and the runner is sleeping before its next
    /// retry. The frontend renders a "Reconnecting…" banner; the
    /// display window stays open so the user doesn't lose their
    /// place.
    Reconnecting,
    Disconnected,
    Error,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientStateEvent {
    pub state: ClientState,
    pub address: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectOptions {
    pub address: String,
    #[serde(default = "default_true")]
    pub hardware_decode: bool,
    #[serde(default = "default_true")]
    pub forward_pad: bool,
    /// Specific gilrs gamepad id to forward, or `None` to take the
    /// first one gilrs reports. Surfaced from the Connect page's
    /// gamepad dropdown.
    #[serde(default)]
    pub gamepad_id: Option<u32>,
    #[serde(default = "default_true")]
    pub audio: bool,
}

fn default_true() -> bool {
    true
}

pub struct ClientSession {
    /// Cheap shutdown token, used by `client_disconnect` to ask the
    /// runner thread to exit. The owning `ClientSessionHandle` has
    /// been moved into a watchdog task that joins the thread and
    /// emits `disconnected` when the runner returns (whether by
    /// user-clicking-X or our shutdown signal).
    shutdown: ShutdownHandle,
    pub address: String,
}

#[tauri::command]
pub async fn client_connect(
    options: ConnectOptions,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    {
        let session = state
            .client
            .lock()
            .map_err(|e| format!("client lock poisoned: {e}"))?;
        if session.is_some() {
            return Err("client is already connected".into());
        }
    }

    // Layer the per-session UI inputs on top of the saved client config.
    let mut cfg = configs::read_client_config(&state.paths.client_toml)
        .map_err(|e| format!("loading saved client config: {e}"))?;
    apply_options(&mut cfg, &options)?;

    emit_state(
        &app,
        ClientState::Connecting,
        Some(options.address.clone()),
        None,
    );

    // Build the lifecycle observer the runner pings on
    // Connecting/Connected/Reconnecting/Disconnected. We forward each
    // mapped state to the frontend via the existing `client:state`
    // event so the React UI can render a "reconnecting…" banner
    // without inventing a parallel channel.
    let app_for_events = app.clone();
    let address_for_events = options.address.clone();
    let session_callback: SessionEventCallback =
        Arc::new(move |ev: SessionEvent| {
            // Skip the runner's terminal `Disconnected` — the
            // watchdog below emits the authoritative final state
            // (Disconnected vs Error) once the runner thread has
            // actually joined, and we don't want to race that.
            let mapped = match ev {
                SessionEvent::Connecting => ClientState::Connecting,
                SessionEvent::Connected => ClientState::Connected,
                SessionEvent::Reconnecting => ClientState::Reconnecting,
                SessionEvent::Disconnected => return,
            };
            let _ = app_for_events.emit(
                "client:state",
                ClientStateEvent {
                    state: mapped,
                    address: Some(address_for_events.clone()),
                    error: None,
                },
            );
        });

    let handle: ClientSessionHandle =
        runner::start_client_session(cfg, Some(session_callback)).map_err(|e| {
            let msg = format!("starting client session: {e}");
            emit_state(&app, ClientState::Error, Some(options.address.clone()), Some(msg.clone()));
            msg
        })?;

    let shutdown = handle.shutdown_handle();

    {
        let mut session = state
            .client
            .lock()
            .map_err(|e| format!("client lock poisoned: {e}"))?;
        *session = Some(ClientSession {
            shutdown,
            address: options.address.clone(),
        });
    }

    // No optimistic "Connected" emit here: the runner pings the
    // session callback with `SessionEvent::Connected` once its UDP
    // socket actually comes up, and the watchdog below emits the
    // authoritative final state when the thread returns.

    // Watchdog: when the runner thread finishes (window closed by
    // user OR shutdown signaled by us), emit the appropriate state
    // and clear the session record so a future connect can succeed.
    let app_for_watchdog = app.clone();
    let address_for_watchdog = options.address.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let result = handle.join();
        // Clear the session record. We hold our own snapshot of the
        // address for the emitted event so this doesn't race against
        // a fresh connect that re-populated the slot.
        let state = app_for_watchdog.state::<AppState>();
        if let Ok(mut guard) = state.client.lock() {
            // Only clear if the address still matches — defensive in
            // case a reconnect raced with the watchdog wakeup.
            if guard
                .as_ref()
                .is_some_and(|s| s.address == address_for_watchdog)
            {
                *guard = None;
            }
        }
        match result {
            Ok(()) => {
                let _ = app_for_watchdog.emit(
                    "client:state",
                    ClientStateEvent {
                        state: ClientState::Disconnected,
                        address: Some(address_for_watchdog),
                        error: None,
                    },
                );
            }
            Err(e) => {
                let _ = app_for_watchdog.emit(
                    "client:state",
                    ClientStateEvent {
                        state: ClientState::Error,
                        address: Some(address_for_watchdog),
                        error: Some(e.to_string()),
                    },
                );
            }
        }
    });

    Ok(())
}

#[tauri::command]
pub async fn client_disconnect(state: State<'_, AppState>) -> Result<(), String> {
    let shutdown = {
        let guard = state
            .client
            .lock()
            .map_err(|e| format!("client lock poisoned: {e}"))?;
        guard.as_ref().map(|s| s.shutdown.clone())
    };
    if let Some(shutdown) = shutdown {
        shutdown.shutdown();
    }
    // Don't clear the session record here — the watchdog does it
    // when the runner thread actually returns.
    Ok(())
}

#[tauri::command]
pub async fn client_status(
    state: State<'_, AppState>,
) -> Result<Option<String>, String> {
    let session = state
        .client
        .lock()
        .map_err(|e| format!("client lock poisoned: {e}"))?;
    Ok(session.as_ref().map(|s| s.address.clone()))
}

fn apply_options(cfg: &mut ClientConfig, opts: &ConnectOptions) -> Result<(), String> {
    cfg.network.host = opts
        .address
        .parse()
        .map_err(|e| format!("invalid address `{}`: {e}", opts.address))?;
    cfg.decode.prefer_hardware = opts.hardware_decode;
    cfg.audio.enabled = opts.audio;
    cfg.input.forward_pad = opts.forward_pad;
    cfg.input.gamepad_id = opts.gamepad_id;
    Ok(())
}

fn emit_state(
    app: &AppHandle,
    state: ClientState,
    address: Option<String>,
    error: Option<String>,
) {
    let _ = app.emit(
        "client:state",
        ClientStateEvent {
            state,
            address,
            error,
        },
    );
}
