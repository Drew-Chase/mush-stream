//! Host streaming session.
//!
//! Spawns the existing `mush-stream-host` binary in stream mode and
//! tails its stdout/stderr, forwarding lines into the in-process log
//! sink and emitting `host:state` / `host:telemetry` events. We do not
//! re-implement the capture+encode pipeline — the binary already does
//! it correctly; we just orchestrate it from the UI.

use std::process::Stdio;
use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::bin_locator;
use crate::configs;
use crate::logs::{ingest_external, LogSink};
use crate::state::AppState;

const BIN_NAME: &str = "mush-stream-host";

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

pub struct HostSession {
    /// Sends a unit value to request the child be killed.
    shutdown_tx: mpsc::Sender<()>,
}

#[tauri::command]
pub async fn host_start(
    app: AppHandle,
    state: State<'_, AppState>,
    log_sink: State<'_, Arc<LogSink>>,
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

    let bin = bin_locator::locate(BIN_NAME)
        .ok_or_else(|| format!("could not locate {BIN_NAME}; build the workspace first"))?;
    let cfg_path = state.paths.host_toml.clone();

    // Make sure the file exists so the spawned host has something to
    // load. read_host_config seeds defaults on first run.
    configs::read_host_config(&cfg_path)
        .map_err(|e| format!("preparing host config: {e}"))?;

    emit_state(&app, HostState::Starting, None);

    let mut child = Command::new(&bin)
        .arg("--stream")
        .arg(&cfg_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawning {}: {e}", bin.display()))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

    {
        let app = app.clone();
        let sink = (*log_sink).clone();
        if let Some(stream) = stdout {
            tokio::spawn(forward_lines(stream, sink, "host.stdout".to_string(), "INFO".to_string(), Some(app), Some("host:state-from-stdout")));
        }
    }
    {
        let sink = (*log_sink).clone();
        if let Some(stream) = stderr {
            tokio::spawn(forward_lines(
                stream,
                sink,
                "host.stderr".to_string(),
                "WARN".to_string(),
                None,
                None,
            ));
        }
    }

    // Driver task: wait for either the child to exit or a shutdown
    // request. Emits the appropriate state event when it returns.
    {
        let app = app.clone();
        let bin_label = bin.display().to_string();
        // Hand the actual child to a dedicated task so the command can
        // be killed from a sibling task without holding it across an
        // await.
        tokio::spawn(async move {
            tokio::select! {
                exit = child.wait() => {
                    match exit {
                        Ok(status) if status.success() => {
                            emit_state(&app, HostState::Idle, None);
                        }
                        Ok(status) => {
                            emit_state(
                                &app,
                                HostState::Idle,
                                Some(format!("{bin_label} exited with status {status}")),
                            );
                        }
                        Err(e) => {
                            emit_state(
                                &app,
                                HostState::Idle,
                                Some(format!("waiting on {bin_label}: {e}")),
                            );
                        }
                    }
                }
                _ = shutdown_rx.recv() => {
                    emit_state(&app, HostState::Stopping, None);
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    emit_state(&app, HostState::Idle, None);
                }
            }
        });
    }

    {
        let mut session = state
            .host
            .lock()
            .map_err(|e| format!("host lock poisoned: {e}"))?;
        *session = Some(HostSession { shutdown_tx });
    }

    // The child always reaches "broadcasting" on success. The host
    // binary doesn't emit a structured "ready" line we can latch
    // onto, so we optimistically transition here; the driver task
    // above will flip back to Idle if the child exits early.
    emit_state(&app, HostState::Broadcasting, None);
    Ok(())
}

#[tauri::command]
pub async fn host_stop(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let shutdown_tx = {
        let mut session = state
            .host
            .lock()
            .map_err(|e| format!("host lock poisoned: {e}"))?;
        match session.take() {
            Some(s) => s.shutdown_tx,
            None => return Ok(()),
        }
    };
    let _ = shutdown_tx.try_send(());
    emit_state(&app, HostState::Stopping, None);
    Ok(())
}

#[tauri::command]
pub async fn host_status(state: State<'_, AppState>) -> Result<HostState, String> {
    let session = state
        .host
        .lock()
        .map_err(|e| format!("host lock poisoned: {e}"))?;
    // Session record's existence signals "broadcasting" — there is
    // no intermediate state stored locally; the events stream owns
    // the user-visible transitions beyond that.
    Ok(if session.is_some() {
        HostState::Broadcasting
    } else {
        HostState::Idle
    })
}

fn emit_state(app: &AppHandle, state: HostState, error: Option<String>) {
    let _ = app.emit("host:state", HostStateEvent { state, error });
}

/// Read complete lines from a child's stdout/stderr stream and forward
/// them into the log sink. Optionally emits an additional Tauri event
/// per line (used to peek the host's own startup messages without
/// duplicating parsing).
async fn forward_lines<R>(
    stream: R,
    sink: Arc<LogSink>,
    target: String,
    level: String,
    _app: Option<AppHandle>,
    _passthrough_event: Option<&'static str>,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut reader = BufReader::new(stream).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        ingest_external(&sink, &level, &target, line);
    }
}
