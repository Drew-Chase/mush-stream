//! winit + pixels presentation.
//!
//! winit 0.30 requires the event loop to live on the main thread, so the
//! decode + network workers run on background threads and push decoded
//! [`DecodedFrame`]s into the event loop via [`EventLoopProxy::send_event`].
//!
//! The `Window` is `Box::leak`-ed so `Pixels` (which borrows the window via
//! `SurfaceTexture<'_, W>`) gets a `'static` lifetime — fine for our
//! single-window CLI; the OS reclaims everything on process exit.

use anyhow::{Context, Result};
use pixels::{Pixels, SurfaceTexture};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy},
    window::{Fullscreen, Window, WindowAttributes, WindowId},
};

use crate::config::DisplayConfig;
use crate::decode::DecodedFrame;

/// Events the worker threads send into the winit event loop.
#[derive(Debug)]
pub enum UserEvent {
    /// A new decoded frame is ready to present.
    Frame(DecodedFrame),
    /// Worker is shutting down (e.g. UDP socket closed). Exit the event loop.
    WorkerExited,
}

/// Latency stats observed at present time. M5 fills these in.
#[derive(Debug, Default, Clone, Copy)]
pub struct PresentStats {
    pub frames_presented: u64,
    pub last_glass_to_glass_us: Option<u64>,
}

pub struct DisplayApp {
    config: DisplayConfig,
    window: Option<&'static Window>,
    pixels: Option<Pixels<'static>>,
    last_frame: Option<DecodedFrame>,
    /// Wallclock at the time of the last presented frame's capture.
    /// Updated each redraw so external observers can read latency.
    pub stats: PresentStats,
}

impl DisplayApp {
    pub fn new(config: DisplayConfig) -> Self {
        Self {
            config,
            window: None,
            pixels: None,
            last_frame: None,
            stats: PresentStats::default(),
        }
    }
}

impl ApplicationHandler<UserEvent> for DisplayApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let mut attrs = WindowAttributes::default()
            .with_title(&self.config.title)
            .with_inner_size(PhysicalSize::new(self.config.width, self.config.height))
            .with_visible(true)
            .with_resizable(true);
        if self.config.fullscreen {
            attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(None)));
        }
        let window = match event_loop.create_window(attrs) {
            Ok(w) => w,
            Err(e) => {
                tracing::error!(error = %e, "failed to create window");
                event_loop.exit();
                return;
            }
        };
        // Leak so Pixels gets a 'static borrow. Single-window CLI; the OS
        // cleans up on process exit.
        let window: &'static Window = Box::leak(Box::new(window));

        let surface = SurfaceTexture::new(self.config.width, self.config.height, window);
        match Pixels::new(self.config.width, self.config.height, surface) {
            Ok(pixels) => {
                self.pixels = Some(pixels);
                self.window = Some(window);
                tracing::info!(
                    width = self.config.width,
                    height = self.config.height,
                    "display window created"
                );
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to create pixels surface");
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                tracing::info!("window close requested");
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => {
                if let (Some(frame), Some(pixels)) = (self.last_frame.as_ref(), self.pixels.as_mut())
                {
                    let dst = pixels.frame_mut();
                    let n = dst.len().min(frame.rgba.len());
                    dst[..n].copy_from_slice(&frame.rgba[..n]);
                    if let Err(e) = pixels.render() {
                        tracing::warn!(error = %e, "pixels render failed");
                    } else {
                        self.stats.frames_presented += 1;
                        // Glass-to-glass: time from host capture to client
                        // present. M5 reads these from the proxy thread or
                        // the on-shutdown drain.
                        let now_us = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_micros() as u64)
                            .unwrap_or(0);
                        let delta = now_us.saturating_sub(frame.timestamp_us);
                        self.stats.last_glass_to_glass_us = Some(delta);
                    }
                }
            }
            WindowEvent::Resized(size) => {
                if let Some(pixels) = self.pixels.as_mut() {
                    let _ = pixels.resize_surface(size.width.max(1), size.height.max(1));
                }
            }
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Frame(frame) => {
                self.last_frame = Some(frame);
                if let Some(window) = self.window {
                    window.request_redraw();
                }
            }
            UserEvent::WorkerExited => {
                tracing::info!("worker exited; closing display");
                event_loop.exit();
            }
        }
    }
}

/// Build the winit event loop. Returns the loop together with a proxy that
/// worker threads can use to push events.
pub fn build_event_loop() -> Result<(EventLoop<UserEvent>, EventLoopProxy<UserEvent>)> {
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .context("building winit event loop")?;
    let proxy = event_loop.create_proxy();
    Ok((event_loop, proxy))
}
