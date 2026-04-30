//! Locate the bundled host/client executables relative to the running
//! Tauri app. Looks for siblings of the current exe (which works for
//! `pnpm tauri dev` where everything lives under `target/debug/`, and
//! for a production install where the bundler places the sidecars next
//! to the main app exe).

use std::path::{Path, PathBuf};

pub fn locate(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    sibling(dir, name)
        .or_else(|| sibling(dir.parent()?, name))
        .or_else(|| {
            // Dev fallback: the Tauri exe in `target/debug/` may have
            // been moved; try the canonical workspace target dirs.
            let workspace = workspace_root()?;
            for profile in ["debug", "release"] {
                if let Some(found) = sibling(&workspace.join("target").join(profile), name) {
                    return Some(found);
                }
            }
            None
        })
}

fn sibling(dir: &Path, name: &str) -> Option<PathBuf> {
    for ext in ["", ".exe"] {
        let candidate = dir.join(format!("{name}{ext}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Walk up from the running exe looking for a `Cargo.lock` (workspace
/// root marker). Returns `None` in production installs where target/
/// is unavailable — locate() then falls through to its earlier
/// sibling checks.
fn workspace_root() -> Option<PathBuf> {
    let mut dir = std::env::current_exe().ok()?;
    while dir.pop() {
        if dir.join("Cargo.lock").is_file() {
            return Some(dir);
        }
    }
    None
}
