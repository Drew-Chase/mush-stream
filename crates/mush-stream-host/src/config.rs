//! TOML configuration loader for `mush-stream-host`.

use std::{fs, path::Path};

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub capture: CaptureConfig,
    pub network: NetworkConfig,
    pub encode: EncodeConfig,
    #[serde(default)]
    pub audio: AudioConfig,
}

#[derive(Debug, Deserialize)]
pub struct CaptureConfig {
    /// Index of the DXGI output to capture. `0` is the primary monitor.
    pub output_index: u32,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Deserialize)]
pub struct NetworkConfig {
    /// UDP port the host listens on. Clients send their input/control
    /// packets here (and the first one announces them as the peer the
    /// host should send video to).
    pub listen_port: u16,
    /// When true, attempt to forward `listen_port` through the local
    /// router via UPnP at startup so a remote client can reach this
    /// host without manual port forwarding.
    #[serde(default)]
    pub enable_upnp: bool,
}

#[derive(Debug, Deserialize)]
pub struct EncodeConfig {
    pub bitrate_kbps: u32,
    pub fps: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AudioConfig {
    /// When true (default), capture system audio on the host and stream
    /// it alongside video. Set false to silence the audio path entirely.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Opus target bitrate in kbps. 96 kbps stereo is the default —
    /// pretty much transparent at speech and game-soundtrack range.
    #[serde(default = "default_audio_bitrate")]
    pub bitrate_kbps: u32,
    /// Process names (case-insensitive, with or without `.exe`) whose
    /// audio output should be excluded from the captured mix. The host
    /// enumerates audio sessions on the default render device, captures
    /// each non-blacklisted session via WASAPI process loopback, and
    /// mixes them. Common entries: "discord.exe", "chrome.exe",
    /// "firefox.exe". Default ships with "discord.exe" so voice-chat
    /// audio doesn't get re-streamed to the remote viewer.
    #[serde(default = "default_blacklist")]
    pub blacklist: Vec<String>,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            bitrate_kbps: default_audio_bitrate(),
            blacklist: default_blacklist(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_audio_bitrate() -> u32 {
    96
}

fn default_blacklist() -> Vec<String> {
    vec!["discord.exe".to_owned()]
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file `{path}`: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config file `{path}`: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid config: {0}")]
    Invalid(String),
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let path_str = path.display().to_string();
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path_str.clone(),
            source,
        })?;
        let cfg: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path_str,
            source,
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.capture.width == 0 || self.capture.height == 0 {
            return Err(ConfigError::Invalid(
                "capture.width and capture.height must be > 0".into(),
            ));
        }
        if self.encode.bitrate_kbps == 0 {
            return Err(ConfigError::Invalid(
                "encode.bitrate_kbps must be > 0".into(),
            ));
        }
        if self.encode.fps == 0 {
            return Err(ConfigError::Invalid("encode.fps must be > 0".into()));
        }
        Ok(())
    }
}
