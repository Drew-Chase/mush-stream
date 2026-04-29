//! DXGI Desktop Duplication capture.
//!
//! Acquires desktop frames from a chosen monitor, GPU-crops to a configured
//! rectangle via `CopySubresourceRegion`, and (for milestone 1 only) reads the
//! cropped pixels back to CPU as BGRA bytes via a staging texture.
//!
//! Milestone 2+ will keep the cropped texture GPU-resident and feed it directly
//! into NVENC; the staging-texture path will move behind a debug feature.

use thiserror::Error;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_RESOURCE_MISC_FLAG, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11CreateDevice,
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

#[derive(Debug, Clone, Copy)]
pub struct CaptureRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("DXGI/D3D11 call failed: {context}: {source}")]
    Win {
        context: &'static str,
        #[source]
        source: windows::core::Error,
    },
    #[error("D3D11CreateDevice did not produce a device or context")]
    DeviceCreationIncomplete,
    #[error(
        "configured capture rect {rx},{ry} {rw}x{rh} extends outside output {ow}x{oh}"
    )]
    RectOutOfBounds {
        rx: u32,
        ry: u32,
        rw: u32,
        rh: u32,
        ow: u32,
        oh: u32,
    },
    #[error("timed out waiting for first desktop frame")]
    FirstFrameTimeout,
    #[error("DXGI access lost (display mode change or secure desktop) — recovery deferred to M2")]
    AccessLost,
}

trait WinExt<T> {
    fn ctx(self, context: &'static str) -> Result<T, CaptureError>;
}

impl<T> WinExt<T> for windows::core::Result<T> {
    fn ctx(self, context: &'static str) -> Result<T, CaptureError> {
        self.map_err(|source| CaptureError::Win { context, source })
    }
}

/// Captures a configured rectangular region of a chosen DXGI output.
pub struct Capturer {
    // Field drop order matters for COM: drop duplication and textures before context/device.
    duplication: IDXGIOutputDuplication,
    crop_tex: ID3D11Texture2D,
    staging_tex: ID3D11Texture2D,
    context: ID3D11DeviceContext,
    _device: ID3D11Device,
    rect: CaptureRect,
    cpu_buf: Vec<u8>,
}

impl Capturer {
    pub fn new(output_index: u32, rect: CaptureRect) -> Result<Self, CaptureError> {
        // 1. DXGI factory + adapter 0.
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ctx("CreateDXGIFactory1")?;
        let adapter: IDXGIAdapter1 =
            unsafe { factory.EnumAdapters1(0) }.ctx("EnumAdapters1(0)")?;

        // 2. D3D11 device + immediate context.
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut feature_level: D3D_FEATURE_LEVEL = D3D_FEATURE_LEVEL_11_0;
        let feature_levels = [D3D_FEATURE_LEVEL_11_0];
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&raw mut device),
                Some(&raw mut feature_level),
                Some(&raw mut context),
            )
        }
        .ctx("D3D11CreateDevice")?;
        let device = device.ok_or(CaptureError::DeviceCreationIncomplete)?;
        let context = context.ok_or(CaptureError::DeviceCreationIncomplete)?;

        // 3. Get the chosen output and validate the crop rect against its bounds.
        let output = unsafe { adapter.EnumOutputs(output_index) }.ctx("EnumOutputs")?;
        let desc = unsafe { output.GetDesc() }.ctx("IDXGIOutput::GetDesc")?;
        let RECT { left, top, right, bottom } = desc.DesktopCoordinates;
        let output1: IDXGIOutput1 = output.cast().ctx("IDXGIOutput -> IDXGIOutput1")?;
        let output_w = (right - left) as u32;
        let output_h = (bottom - top) as u32;
        if rect.x.saturating_add(rect.width) > output_w
            || rect.y.saturating_add(rect.height) > output_h
        {
            return Err(CaptureError::RectOutOfBounds {
                rx: rect.x,
                ry: rect.y,
                rw: rect.width,
                rh: rect.height,
                ow: output_w,
                oh: output_h,
            });
        }

        // 4. Create the duplication.
        let duplication = unsafe { output1.DuplicateOutput(&device) }.ctx("DuplicateOutput")?;

        // 5. Allocate the GPU-resident crop texture (reused by M2+ encoder).
        let crop_desc = D3D11_TEXTURE2D_DESC {
            Width: rect.width,
            Height: rect.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_FLAG(0).0 as u32,
        };
        let mut crop_tex: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&raw const crop_desc, None, Some(&raw mut crop_tex)) }
            .ctx("CreateTexture2D(crop)")?;
        let crop_tex = crop_tex.ok_or(CaptureError::DeviceCreationIncomplete)?;

        // 6. Allocate the staging texture (M1-only CPU readback path).
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: rect.width,
            Height: rect.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging_tex: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&raw const staging_desc, None, Some(&raw mut staging_tex)) }
            .ctx("CreateTexture2D(staging)")?;
        let staging_tex = staging_tex.ok_or(CaptureError::DeviceCreationIncomplete)?;

        let cpu_buf = vec![0u8; (rect.width * rect.height * 4) as usize];

        Ok(Self {
            duplication,
            crop_tex,
            staging_tex,
            context,
            _device: device,
            rect,
            cpu_buf,
        })
    }

    /// Acquires the next desktop frame, GPU-crops it, copies to staging, and
    /// returns tightly-packed BGRA bytes (`width * height * 4`).
    ///
    /// Retries internally on `DXGI_ERROR_WAIT_TIMEOUT` up to `max_attempts`,
    /// since DDA's first acquisitions after `DuplicateOutput` commonly time out
    /// before the compositor has produced a frame.
    pub fn next_frame_bgra(&mut self, max_attempts: u32) -> Result<&[u8], CaptureError> {
        for _ in 0..max_attempts {
            match self.try_one_frame() {
                Ok(()) => return Ok(&self.cpu_buf),
                Err(CaptureError::Win { source, .. })
                    if source.code() == DXGI_ERROR_WAIT_TIMEOUT => {}
                Err(e) => return Err(e),
            }
        }
        Err(CaptureError::FirstFrameTimeout)
    }

    fn try_one_frame(&mut self) -> Result<(), CaptureError> {
        let mut frame_info: DXGI_OUTDUPL_FRAME_INFO = unsafe { std::mem::zeroed() };
        let mut resource: Option<IDXGIResource> = None;
        let acquire_res = unsafe {
            self.duplication
                .AcquireNextFrame(16, &raw mut frame_info, &raw mut resource)
        };
        if let Err(e) = acquire_res {
            if e.code() == DXGI_ERROR_ACCESS_LOST {
                return Err(CaptureError::AccessLost);
            }
            return Err(CaptureError::Win {
                context: "AcquireNextFrame",
                source: e,
            });
        }
        // RAII guard: ensures ReleaseFrame is called even if a later step errors.
        let _frame_guard = FrameGuard {
            duplication: &self.duplication,
        };
        let resource = resource.ok_or(CaptureError::Win {
            context: "AcquireNextFrame returned no resource",
            source: windows::core::Error::from_hresult(windows::core::HRESULT(0)),
        })?;
        let desktop_tex: ID3D11Texture2D =
            resource.cast().ctx("IDXGIResource -> ID3D11Texture2D")?;

        let src_box = D3D11_BOX {
            left: self.rect.x,
            top: self.rect.y,
            front: 0,
            right: self.rect.x + self.rect.width,
            bottom: self.rect.y + self.rect.height,
            back: 1,
        };
        unsafe {
            self.context.CopySubresourceRegion(
                &self.crop_tex,
                0,
                0,
                0,
                0,
                &desktop_tex,
                0,
                Some(&raw const src_box),
            );
            self.context.CopyResource(&self.staging_tex, &self.crop_tex);
        }

        // Map staging, strip RowPitch padding into cpu_buf.
        let mut mapped: D3D11_MAPPED_SUBRESOURCE = unsafe { std::mem::zeroed() };
        unsafe {
            self.context
                .Map(&self.staging_tex, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped))
        }
        .ctx("Map(staging)")?;

        let row_bytes = (self.rect.width * 4) as usize;
        let row_pitch = mapped.RowPitch as usize;
        // SAFETY: we wrote a properly sized cpu_buf in `new`, mapped points to a
        // GPU-mapped region of at least `row_pitch * height` bytes.
        unsafe {
            let src = mapped.pData.cast::<u8>();
            for row in 0..self.rect.height as usize {
                let src_row = src.add(row * row_pitch);
                let dst_row = self.cpu_buf.as_mut_ptr().add(row * row_bytes);
                std::ptr::copy_nonoverlapping(src_row, dst_row, row_bytes);
            }
            self.context.Unmap(&self.staging_tex, 0);
        }

        Ok(())
    }
}

/// RAII guard that calls `ReleaseFrame` on drop — runs even if a later step
/// errored, so we never deadlock the duplication on a held frame.
struct FrameGuard<'a> {
    duplication: &'a IDXGIOutputDuplication,
}

impl Drop for FrameGuard<'_> {
    fn drop(&mut self) {
        // Safety: ReleaseFrame is only valid after a successful AcquireNextFrame;
        // we only construct this guard on that path.
        let _ = unsafe { self.duplication.ReleaseFrame() };
    }
}
