//! TOML configuration loader for `mush-stream-host`.

use std::{fs, net::SocketAddr, path::Path};

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub capture: CaptureConfig,
    // `network` is loaded but unused until M3; declared now so the schema is
    // stable across milestones.
    #[allow(dead_code)]
    pub network: NetworkConfig,
    pub encode: EncodeConfig,
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
#[allow(dead_code)]
pub struct NetworkConfig {
    pub video_bind: SocketAddr,
    pub input_bind: SocketAddr,
    pub peer: SocketAddr,
    /// When true, attempt to forward `input_bind`'s UDP port through the
    /// local router via UPnP at startup, so a remote client can reach
    /// us without manual port forwarding. Defaults to false (matching
    /// the spec's "use Tailscale, no NAT traversal" stance — flip on
    /// only if you're not behind a VPN).
    #[serde(default)]
    pub enable_upnp: bool,
}

#[derive(Debug, Deserialize)]
pub struct EncodeConfig {
    pub bitrate_kbps: u32,
    pub fps: u32,
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
