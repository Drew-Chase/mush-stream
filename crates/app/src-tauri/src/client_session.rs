//! Client connection session.
//!
//! Spawns the existing `mush-stream-client` binary against a temp
//! `client.toml` seeded from the UI's address. The native client
//! window appears as its own OS-level window; the Tauri Connect page
//! shows status + tails the child's stdout for log lines.

use std::process::Stdio;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::bin_locator;
use crate::configs::{self, ClientConfig};
use crate::logs::{ingest_external, LogSink};
use crate::state::AppState;

const BIN_NAME: &str = "mush-stream-client";
const SESSION_CFG_NAME: &str = "client.session.toml";

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)] // Idle is the implicit default emitted from the frontend
pub enum ClientState {
    Idle,
    Connecting,
    Connected,
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
    #[serde(default = "default_true")]
    pub audio: bool,
}

fn default_true() -> bool {
    true
}

pub struct ClientSession {
    shutdown_tx: mpsc::Sender<()>,
    pub address: String,
}

#[tauri::command]
#[allow(clippy::too_many_lines)] // single coherent spawn + tail orchestration
pub async fn client_connect(
    options: ConnectOptions,
    app: AppHandle,
    state: State<'_, AppState>,
    log_sink: State<'_, Arc<LogSink>>,
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

    let bin = bin_locator::locate(BIN_NAME)
        .ok_or_else(|| format!("could not locate {BIN_NAME}; build the workspace first"))?;

    // Build a per-session client.toml that overrides the saved
    // network.host with whatever the user typed in. Saved
    // settings (hwdec, audio) are layered on top.
    let mut cfg = configs::read_client_config(&state.paths.client_toml)
        .map_err(|e| format!("loading saved client config: {e}"))?;
    apply_options(&mut cfg, &options)?;

    let session_path = state.paths.config_dir.join(SESSION_CFG_NAME);
    configs::write_client_config(&session_path, &cfg)
        .map_err(|e| format!("writing session config: {e}"))?;

    emit_state(
        &app,
        ClientState::Connecting,
        Some(options.address.clone()),
        None,
    );

    let mut child = Command::new(&bin)
        .arg(&session_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawning {}: {e}", bin.display()))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

    if let Some(stream) = stdout {
        let sink = (*log_sink).clone();
        let app = app.clone();
        let address = options.address.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stream).lines();
            let mut connected_emitted = false;
            while let Ok(Some(line)) = reader.next_line().await {
                ingest_external(&sink, "INFO", "client.stdout", line.clone());
                // Heuristic state transitions: the client logs
                // "decoder ready" once the decoder backend is open.
                if !connected_emitted
                    && (line.contains("decoder ready")
                        || line.contains("first packet"))
                {
                    connected_emitted = true;
                    let _ = app.emit(
                        "client:state",
                        ClientStateEvent {
                            state: ClientState::Connected,
                            address: Some(address.clone()),
                            error: None,
                        },
                    );
                }
            }
        });
    }
    if let Some(stream) = stderr {
        let sink = (*log_sink).clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stream).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                ingest_external(&sink, "WARN", "client.stderr", line);
            }
        });
    }

    {
        let app = app.clone();
        let address = options.address.clone();
        tokio::spawn(async move {
            tokio::select! {
                exit = child.wait() => {
                    match exit {
                        Ok(status) if status.success() => {
                            emit_state(&app, ClientState::Disconnected, Some(address), None);
                        }
                        Ok(status) => {
                            emit_state(
                                &app,
                                ClientState::Disconnected,
                                Some(address),
                                Some(format!("client exited with {status}")),
                            );
                        }
                        Err(e) => {
                            emit_state(
                                &app,
                                ClientState::Error,
                                Some(address),
                                Some(format!("waiting on client: {e}")),
                            );
                        }
                    }
                }
                _ = shutdown_rx.recv() => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    emit_state(&app, ClientState::Disconnected, Some(address), None);
                }
            }
        });
    }

    {
        let mut session = state
            .client
            .lock()
            .map_err(|e| format!("client lock poisoned: {e}"))?;
        *session = Some(ClientSession {
            shutdown_tx,
            address: options.address.clone(),
        });
    }
    Ok(())
}

#[tauri::command]
pub async fn client_disconnect(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let (shutdown_tx, address) = {
        let mut session = state
            .client
            .lock()
            .map_err(|e| format!("client lock poisoned: {e}"))?;
        match session.take() {
            Some(s) => (s.shutdown_tx, s.address),
            None => return Ok(()),
        }
    };
    let _ = shutdown_tx.try_send(());
    emit_state(&app, ClientState::Disconnected, Some(address), None);
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
    // forward_pad isn't a runtime config flag for the existing client
    // binary — it's compiled-in. Left here as a UI passthrough for
    // future plumbing.
    let _ = opts.forward_pad;
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
