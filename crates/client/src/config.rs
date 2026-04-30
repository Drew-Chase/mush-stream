//! Client TOML config loader.

use std::{fs, net::SocketAddr, path::Path};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub network: NetworkConfig,
    pub display: DisplayConfig,
    pub decode: DecodeConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub input: InputConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    /// Play the host's audio stream through the default output device.
    /// Set false to mute the audio path locally without touching the
    /// host's capture (useful if you want to listen to your own
    /// machine's audio while screen-sharing).
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Host's UDP address — `<ip>:<port>`. The client connects its UDP
    /// socket here and sends a discovery probe at startup; the host
    /// learns the client's address from that packet's source field and
    /// sends video back to it (UDP hole-punch). The client doesn't need
    /// to bind a specific port on its end.
    pub host: SocketAddr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayConfig {
    pub width: u32,
    pub height: u32,
    #[serde(default = "default_title")]
    pub title: String,
    #[serde(default)]
    pub fullscreen: bool,
}

fn default_title() -> String {
    "app".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputConfig {
    /// Forward gamepad input to the host. When false, the gamepad
    /// poll thread is not spawned at all — the client opens its
    /// network and decode pipeline as usual but emits no
    /// `InputPacket`s.
    #[serde(default = "default_true")]
    pub forward_pad: bool,
    /// Gilrs gamepad id to forward exclusively (the `usize` from
    /// `usize::from(gp.id())`, narrowed to `u32` for JSON-friendly
    /// transport across the Tauri boundary). `None` selects the
    /// first gamepad gilrs reports — backwards-compatible with the
    /// pre-selection behavior.
    #[serde(default)]
    pub gamepad_id: Option<u32>,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            forward_pad: default_true(),
            gamepad_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecodeConfig {
    /// Try `h264_cuvid` first; if it fails to initialize (no NVIDIA GPU,
    /// missing driver), fall back to the software h264 decoder.
    #[serde(default = "default_true")]
    pub prefer_hardware: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read client config `{path}`: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse client config `{path}`: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid client config: {0}")]
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
        if self.display.width == 0 || self.display.height == 0 {
            return Err(ConfigError::Invalid(
                "display.width and display.height must be > 0".into(),
            ));
        }
        Ok(())
    }
}
