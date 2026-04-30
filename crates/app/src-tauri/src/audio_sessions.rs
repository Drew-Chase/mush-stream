//! Audio session enumeration for the Host page's audio-source
//! toggle list.
//!
//! The host crate's `enumerate_sessions` does the WASAPI work; we
//! adapt the result into a serde-friendly shape and run it on a
//! blocking task because COM calls aren't `Send`.

use mush_stream_host::audio::{enumerate_sessions, AudioSession};
use serde::Serialize;
use tauri::async_runtime;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioSessionInfo {
    pub pid: u32,
    /// Leaf exe name (e.g. `chrome.exe`) or `"System"` for the
    /// system-sounds session. This is the value that goes into
    /// `host.toml`'s `[audio].blacklist`.
    pub process_name: String,
    /// Friendly display name from WASAPI; empty for sessions that
    /// don't set one (most apps don't bother).
    pub display_name: String,
    pub is_system: bool,
    pub state: &'static str,
}

impl From<AudioSession> for AudioSessionInfo {
    fn from(s: AudioSession) -> Self {
        Self {
            pid: s.pid,
            process_name: s.process_name,
            display_name: s.display_name,
            is_system: s.is_system,
            state: s.state.as_str(),
        }
    }
}

#[tauri::command]
pub async fn audio_sessions_list() -> Result<Vec<AudioSessionInfo>, String> {
    async_runtime::spawn_blocking(|| {
        enumerate_sessions()
            .map(|sessions| sessions.into_iter().map(AudioSessionInfo::from).collect())
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("audio_sessions_list task panicked: {e}"))?
}
