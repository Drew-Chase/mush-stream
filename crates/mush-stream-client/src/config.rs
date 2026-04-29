//! Client TOML config loader.

use std::{fs, net::SocketAddr, path::Path};

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub network: NetworkConfig,
    pub display: DisplayConfig,
    pub decode: DecodeConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NetworkConfig {
    /// UDP socket the client binds locally to receive video.
    pub video_bind: SocketAddr,
    /// Address of the host's input/control listener.
    pub host_input_addr: SocketAddr,
    /// When true, attempt to forward `video_bind`'s UDP port through the
    /// local router via UPnP at startup, so the host can reach us
    /// without manual port forwarding. Defaults to false (matching the
    /// spec's "use Tailscale, no NAT traversal" stance — flip on only
    /// if you're not behind a VPN).
    #[serde(default)]
    pub enable_upnp: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DisplayConfig {
    pub width: u32,
    pub height: u32,
    #[serde(default = "default_title")]
    pub title: String,
    #[serde(default)]
    pub fullscreen: bool,
}

fn default_title() -> String {
    "mush-stream".to_owned()
}

#[derive(Debug, Clone, Deserialize)]
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
