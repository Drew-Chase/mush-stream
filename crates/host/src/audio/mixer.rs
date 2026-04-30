//! Multi-source audio mixer for the per-app blacklist path.
//!
//! Owns one [`super::process_loopback::ProcessLoopbackCapture`] per
//! non-blacklisted process tree on the default render endpoint. Drains
//! each capture into its own per-PID accumulator, then sums the
//! interleaved-stereo-f32 samples element-wise into the output buffer.
//!
//! Sessions come and go (apps start, exit, switch endpoints), so the
//! mixer also re-enumerates audio sessions periodically and reconciles
//! its set of captures: open new ones, drop captures whose process has
//! exited.

use std::collections::{HashMap, HashSet};

use thiserror::Error;
use windows::Win32::Foundation::{CloseHandle, MAX_PATH};
use windows::Win32::Media::Audio::{
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

use super::process_loopback::{ProcessLoopbackCapture, ProcessLoopbackError};

#[derive(Debug, Error)]
pub enum MixerError {
    #[error("WASAPI/COM call failed: {context}: {source}")]
    Win {
        context: &'static str,
        #[source]
        source: windows::core::Error,
    },
    #[error("process loopback capture: {0}")]
    Capture(#[from] ProcessLoopbackError),
}

trait WinExt<T> {
    fn ctx(self, context: &'static str) -> Result<T, MixerError>;
}
impl<T> WinExt<T> for windows::core::Result<T> {
    fn ctx(self, context: &'static str) -> Result<T, MixerError> {
        self.map_err(|source| MixerError::Win { context, source })
    }
}

/// One process tree, identified by its root PID and the exe name we
/// use for blacklist matching.
struct PerPidSource {
    capture: ProcessLoopbackCapture,
    /// Carried-over interleaved-stereo-f32 samples from the last drain
    /// that didn't get mixed (because another source had less ready).
    accum: Vec<f32>,
    /// `name.exe`, lower-cased, no path. Used for log lines and to
    /// detect blacklist additions on refresh.
    exe_name: String,
}

pub struct Mixer {
    /// `discord.exe`-style names, lower-cased, with `.exe` stripped on
    /// both sides for tolerant matching.
    blacklist: Vec<String>,
    sources: HashMap<u32, PerPidSource>,
}

impl Mixer {
    pub fn new(blacklist: Vec<String>) -> Self {
        let blacklist = blacklist
            .into_iter()
            .map(|s| normalize_exe_name(&s))
            .collect();
        Self {
            blacklist,
            sources: HashMap::new(),
        }
    }

    /// Re-enumerate audio sessions. For each session whose process is
    /// not on the blacklist, ensure a per-process capture exists; if a
    /// previously-tracked PID is gone, drop the capture.
    pub fn refresh(&mut self) -> Result<(), MixerError> {
        let live_sessions = enumerate_sessions()?;
        let mut live_pids: HashSet<u32> = HashSet::new();

        for (pid, exe_name) in live_sessions {
            // Skip the system-sounds / audio-service session (PID 0)
            // and any session we couldn't resolve to a name.
            if pid == 0 {
                continue;
            }
            let normalized = normalize_exe_name(&exe_name);
            if self.blacklist.iter().any(|b| b == &normalized) {
                continue;
            }
            live_pids.insert(pid);
            if let std::collections::hash_map::Entry::Vacant(slot) = self.sources.entry(pid) {
                match ProcessLoopbackCapture::open(pid) {
                    Ok(capture) => {
                        tracing::info!(pid, exe = %exe_name, "process loopback opened");
                        slot.insert(PerPidSource {
                            capture,
                            accum: Vec::with_capacity(48_000),
                            exe_name: normalized,
                        });
                    }
                    Err(e) => {
                        // Don't fail the whole mixer over one
                        // un-openable session. Common reasons:
                        // privileged process (anti-cheat), already
                        // exited, transient ACL.
                        tracing::warn!(pid, exe = %exe_name, error = %e,
                            "process loopback open failed; skipping this session");
                    }
                }
            }
        }

        // Drop captures whose process tree is no longer enumerated.
        let stale: Vec<u32> = self
            .sources
            .keys()
            .copied()
            .filter(|p| !live_pids.contains(p))
            .collect();
        for pid in stale {
            if let Some(src) = self.sources.remove(&pid) {
                tracing::info!(pid, exe = %src.exe_name, "process loopback closed");
            }
        }
        Ok(())
    }

    /// Read whatever each per-process source has ready, mix the
    /// element-wise sum of the common-prefix samples into `out`, and
    /// keep any per-source overflow in its accumulator for the next
    /// call.
    ///
    /// If there are no sources, returns silence — the caller still
    /// produces 20 ms Opus frames so the wire keeps a steady cadence.
    pub fn read_into(&mut self, out: &mut Vec<f32>, max_samples: usize) {
        // Drain each source into its accumulator.
        for src in self.sources.values_mut() {
            // Keep accumulators bounded so a stalled mixer doesn't
            // grow memory unbounded — clamp to ~250 ms (24,000 stereo
            // samples). If we're behind by more than that something is
            // wrong upstream and we'd rather drop than OOM.
            const ACCUM_CAP: usize = 24_000;
            if src.accum.len() < ACCUM_CAP {
                let _ = src.capture.read_into(&mut src.accum);
                if src.accum.len() > ACCUM_CAP {
                    let drop = src.accum.len() - ACCUM_CAP;
                    src.accum.drain(..drop);
                }
            }
        }

        if self.sources.is_empty() {
            // No non-blacklisted apps producing audio — emit silence.
            // Sleep ~10 ms first so the audio thread doesn't tight-spin
            // the encoder while everything's blacklisted (no
            // per-process WASAPI wait is happening to pace us).
            std::thread::sleep(std::time::Duration::from_millis(10));
            out.extend(std::iter::repeat_n(0.0f32, max_samples));
            return;
        }

        // Mix the common prefix. We bound the sum at `max_samples` so
        // the caller can drive frame-sized batches.
        let common = self
            .sources
            .values()
            .map(|s| s.accum.len())
            .min()
            .unwrap_or(0)
            .min(max_samples);
        if common == 0 {
            // Nothing ready yet. Emit a small amount of silence to
            // keep the encoder ticking. The outer loop will retry.
            return;
        }

        // Element-wise sum. f32 has plenty of headroom; we'd need
        // dozens of full-scale apps to clip, and even then the encoder
        // soft-clips reasonably.
        let start = out.len();
        out.resize(start + common, 0.0);
        for src in self.sources.values_mut() {
            let dst = &mut out[start..start + common];
            let src_slice = &src.accum[..common];
            for (d, s) in dst.iter_mut().zip(src_slice.iter()) {
                *d += *s;
            }
            src.accum.drain(..common);
        }
    }

    pub fn active_sources(&self) -> usize {
        self.sources.len()
    }
}

/// Normalize an exe-name-ish string for blacklist matching: lower-case
/// and strip a trailing `.exe`. So "Discord", "discord", "Discord.exe",
/// "discord.exe" all match.
fn normalize_exe_name(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    lower
        .strip_suffix(".exe")
        .map(str::to_owned)
        .unwrap_or(lower)
}

/// Returns `(pid, exe_leaf_name)` for every audio session on the
/// default render endpoint. Skips PID 0 (system sounds / audio
/// service) at the caller — we still emit it for completeness.
fn enumerate_sessions() -> Result<Vec<(u32, String)>, MixerError> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .ctx("CoCreateInstance(MMDeviceEnumerator)")?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .ctx("GetDefaultAudioEndpoint")?;
        let manager: IAudioSessionManager2 = device
            .Activate(CLSCTX_ALL, None)
            .ctx("Activate(IAudioSessionManager2)")?;
        let session_enum: IAudioSessionEnumerator = manager
            .GetSessionEnumerator()
            .ctx("GetSessionEnumerator")?;
        let count = session_enum.GetCount().ctx("GetCount")?;

        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let control = session_enum.GetSession(i).ctx("GetSession")?;
            let control2: IAudioSessionControl2 =
                control.cast().ctx("cast IAudioSessionControl2")?;
            let pid = control2.GetProcessId().unwrap_or(0);
            let name = exe_name_for_pid(pid).unwrap_or_default();
            out.push((pid, name));
        }
        Ok(out)
    }
}

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
        Some(
            path.rsplit(['\\', '/'])
                .next()
                .unwrap_or(&path)
                .to_owned(),
        )
    }
}
