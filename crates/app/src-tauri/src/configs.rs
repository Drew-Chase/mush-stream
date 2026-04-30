//! Load + save commands for `host.toml` / `client.toml`.
//!
//! On first run, neither file exists in the app's config dir. We seed
//! both with the schema's `Default`s — using the same structures the
//! host/client crates already deserialize so the running pipeline gets
//! a known-good config immediately.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use tauri::State;

use crate::state::AppState;

// Re-export the host/client config types so the frontend can talk in
// the same shape as the lib crates. Tagged `serde(rename_all)` is
// already applied in the source crates.
pub use mush_stream_client::config::Config as ClientConfig;
pub use mush_stream_host::config::Config as HostConfig;

/// Load the host config from disk, seeding the file with defaults if
/// it doesn't exist yet.
pub fn read_host_config(path: &Path) -> Result<HostConfig> {
    if !path.exists() {
        let cfg = default_host_config();
        write_host_config(path, &cfg)?;
        return Ok(cfg);
    }
    HostConfig::load(path)
        .with_context(|| format!("loading host config from {}", path.display()))
}

pub fn write_host_config(path: &Path, cfg: &HostConfig) -> Result<()> {
    let text = toml::to_string_pretty(cfg).context("serializing host config")?;
    fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn read_client_config(path: &Path) -> Result<ClientConfig> {
    if !path.exists() {
        let cfg = default_client_config();
        write_client_config(path, &cfg)?;
        return Ok(cfg);
    }
    ClientConfig::load(path)
        .with_context(|| format!("loading client config from {}", path.display()))
}

pub fn write_client_config(path: &Path, cfg: &ClientConfig) -> Result<()> {
    let text = toml::to_string_pretty(cfg).context("serializing client config")?;
    fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Default host config — mirrors `host.toml.example` at the workspace
/// root, but parameterized through the same struct types the host crate
/// uses so unknown fields are caught at compile-time.
pub fn default_host_config() -> HostConfig {
    use mush_stream_host::config::{
        AudioConfig, CaptureConfig, EncodeConfig, NetworkConfig,
    };
    HostConfig {
        capture: CaptureConfig {
            output_index: 0,
            x: 0,
            y: 0,
            width: 2560,
            height: 1440,
        },
        network: NetworkConfig {
            listen_port: 9002,
            enable_upnp: false,
        },
        encode: EncodeConfig {
            bitrate_kbps: 9000,
            fps: 60,
        },
        audio: AudioConfig::default(),
    }
}

pub fn default_client_config() -> ClientConfig {
    use mush_stream_client::config::{
        AudioConfig, DecodeConfig, DisplayConfig, NetworkConfig,
    };
    ClientConfig {
        network: NetworkConfig {
            host: "127.0.0.1:9002"
                .parse()
                .expect("hardcoded default address parses"),
        },
        display: DisplayConfig {
            width: 2560,
            height: 1440,
            title: "Mush Stream".to_string(),
            fullscreen: false,
        },
        decode: DecodeConfig {
            prefer_hardware: true,
        },
        audio: AudioConfig::default(),
    }
}

/// Reads the network listen port from the persisted host config.
/// Falls back to `9002` if the file is missing or unparseable —
/// callers use this as a hint for the share-address card and don't
/// need a hard error.
pub fn current_listen_port(state: &AppState) -> Option<u16> {
    HostConfig::load(&state.paths.host_toml)
        .ok()
        .map(|cfg| cfg.network.listen_port)
}

pub fn current_upnp_enabled(state: &AppState) -> Option<bool> {
    HostConfig::load(&state.paths.host_toml)
        .ok()
        .map(|cfg| cfg.network.enable_upnp)
}

// Tauri commands ------------------------------------------------------

#[tauri::command]
pub async fn config_load_host(
    state: State<'_, AppState>,
) -> Result<HostConfig, String> {
    read_host_config(&state.paths.host_toml).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn config_save_host(
    cfg: HostConfig,
    state: State<'_, AppState>,
) -> Result<(), String> {
    write_host_config(&state.paths.host_toml, &cfg).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn config_load_client(
    state: State<'_, AppState>,
) -> Result<ClientConfig, String> {
    read_client_config(&state.paths.client_toml).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn config_save_client(
    cfg: ClientConfig,
    state: State<'_, AppState>,
) -> Result<(), String> {
    write_client_config(&state.paths.client_toml, &cfg).map_err(|e| e.to_string())
}
