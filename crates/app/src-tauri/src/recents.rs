//! Persistent list of recent host destinations the user has connected
//! to. Stored as a JSON file in the app's config dir.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::state::AppState;

const MAX_RECENTS: usize = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentEntry {
    pub address: String,
    /// Optional human label. Defaults to the host portion of the
    /// address (everything before the colon).
    pub name: String,
    /// Unix-millis timestamp; sorted newest-first.
    pub last_used: i64,
}

pub fn load_from_disk(path: &Path) -> Result<Vec<RecentEntry>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let parsed: Vec<RecentEntry> = serde_json::from_str(&text)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(parsed)
}

fn write_to_disk(path: &Path, entries: &[RecentEntry]) -> Result<()> {
    let json = serde_json::to_string_pretty(entries).context("serializing recents")?;
    fs::write(path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[tauri::command]
pub async fn recents_list(
    state: State<'_, AppState>,
) -> Result<Vec<RecentEntry>, String> {
    let recents = state
        .recents
        .lock()
        .map_err(|e| format!("recents lock poisoned: {e}"))?;
    Ok(recents.clone())
}

#[tauri::command]
pub async fn recents_add(
    address: String,
    state: State<'_, AppState>,
) -> Result<Vec<RecentEntry>, String> {
    let now = chrono::Utc::now().timestamp_millis();
    let name = address.rsplit_once(':').map_or_else(
        || address.clone(),
        |(host, _)| host.to_string(),
    );

    let snapshot = {
        let mut recents = state
            .recents
            .lock()
            .map_err(|e| format!("recents lock poisoned: {e}"))?;
        // Bump-or-insert: drop any prior entry for this address, then push.
        recents.retain(|e| e.address != address);
        recents.insert(
            0,
            RecentEntry {
                address: address.clone(),
                name,
                last_used: now,
            },
        );
        recents.truncate(MAX_RECENTS);
        recents.clone()
    };

    write_to_disk(&state.paths.recents_json, &snapshot)
        .map_err(|e| format!("persisting recents: {e}"))?;
    Ok(snapshot)
}

#[tauri::command]
pub async fn recents_clear(state: State<'_, AppState>) -> Result<(), String> {
    {
        let mut recents = state
            .recents
            .lock()
            .map_err(|e| format!("recents lock poisoned: {e}"))?;
        recents.clear();
    }
    write_to_disk(&state.paths.recents_json, &[])
        .map_err(|e| format!("persisting recents: {e}"))?;
    Ok(())
}
