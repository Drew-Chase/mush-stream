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

/// Latency stats observed at present time.
#[derive(Debug, Default, Clone, Copy)]
pub struct PresentStats {
    pub frames_presented: u64,
    pub last_glass_to_glass_us: Option<u64>,
    pub cumulative_min_us: u64,
    pub cumulative_max_us: u64,
    pub cumulative_sum_us: u64,
    pub cumulative_samples: u64,
}

impl PresentStats {
    pub fn cumulative_avg_us(&self) -> Option<u64> {
        (self.cumulative_samples > 0)
            .then(|| self.cumulative_sum_us / self.cumulative_samples)
    }
}

/// Rolling window of glass-to-glass latency samples. Logs a percentile
/// snapshot each time the window fills.
#[derive(Debug)]
struct LatencyTracker {
    capacity: usize,
    samples: Vec<u64>,
}

impl LatencyTracker {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            samples: Vec::with_capacity(capacity),
        }
    }

    fn record(&mut self, us: u64) -> Option<LatencySnapshot> {
        self.samples.push(us);
        if self.samples.len() < self.capacity {
            return None;
        }
        self.samples.sort_unstable();
        let n = self.samples.len();
        let pct = |p: f64| self.samples[((n as f64 - 1.0) * p).round() as usize];
        let snap = LatencySnapshot {
            count: n as u64,
            min_us: self.samples[0],
            p50_us: pct(0.50),
            p95_us: pct(0.95),
            p99_us: pct(0.99),
            max_us: self.samples[n - 1],
            avg_us: self.samples.iter().sum::<u64>() / n as u64,
        };
        self.samples.clear();
        Some(snap)
    }
}

/// One window's worth of latency stats — logged at info level.
#[derive(Debug, Clone, Copy)]
pub struct LatencySnapshot {
    pub count: u64,
    pub min_us: u64,
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
    pub avg_us: u64,
}

pub struct DisplayApp {
    config: DisplayConfig,
    window: Option<&'static Window>,
    pixels: Option<Pixels<'static>>,
    last_frame: Option<DecodedFrame>,
    /// Cumulative present stats; observers can read this after exit.
    pub stats: PresentStats,
    latency_tracker: LatencyTracker,
}

impl DisplayApp {
    pub fn new(config: DisplayConfig) -> Self {
        // Window of 60 samples ≈ 1 second at 60fps. Logs a snapshot per
        // window so glass-to-glass percentiles are visible at info level
        // without flooding for every frame.
        Self {
            config,
            window: None,
            pixels: None,
            last_frame: None,
            stats: PresentStats {
                cumulative_min_us: u64::MAX,
                ..PresentStats::default()
            },
            latency_tracker: LatencyTracker::new(60),
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
                        // present. host and client must share a clock for
                        // the absolute number to be meaningful — fine on
                        // localhost (M5 target); for cross-machine M4+
                        // testing rely on Tailscale + NTP.
                        let now_us = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_micros() as u64)
                            .unwrap_or(0);
                        let delta = now_us.saturating_sub(frame.timestamp_us);
                        self.stats.last_glass_to_glass_us = Some(delta);
                        self.stats.cumulative_min_us =
                            self.stats.cumulative_min_us.min(delta);
                        self.stats.cumulative_max_us =
                            self.stats.cumulative_max_us.max(delta);
                        self.stats.cumulative_sum_us =
                            self.stats.cumulative_sum_us.saturating_add(delta);
                        self.stats.cumulative_samples += 1;
                        if let Some(snap) = self.latency_tracker.record(delta) {
                            tracing::info!(
                                count = snap.count,
                                min_us = snap.min_us,
                                p50_us = snap.p50_us,
                                p95_us = snap.p95_us,
                                p99_us = snap.p99_us,
                                max_us = snap.max_us,
                                avg_us = snap.avg_us,
                                "glass-to-glass latency window"
                            );
                        }
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
