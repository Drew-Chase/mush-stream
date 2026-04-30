//! Enumerates the audio sessions on the default render endpoint and
//! prints a copy-paste-friendly list. Wired up to the host's
//! `--list-audio-sessions` CLI flag so users can discover the right
//! process name for the `[audio].blacklist` entry.
//!
//! Uses `IAudioSessionManager2` + `IAudioSessionEnumerator` + the
//! `IAudioSessionControl2` cast to get the PID, then resolves the PID
//! to the exe leaf name via `OpenProcess` + `QueryFullProcessImageNameW`.

use thiserror::Error;
use windows::Win32::Foundation::{CloseHandle, MAX_PATH};
use windows::Win32::Media::Audio::{
    AudioSessionStateActive, AudioSessionStateExpired, AudioSessionStateInactive,
    IAudioSessionControl2, IAudioSessionEnumerator, IAudioSessionManager2,
    IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::core::{Interface, PWSTR};

#[derive(Debug, Error)]
pub enum ListError {
    #[error("WASAPI/COM call failed: {context}: {source}")]
    Win {
        context: &'static str,
        #[source]
        source: windows::core::Error,
    },
}

trait WinExt<T> {
    fn ctx(self, context: &'static str) -> Result<T, ListError>;
}
impl<T> WinExt<T> for windows::core::Result<T> {
    fn ctx(self, context: &'static str) -> Result<T, ListError> {
        self.map_err(|source| ListError::Win { context, source })
    }
}

/// One audio session on the default render endpoint, in structured
/// form. `process_name` is the leaf exe name (e.g. `chrome.exe`) and
/// is what `host.toml`'s `[audio].blacklist` matches against.
#[derive(Debug, Clone)]
pub struct AudioSession {
    pub pid: u32,
    pub process_name: String,
    /// Friendly display name from `IAudioSessionControl::GetDisplayName`.
    /// Empty when the session didn't set one (most common — apps
    /// rarely populate it).
    pub display_name: String,
    pub is_system: bool,
    pub state: SessionState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Active,
    Inactive,
    Expired,
    Unknown,
}

impl SessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "Active",
            Self::Inactive => "Inactive",
            Self::Expired => "Expired",
            Self::Unknown => "Unknown",
        }
    }
}

/// Walk the session enumerator on the default render endpoint and
/// collect each session as an `AudioSession`. Used by the CLI's
/// `--list-audio-sessions` flag (via [`list_audio_sessions`]) and by
/// the Tauri shell's audio-toggle UI.
pub fn enumerate_sessions() -> Result<Vec<AudioSession>, ListError> {
    let mut out = Vec::new();
    unsafe {
        // CoInitializeEx is per-thread; ignore the S_FALSE second-call.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .ctx("CoCreateInstance(MMDeviceEnumerator)")?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .ctx("GetDefaultAudioEndpoint(eRender, eConsole)")?;
        let manager: IAudioSessionManager2 = device
            .Activate(CLSCTX_ALL, None)
            .ctx("device.Activate(IAudioSessionManager2)")?;
        let session_enum: IAudioSessionEnumerator = manager
            .GetSessionEnumerator()
            .ctx("IAudioSessionManager2::GetSessionEnumerator")?;
        let count = session_enum
            .GetCount()
            .ctx("IAudioSessionEnumerator::GetCount")?;

        for i in 0..count {
            let control = session_enum
                .GetSession(i)
                .ctx("IAudioSessionEnumerator::GetSession")?;
            let control2: IAudioSessionControl2 = control
                .cast()
                .ctx("IAudioSessionControl::cast::<IAudioSessionControl2>")?;

            let pid = control2.GetProcessId().unwrap_or(0);
            // IsSystemSoundsSession returns S_OK for the system-sounds
            // session and S_FALSE otherwise. windows-rs's `.ok()`
            // converts both to Ok(()) (S_FALSE is non-negative HRESULT,
            // technically a success), so we go through the vtable for
            // the raw HRESULT and compare against S_OK.
            let is_system = is_system_sounds_session(&control2);

            let state = match control.GetState() {
                Ok(s) if s == AudioSessionStateActive => SessionState::Active,
                Ok(s) if s == AudioSessionStateInactive => SessionState::Inactive,
                Ok(s) if s == AudioSessionStateExpired => SessionState::Expired,
                _ => SessionState::Unknown,
            };

            // GetDisplayName returns a COM-allocated PWSTR. We accept
            // the tiny per-session leak rather than wiring CoTaskMemFree.
            let display_name = control
                .GetDisplayName()
                .ok()
                .and_then(|p: PWSTR| if p.is_null() { None } else { p.to_string().ok() })
                .filter(|s| !s.is_empty())
                .unwrap_or_default();

            let process_name = if is_system {
                "System".to_owned()
            } else {
                exe_name_for_pid(pid).unwrap_or_else(|| format!("pid:{pid}"))
            };

            out.push(AudioSession {
                pid,
                process_name,
                display_name,
                is_system,
                state,
            });
        }
    }
    Ok(out)
}

/// Print the list of audio sessions on the default render endpoint.
/// Each line is one session; the "Process" column is what you copy
/// into `host.toml` under `[audio].blacklist`.
pub fn list_audio_sessions() -> Result<(), ListError> {
    let sessions = enumerate_sessions()?;
    println!();
    println!(
        "Audio sessions on the default render endpoint ({} total):",
        sessions.len()
    );
    println!();
    println!(
        "{:<6}  {:<9}  {:<6}  {:<28}  Display",
        "PID", "State", "System", "Process",
    );
    println!(
        "{:<6}  {:<9}  {:<6}  {:<28}  -------",
        "----", "---------", "------", "----------------------------",
    );
    for s in &sessions {
        println!(
            "{:<6}  {:<9}  {:<6}  {:<28}  {}",
            s.pid,
            s.state.as_str(),
            if s.is_system { "yes" } else { "-" },
            s.process_name,
            if s.display_name.is_empty() {
                "(none)"
            } else {
                &s.display_name
            },
        );
    }
    println!();
    println!("Copy a value from the \"Process\" column (case-insensitive) into");
    println!("host.toml under [audio].blacklist to exclude that app's audio");
    println!("from the streamed mix. The host opens a per-process WASAPI");
    println!("loopback for every non-blacklisted session and software-mixes");
    println!("them into the Opus stream.");
    println!();
    Ok(())
}

/// True iff the session is the system-sounds session. We bypass the
/// `windows::core::Result` wrapper because S_FALSE (1) is a positive
/// HRESULT — `Result::ok()` would treat it as success and lose the
/// distinction we need.
fn is_system_sounds_session(control2: &IAudioSessionControl2) -> bool {
    unsafe {
        let vtbl = Interface::vtable(control2);
        let raw = Interface::as_raw(control2);
        let hr = (vtbl.IsSystemSoundsSession)(raw);
        // S_OK == 0 means "yes, this is the system-sounds session".
        hr.0 == 0
    }
}

/// Resolve a PID to its exe leaf name (e.g. `chrome.exe`). Returns
/// `None` if the process can't be opened (insufficient privilege,
/// already exited, etc.).
fn exe_name_for_pid(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; MAX_PATH as usize];
        let mut size: u32 = buf.len() as u32;
        let res = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &raw mut size,
        );
        let _ = CloseHandle(handle);
        res.ok()?;
        let path = String::from_utf16_lossy(&buf[..size as usize]);
        // Strip the directory; keep just the "name.exe" leaf.
        Some(
            path.rsplit(['\\', '/'])
                .next()
                .unwrap_or(&path)
                .to_owned(),
        )
    }
}
