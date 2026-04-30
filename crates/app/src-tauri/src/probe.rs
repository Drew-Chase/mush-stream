//! System capability probes.
//!
//! Runs synchronous checks against the host machine and reports what's
//! available so the UI can show real values in the sidebar / Home
//! "system check" card. Each probe is best-effort; failure of one probe
//! never aborts the others.

use serde::Serialize;
use tauri::async_runtime;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemProbe {
    /// Overall "ready" flag: NVENC ok AND ViGEm ok AND ffmpeg linked.
    pub ready: bool,
    pub nvenc: ProbeRow,
    pub nvdec: ProbeRow,
    pub vigem: ProbeRow,
    pub ffmpeg: ProbeRow,
    pub udp_port: ProbeRow,
    pub upnp: ProbeRow,
    /// "NVIDIA GeForce RTX 4070" — used by the sidebar to label the GPU.
    pub gpu_label: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeRow {
    pub status: ProbeStatus,
    /// Short label for the system check ("NVIDIA RTX 4070", "v1.22.0",
    /// "h264_cuvid", "free", "off").
    pub detail: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProbeStatus {
    Ok,
    Mid,
    Bad,
}

#[tauri::command]
pub async fn system_probe() -> Result<SystemProbe, String> {
    async_runtime::spawn_blocking(blocking_probe)
        .await
        .map_err(|e| format!("probe task panicked: {e}"))
}

fn blocking_probe() -> SystemProbe {
    let gpu_label = probe_gpu();
    let nvenc = probe_nvenc();
    let nvdec = probe_nvdec();
    let vigem = probe_vigem();
    let udp_port = probe_udp_9002();
    let upnp = ProbeRow {
        status: ProbeStatus::Mid,
        detail: "off".into(),
    };
    let ffmpeg = ProbeRow {
        status: ProbeStatus::Ok,
        detail: "8.1 shared".into(),
    };

    let ready = matches!(nvenc.status, ProbeStatus::Ok)
        && matches!(vigem.status, ProbeStatus::Ok)
        && matches!(ffmpeg.status, ProbeStatus::Ok);

    SystemProbe {
        ready,
        nvenc,
        nvdec,
        vigem,
        ffmpeg,
        udp_port,
        upnp,
        gpu_label,
    }
}

/// Try to construct an NVENC encoder at a tiny resolution. Success
/// implies an NVIDIA driver is present and ffmpeg can locate the
/// `h264_nvenc` codec.
fn probe_nvenc() -> ProbeRow {
    use mush_stream_host::encode::VideoEncoder;
    match VideoEncoder::new(640, 360, 30, 1_000_000, false) {
        Ok(_) => ProbeRow {
            status: ProbeStatus::Ok,
            detail: "h264_nvenc".into(),
        },
        Err(e) => ProbeRow {
            status: ProbeStatus::Bad,
            detail: short_err(&e),
        },
    }
}

/// Try to construct a hardware-preferred decoder. The decoder reports
/// the backend it actually selected (`h264_cuvid` on NVIDIA, plain
/// `h264` for software fallback).
fn probe_nvdec() -> ProbeRow {
    use mush_stream_client::decode::VideoDecoder;
    match VideoDecoder::new(true, 640, 360) {
        Ok(d) => {
            let backend = d.backend();
            let status = if backend.contains("cuvid") {
                ProbeStatus::Ok
            } else {
                ProbeStatus::Mid
            };
            ProbeRow {
                status,
                detail: backend.to_string(),
            }
        }
        Err(e) => ProbeRow {
            status: ProbeStatus::Bad,
            detail: short_err(&e),
        },
    }
}

/// Try to attach to the ViGEmBus driver. Drops the connection
/// immediately so we don't hold the slot.
fn probe_vigem() -> ProbeRow {
    use mush_stream_host::vigem::VirtualGamepad;
    match VirtualGamepad::connect() {
        Ok(_pad) => {
            let detail = vigem_driver_version()
                .unwrap_or_else(|| "available".to_string());
            ProbeRow {
                status: ProbeStatus::Ok,
                detail,
            }
        }
        Err(_) => ProbeRow {
            status: ProbeStatus::Bad,
            detail: "missing".into(),
        },
    }
}

/// Try to bind a fresh UDP socket to 0.0.0.0:9002 and immediately drop
/// it. If the bind fails, the port is in use (probably by an active
/// host instance — or another listener).
fn probe_udp_9002() -> ProbeRow {
    match std::net::UdpSocket::bind("0.0.0.0:9002") {
        Ok(_) => ProbeRow {
            status: ProbeStatus::Ok,
            detail: "free".into(),
        },
        Err(_) => ProbeRow {
            status: ProbeStatus::Mid,
            detail: "in use".into(),
        },
    }
}

/// Enumerate DXGI adapters and return the first non-Microsoft (i.e.
/// not "Microsoft Basic Render Driver") description we find. Used by
/// the sidebar to label the NVENC row as "NVENC RTX 4070".
fn probe_gpu() -> String {
    #[cfg(windows)]
    {
        if let Some(name) = enumerate_dxgi_adapters() {
            return name;
        }
    }
    "GPU".to_string()
}

#[cfg(windows)]
fn enumerate_dxgi_adapters() -> Option<String> {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1,
    };

    // SAFETY: Single-threaded factory creation; we ask for the
    // first-adapter description and immediately drop the COM objects.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        for i in 0u32.. {
            let adapter: IDXGIAdapter1 = factory.EnumAdapters1(i).ok()?;
            let desc = adapter.GetDesc1().ok()?;
            // Trim the trailing nuls in the fixed-length description buffer.
            let len = desc
                .Description
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(desc.Description.len());
            let name = String::from_utf16_lossy(&desc.Description[..len]);
            // Skip the software fallback adapter so we get the real GPU.
            if !name.contains("Microsoft Basic Render Driver") {
                return Some(name.trim().to_string());
            }
        }
        None
    }
}

/// Read `HKLM\SYSTEM\CurrentControlSet\Services\ViGEmBus\DisplayName`
/// (the service registry entry created by ViGEmBus' installer). The
/// value is something like "Virtual Gamepad Emulation Bus 1.22.0".
#[cfg(windows)]
fn vigem_driver_version() -> Option<String> {
    use windows::Win32::System::Registry::{
        HKEY_LOCAL_MACHINE, REG_VALUE_TYPE, RegGetValueW, RRF_RT_REG_SZ,
    };
    use windows::core::{HSTRING, PCWSTR};

    let key: HSTRING =
        HSTRING::from(r"SYSTEM\CurrentControlSet\Services\ViGEmBus");
    let value: HSTRING = HSTRING::from("DisplayName");
    let mut buf = [0u16; 256];
    let mut size: u32 = (buf.len() * 2) as u32;
    let mut kind = REG_VALUE_TYPE::default();

    // SAFETY: caller-provided buffer is sized; size is updated by
    // RegGetValueW to bytes-written including the trailing nul.
    let res = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            Some(&raw mut kind),
            Some(buf.as_mut_ptr().cast()),
            Some(&raw mut size),
        )
    };
    if res.is_err() {
        return None;
    }
    // size is in bytes including the trailing nul.
    let chars = (size as usize) / 2;
    let len = chars.saturating_sub(1).min(buf.len());
    let display = String::from_utf16_lossy(&buf[..len]);
    Some(display.trim().to_string())
}

#[cfg(not(windows))]
fn vigem_driver_version() -> Option<String> {
    None
}

fn short_err<E: std::fmt::Display>(e: &E) -> String {
    let s = e.to_string();
    s.lines().next().unwrap_or("error").to_string()
}
