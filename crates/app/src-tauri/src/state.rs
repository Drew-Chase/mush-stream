//! Shared mutable application state managed by `tauri::Manager`.

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};
use tauri::{AppHandle, Manager};

use crate::client_session::ClientSession;
use crate::host_session::HostSession;
use crate::recents::RecentEntry;

/// Filesystem paths derived once at startup.
pub struct AppPaths {
    /// Root of the per-user config dir. Not currently read directly —
    /// every consumer goes through one of the typed paths below — but
    /// kept for callers that want to write ad-hoc files (e.g. future
    /// log dumps).
    #[allow(dead_code)]
    pub config_dir: PathBuf,
    pub host_toml: PathBuf,
    pub client_toml: PathBuf,
    pub recents_json: PathBuf,
}

impl AppPaths {
    pub fn from_app(app: &AppHandle) -> Result<Self> {
        let config_dir = app
            .path()
            .app_config_dir()
            .context("resolving app_config_dir")?;
        std::fs::create_dir_all(&config_dir).with_context(|| {
            format!("creating app config dir at {}", config_dir.display())
        })?;
        Ok(Self {
            host_toml: config_dir.join("host.toml"),
            client_toml: config_dir.join("client.toml"),
            recents_json: config_dir.join("recents.json"),
            config_dir,
        })
    }
}

/// Single piece of state held in `tauri::Manager::manage` for the
/// lifetime of the app.
pub struct AppState {
    pub paths: AppPaths,
    pub host: Mutex<Option<HostSession>>,
    pub client: Mutex<Option<ClientSession>>,
    pub recents: Mutex<Vec<RecentEntry>>,
}

impl AppState {
    pub fn new(paths: AppPaths) -> Self {
        Self {
            paths,
            host: Mutex::new(None),
            client: Mutex::new(None),
            recents: Mutex::new(Vec::new()),
        }
    }
}
