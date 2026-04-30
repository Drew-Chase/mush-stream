//! Monitor enumeration and one-shot screenshot capture for the
//! Host page's interactive capture-region picker.

use base64::engine::general_purpose;
use base64::Engine;
use mush_stream_host::capture::{CaptureError, CaptureRect, Capturer};
use serde::Serialize;
use tauri::async_runtime;

/// Width the screenshot is downscaled to before PNG-encoding. The
/// preview area is at most ~800px wide on the Host page; anything
/// larger just wastes bytes on the wire and CPU on the encoder.
const SCREENSHOT_TARGET_WIDTH: u32 = 960;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorInfo {
    /// Index passed back to the host as `capture.output_index`.
    pub index: u32,
    /// Friendly label (e.g. "DELL U2720Q" or, when the friendly name
    /// can't be resolved, "DISPLAY1 (2560×1440)").
    pub name: String,
    /// Monitor's full virtual-desktop position. The host crate uses
    /// monitor-local coords for its `CaptureRect`, but the UI needs
    /// the position to label the dropdown ("primary monitor",
    /// "secondary at 2560,0", etc.).
    pub virtual_x: i32,
    pub virtual_y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// `MonitorScreenshot` carries the actual image as a base64-encoded
/// PNG plus the monitor's pixel dimensions so the frontend can map
/// the marquee's percentage coordinates back to monitor pixels.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorScreenshot {
    pub width: u32,
    pub height: u32,
    /// `data:image/png;base64,...` URL ready to drop into an `<img>`.
    pub data_url: String,
}

#[tauri::command]
pub async fn monitors_list() -> Result<Vec<MonitorInfo>, String> {
    async_runtime::spawn_blocking(blocking_list)
        .await
        .map_err(|e| format!("monitor enumeration task panicked: {e}"))?
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn monitor_screenshot(index: u32) -> Result<MonitorScreenshot, String> {
    async_runtime::spawn_blocking(move || blocking_screenshot(index))
        .await
        .map_err(|e| format!("screenshot task panicked: {e}"))?
        .map_err(|e| e.to_string())
}

#[cfg(windows)]
fn blocking_list() -> anyhow::Result<Vec<MonitorInfo>> {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput,
    };

    let mut monitors: Vec<MonitorInfo> = Vec::new();
    // SAFETY: COM calls are single-threaded here; objects drop at end
    // of scope. Mirrors the host's own EnumAdapters1(0) approach so
    // the indices line up with what `Capturer::new` accepts.
    unsafe {
        let factory: IDXGIFactory1 =
            CreateDXGIFactory1().map_err(|e| anyhow::anyhow!("CreateDXGIFactory1: {e}"))?;
        let adapter: IDXGIAdapter1 = factory
            .EnumAdapters1(0)
            .map_err(|e| anyhow::anyhow!("EnumAdapters1(0): {e}"))?;
        for i in 0u32.. {
            let output: IDXGIOutput = match adapter.EnumOutputs(i) {
                Ok(o) => o,
                Err(_) => break, // DXGI_ERROR_NOT_FOUND ends the walk
            };
            let Ok(desc) = output.GetDesc() else { continue };
            let bounds = desc.DesktopCoordinates;
            let width = (bounds.right - bounds.left).max(0) as u32;
            let height = (bounds.bottom - bounds.top).max(0) as u32;
            let device_name = wide_to_string(&desc.DeviceName);
            let friendly = friendly_monitor_name(&device_name)
                .unwrap_or_else(|| short_device_name(&device_name));
            let primary = bounds.left == 0 && bounds.top == 0;
            let label = format!("{friendly} ({width}×{height})");
            monitors.push(MonitorInfo {
                index: i,
                name: label,
                virtual_x: bounds.left,
                virtual_y: bounds.top,
                width,
                height,
                primary,
            });
        }
    }
    Ok(monitors)
}

#[cfg(not(windows))]
fn blocking_list() -> anyhow::Result<Vec<MonitorInfo>> {
    Ok(Vec::new())
}

fn blocking_screenshot(index: u32) -> Result<MonitorScreenshot, anyhow::Error> {
    // Discover the monitor's full pixel dimensions so we capture the
    // entire surface (the user's saved capture rect may be smaller).
    let monitors = blocking_list()?;
    let Some(monitor) = monitors.iter().find(|m| m.index == index) else {
        return Err(anyhow::anyhow!("monitor {index} not found"));
    };

    let rect = CaptureRect {
        x: 0,
        y: 0,
        width: monitor.width,
        height: monitor.height,
    };
    let mut capturer = Capturer::new(index, rect)
        .map_err(|e: CaptureError| anyhow::anyhow!("Capturer::new: {e}"))?;
    // Up to 60 attempts × 16ms = ~1s — enough for DXGI Desktop
    // Duplication to ramp on the first frame after a fresh open.
    let bgra = capturer
        .next_frame_bgra(60)
        .map_err(|e| anyhow::anyhow!("next_frame_bgra: {e}"))?;

    let (out_w, out_h, rgba) = downscale_bgra_to_rgba(
        bgra,
        monitor.width,
        monitor.height,
        SCREENSHOT_TARGET_WIDTH,
    );
    let png_bytes = encode_png(&rgba, out_w, out_h)?;
    let b64 = general_purpose::STANDARD.encode(&png_bytes);
    Ok(MonitorScreenshot {
        width: monitor.width,
        height: monitor.height,
        data_url: format!("data:image/png;base64,{b64}"),
    })
}

/// Nearest-neighbor downscale of a BGRA buffer to an RGBA buffer
/// fitting `target_w` while preserving aspect. Cheap, branch-free,
/// good enough for a static thumbnail. The casts to f32 are
/// intentional: monitor dimensions (≤ 8K) fit well within the
/// mantissa, and we only need pixel-quantized accuracy for
/// nearest-neighbor sampling.
#[allow(clippy::cast_precision_loss, clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn downscale_bgra_to_rgba(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    target_w: u32,
) -> (u32, u32, Vec<u8>) {
    let target_w = target_w.min(src_w).max(1);
    let target_h = ((src_h as f32 * target_w as f32) / src_w as f32).max(1.0) as u32;
    let mut out = vec![0u8; (target_w as usize) * (target_h as usize) * 4];
    let scale_x = src_w as f32 / target_w as f32;
    let scale_y = src_h as f32 / target_h as f32;
    let src_stride = src_w as usize * 4;
    for y in 0..target_h {
        let sy = ((y as f32 + 0.5) * scale_y) as usize;
        let sy = sy.min(src_h as usize - 1);
        let row_off = sy * src_stride;
        for x in 0..target_w {
            let sx = ((x as f32 + 0.5) * scale_x) as usize;
            let sx = sx.min(src_w as usize - 1);
            let src_off = row_off + sx * 4;
            let dst_off = (y as usize * target_w as usize + x as usize) * 4;
            // BGRA → RGBA
            out[dst_off] = src[src_off + 2];
            out[dst_off + 1] = src[src_off + 1];
            out[dst_off + 2] = src[src_off];
            out[dst_off + 3] = src[src_off + 3];
        }
    }
    (target_w, target_h, out)
}

fn encode_png(rgba: &[u8], width: u32, height: u32) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::with_capacity((rgba.len() / 4) + 1024);
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| anyhow::anyhow!("png header: {e}"))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| anyhow::anyhow!("png data: {e}"))?;
    }
    Ok(out)
}

#[cfg(windows)]
fn wide_to_string(wide: &[u16]) -> String {
    let len = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..len])
}

#[cfg(windows)]
fn short_device_name(device: &str) -> String {
    // "\\\\.\\DISPLAY3" → "DISPLAY3"
    device
        .rsplit('\\')
        .next()
        .unwrap_or(device)
        .to_string()
}

/// Best-effort: walk `EnumDisplayDevicesW` against the DXGI device
/// path and return the friendly monitor name (e.g. "Generic PnP
/// Monitor"). Returns `None` when the API can't resolve one — the
/// caller falls back to the short device name.
#[cfg(windows)]
fn friendly_monitor_name(device_name: &str) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Graphics::Gdi::{
        DISPLAY_DEVICEW, EnumDisplayDevicesW,
    };
    use windows::core::PCWSTR;

    let wide_device: Vec<u16> = std::ffi::OsStr::new(device_name)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut info = DISPLAY_DEVICEW {
        cb: u32::try_from(std::mem::size_of::<DISPLAY_DEVICEW>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: device_name is a fixed-length wide string buffer; we
    // pass index 0 to ask for the monitor child of that adapter.
    let ok = unsafe {
        EnumDisplayDevicesW(PCWSTR(wide_device.as_ptr()), 0, &raw mut info, 0)
    };
    if !ok.as_bool() {
        return None;
    }
    let name = wide_to_string(&info.DeviceString);
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}
