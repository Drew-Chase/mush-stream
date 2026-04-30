//! Tauri app entry point. Wires up the in-process tracing layer,
//! seeds persistent state from disk, registers commands, and spawns
//! the log-emitter task.

mod addresses;
mod client_session;
mod configs;
mod host_session;
mod logs;
mod monitors;
mod probe;
mod recents;
mod state;

use tauri::Manager;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use crate::logs::{ChannelLayer, LogSink};
use crate::state::{AppPaths, AppState};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
#[allow(clippy::too_many_lines)] // setup closure + handler list together
pub fn run() {
    let log_sink = LogSink::new();

    // Install the global tracing subscriber: pretty stderr formatter
    // for `pnpm tauri dev` plus a layer that forwards every event into
    // the LogSink (which the frontend tails via `app:log`).
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,mush_stream=debug"));
    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(ChannelLayer::new(log_sink.clone()))
        .init();

    tracing::info!("mush-stream Tauri app starting");

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(log_sink.clone())
        .setup(move |app| {
            let handle = app.handle().clone();
            let paths = AppPaths::from_app(&handle)?;

            // Seed recents from disk (best-effort).
            let recents = recents::load_from_disk(&paths.recents_json).unwrap_or_default();
            let app_state = AppState::new(paths);
            {
                let mut guard = app_state
                    .recents
                    .lock()
                    .expect("freshly-built mutex cannot be poisoned");
                *guard = recents;
            }
            handle.manage(app_state);

            // Forward all log events to the frontend.
            logs::spawn_emitter(handle.clone(), log_sink.clone());

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            probe::system_probe,
            monitors::monitors_list,
            monitors::monitor_screenshot,
            addresses::host_addresses,
            recents::recents_list,
            recents::recents_add,
            recents::recents_clear,
            configs::config_load_host,
            configs::config_save_host,
            configs::config_load_client,
            configs::config_save_client,
            host_session::host_start,
            host_session::host_stop,
            host_session::host_status,
            client_session::client_connect,
            client_session::client_disconnect,
            client_session::client_status,
            logs::logs_buffer,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
