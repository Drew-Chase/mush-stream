//! Gamepad enumeration for the Connect page's "which controller to
//! forward" dropdown. Thin wrapper around the client crate's gilrs-
//! backed `list_gamepads()`; we map the internal type into a
//! camelCase serde shape the React frontend can consume directly.

use serde::Serialize;

use mush_stream_client::input::{self, GamepadInfo};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GamepadInfoView {
    /// gilrs gamepad id (the `usize::from(GamepadId)` value, narrowed
    /// to `u32`). Stable for the lifetime of the host process; the
    /// frontend passes this back in `ConnectOptions.gamepadId` to
    /// pin which controller gets forwarded.
    pub id: u32,
    pub name: String,
    pub is_connected: bool,
}

impl From<GamepadInfo> for GamepadInfoView {
    fn from(info: GamepadInfo) -> Self {
        Self {
            id: info.id,
            name: info.name,
            is_connected: info.is_connected,
        }
    }
}

/// List the gamepads gilrs currently reports. Returns an empty Vec
/// when gilrs init fails (no XInput / evdev / IOKit) — the frontend
/// renders that as "no gamepads detected" without throwing.
///
/// `Gilrs::new()` is a few-millisecond syscall on Windows; we run it
/// on a blocking task so the Tauri command worker isn't stalled.
#[tauri::command]
pub async fn gamepads_list() -> Result<Vec<GamepadInfoView>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        input::list_gamepads()
            .into_iter()
            .map(GamepadInfoView::from)
            .collect()
    })
    .await
    .map_err(|e| format!("gamepads_list join failed: {e}"))
}
