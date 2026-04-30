//! winit + pixels presentation, plus an opt-in debug overlay (Ctrl+Alt+D).
//!
//! winit 0.30 requires the event loop to live on the main thread, so the
//! decode + network workers run on background threads and push decoded
//! [`DecodedFrame`]s into the event loop via [`EventLoopProxy::send_event`].
//!
//! The `Window` is `Box::leak`-ed so `Pixels` (which borrows the window via
//! `SurfaceTexture<'_, W>`) gets a `'static` lifetime — fine for our
//! single-window CLI; the OS reclaims everything on process exit.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use font8x8::UnicodeFonts;
use pixels::{
    wgpu::{Color, PresentMode},
    Pixels, PixelsBuilder, ScalingMode, SurfaceTexture,
};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::{ElementState, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy},
    keyboard::{KeyCode, ModifiersState, PhysicalKey},
    window::{Fullscreen, Window, WindowAttributes, WindowId},
};

use crate::config::DisplayConfig;
use crate::decode::DecodedFrame;

/// Events the worker threads send into the winit event loop.
#[derive(Debug)]
pub enum UserEvent {
    /// A new decoded frame is ready to present.
    Frame(DecodedFrame),
    /// Decoder finished init; report the chosen backend to display so the
    /// debug overlay can name it.
    DecoderReady { backend: &'static str },
    /// Worker is shutting down (e.g. UDP socket closed). Exit the event loop.
    WorkerExited,
}

/// Lag stats observed at present time. "Lag" here is the network-arrival
/// → on-screen delay, measured locally via `Instant` so it's honest
/// regardless of cross-machine clock skew. The capture/encode/wire
/// portion of true glass-to-glass is constant and not included.
#[derive(Debug, Default, Clone, Copy)]
pub struct PresentStats {
    pub frames_presented: u64,
    pub last_lag_us: Option<u64>,
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

/// Rolling window of lag samples. Logs a percentile snapshot each time
/// the window fills.
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
        // Window capacity is small (~60), so usize→f64 precision loss
        // isn't a concern.
        #[allow(clippy::cast_precision_loss)]
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

/// Sliding window for the FPS and bitrate readouts in the debug overlay.
const OVERLAY_WINDOW: Duration = Duration::from_secs(2);

pub struct DisplayApp {
    config: DisplayConfig,
    window: Option<&'static Window>,
    pixels: Option<Pixels<'static>>,
    /// Current framebuffer dimensions. Starts at the configured
    /// window size; replaced with the source's true dimensions on the
    /// first decoded frame so `pixels`'s aspect-preserving scaler can
    /// letterbox / pillarbox correctly when the window is resized.
    buffer_size: (u32, u32),
    last_frame: Option<DecodedFrame>,
    /// Cumulative present stats; observers can read this after exit.
    pub stats: PresentStats,
    latency_tracker: LatencyTracker,

    // ---- Debug overlay state (Ctrl+Alt+D toggles `show_debug`) ----
    show_debug: bool,
    modifiers: ModifiersState,
    backend: Option<&'static str>,
    last_snapshot: Option<LatencySnapshot>,
    /// Presentation timestamps over `OVERLAY_WINDOW` for FPS estimation.
    present_window: VecDeque<Instant>,
    /// (arrival time, encoded NAL bytes) over `OVERLAY_WINDOW` for bitrate.
    bitrate_window: VecDeque<(Instant, usize)>,
}

impl DisplayApp {
    pub fn new(config: DisplayConfig) -> Self {
        let buffer_size = (config.width, config.height);
        Self {
            config,
            window: None,
            pixels: None,
            buffer_size,
            last_frame: None,
            stats: PresentStats {
                cumulative_min_us: u64::MAX,
                ..PresentStats::default()
            },
            latency_tracker: LatencyTracker::new(60),
            show_debug: false,
            modifiers: ModifiersState::empty(),
            backend: None,
            last_snapshot: None,
            present_window: VecDeque::with_capacity(128),
            bitrate_window: VecDeque::with_capacity(128),
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
        // Mailbox: latest-frame-wins, GPU never blocks the render() call
        // waiting on vsync. Pixels' default is `AutoVsync` (FIFO), which
        // will stall render() for up to 16 ms (or a *lot* longer if the
        // GPU has any contention). For low-latency streaming we'd
        // rather drop a frame than buffer one. Falls back to FIFO if
        // the surface doesn't support Mailbox.
        let pixels_result =
            PixelsBuilder::new(self.config.width, self.config.height, surface)
                .present_mode(PresentMode::Mailbox)
                // Explicit black margins. With `ScalingMode::Fill`
                // (set below), `pixels` does fractional aspect-
                // preserving fit; when the window's aspect differs
                // from the framebuffer's, the surface clear shows
                // through as letterbox / pillarbox.
                .clear_color(Color::BLACK)
                .build();
        match pixels_result {
            Ok(mut pixels) => {
                // Fractional fit, not pixel-perfect integer scaling.
                // The default `PixelPerfect` mode caps scale at
                // floor(min) ≥ 1.0 — so a 2560×1440 framebuffer in a
                // 1280×720 window would render at 1.0× and clip. Using
                // `Fill` lets a larger source downscale to fit the
                // window while preserving aspect, with margins filled
                // by the clear colour above.
                pixels.set_scaling_mode(ScalingMode::Fill);
                self.pixels = Some(pixels);
                self.window = Some(window);
                tracing::info!(
                    width = self.config.width,
                    height = self.config.height,
                    "display window created (Alt+Enter toggles fullscreen, Ctrl+Alt+D toggles debug overlay)"
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
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
            }
            WindowEvent::KeyboardInput { event: ke, .. } => {
                // Only react to fresh presses; `repeat` would otherwise
                // strobe the toggles when the chord is held.
                if !matches!(ke.state, ElementState::Pressed) || ke.repeat {
                    return;
                }
                match ke.physical_key {
                    // Ctrl+Alt+D toggles the debug overlay.
                    PhysicalKey::Code(KeyCode::KeyD)
                        if self.modifiers.control_key()
                            && self.modifiers.alt_key() =>
                    {
                        self.show_debug = !self.show_debug;
                        tracing::info!(
                            show_debug = self.show_debug,
                            "debug overlay toggled"
                        );
                        if let Some(window) = self.window {
                            window.request_redraw();
                        }
                    }
                    // Alt+Enter toggles borderless fullscreen. Reject
                    // when Ctrl is also held so we don't collide with
                    // future Ctrl+Alt+Enter combos.
                    PhysicalKey::Code(KeyCode::Enter)
                        if self.modifiers.alt_key()
                            && !self.modifiers.control_key() =>
                    {
                        if let Some(window) = self.window {
                            let next = if window.fullscreen().is_some() {
                                None
                            } else {
                                Some(Fullscreen::Borderless(None))
                            };
                            tracing::info!(
                                fullscreen = next.is_some(),
                                "fullscreen toggled (Alt+Enter)"
                            );
                            window.set_fullscreen(next);
                        }
                    }
                    _ => {}
                }
            }
            WindowEvent::RedrawRequested => self.redraw(),
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
                // If the source resolution doesn't match our current
                // framebuffer (first frame, or the host changed its
                // capture rect mid-session), resize the pixels buffer
                // so its aspect ratio matches the source. The built-in
                // ScalingRenderer then letterboxes / pillarboxes inside
                // the window — content is contained, never cropped.
                if (frame.width, frame.height) != self.buffer_size
                    && let Some(pixels) = self.pixels.as_mut()
                {
                    if let Err(e) = pixels.resize_buffer(frame.width, frame.height)
                    {
                        tracing::warn!(
                            error = %e,
                            new_w = frame.width,
                            new_h = frame.height,
                            "pixels.resize_buffer failed; keeping previous size",
                        );
                    } else {
                        tracing::info!(
                            old = ?self.buffer_size,
                            new_w = frame.width,
                            new_h = frame.height,
                            "framebuffer resized to source dimensions",
                        );
                        self.buffer_size = (frame.width, frame.height);
                    }
                }

                // Track recent encoded sizes for the bitrate readout.
                let now = Instant::now();
                self.bitrate_window.push_back((now, frame.encoded_bytes));
                self.trim_overlay_window(now);
                self.last_frame = Some(frame);
                if let Some(window) = self.window {
                    window.request_redraw();
                }
            }
            UserEvent::DecoderReady { backend } => {
                self.backend = Some(backend);
            }
            UserEvent::WorkerExited => {
                tracing::info!("worker exited; closing display");
                event_loop.exit();
            }
        }
    }
}

impl DisplayApp {
    fn redraw(&mut self) {
        // Take both the frame and pixels, run the present, then track
        // latency. Split borrows here let us call `render_overlay` which
        // borrows `&self`.
        let Some(pixels) = self.pixels.as_mut() else {
            return;
        };
        let Some(frame) = self.last_frame.as_ref() else {
            return;
        };
        let dst = pixels.frame_mut();
        let n = dst.len().min(frame.rgba.len());
        dst[..n].copy_from_slice(&frame.rgba[..n]);

        if self.show_debug {
            // Render text directly into the pixels framebuffer before
            // pixels.render() copies it to the GPU. Use the *current*
            // buffer dims (which track the source) — `config.width/
            // height` was just the initial window size and may not
            // match the framebuffer once the source is known.
            let (buf_w, buf_h) = self.buffer_size;
            render_overlay(
                dst,
                buf_w,
                buf_h,
                &OverlayState {
                    backend: self.backend,
                    frames_presented: self.stats.frames_presented + 1, // about to present
                    last_lag_us: self.stats.last_lag_us,
                    last_snapshot: self.last_snapshot,
                    fps: estimate_fps(&self.present_window),
                    bitrate_bps: estimate_bitrate_bps(&self.bitrate_window),
                    show_hint: !self.show_debug, // unused; placeholder
                },
            );
        }

        if let Err(e) = pixels.render() {
            tracing::warn!(error = %e, "pixels render failed");
            return;
        }

        // Stats post-present. Lag = (now - first_packet_instant) — uses
        // monotonic Instant so cross-machine clock skew is irrelevant.
        let now = Instant::now();
        self.present_window.push_back(now);
        self.trim_overlay_window(now);
        self.stats.frames_presented += 1;
        if let Some(first) = self.last_frame.as_ref().map(|f| f.first_packet_instant) {
            let delta = now.saturating_duration_since(first).as_micros();
            #[allow(clippy::cast_possible_truncation)]
            let delta_us = delta.min(u64::MAX as u128) as u64;
            self.stats.last_lag_us = Some(delta_us);
            self.stats.cumulative_min_us = self.stats.cumulative_min_us.min(delta_us);
            self.stats.cumulative_max_us = self.stats.cumulative_max_us.max(delta_us);
            self.stats.cumulative_sum_us = self.stats.cumulative_sum_us.saturating_add(delta_us);
            self.stats.cumulative_samples += 1;
            if let Some(snap) = self.latency_tracker.record(delta_us) {
                tracing::info!(
                    count = snap.count,
                    min_us = snap.min_us,
                    p50_us = snap.p50_us,
                    p95_us = snap.p95_us,
                    p99_us = snap.p99_us,
                    max_us = snap.max_us,
                    avg_us = snap.avg_us,
                    "client lag window"
                );
                self.last_snapshot = Some(snap);
            }
        }
    }

    fn trim_overlay_window(&mut self, now: Instant) {
        while let Some(&t) = self.present_window.front() {
            if now.duration_since(t) > OVERLAY_WINDOW {
                self.present_window.pop_front();
            } else {
                break;
            }
        }
        while let Some(&(t, _)) = self.bitrate_window.front() {
            if now.duration_since(t) > OVERLAY_WINDOW {
                self.bitrate_window.pop_front();
            } else {
                break;
            }
        }
    }
}

fn estimate_fps(window: &VecDeque<Instant>) -> f64 {
    if window.len() < 2 {
        return 0.0;
    }
    let span = window
        .back()
        .unwrap()
        .duration_since(*window.front().unwrap())
        .as_secs_f64();
    if span <= 0.0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let n = (window.len() - 1) as f64;
    n / span
}

fn estimate_bitrate_bps(window: &VecDeque<(Instant, usize)>) -> f64 {
    if window.len() < 2 {
        return 0.0;
    }
    let span = window
        .back()
        .unwrap()
        .0
        .duration_since(window.front().unwrap().0)
        .as_secs_f64();
    if span <= 0.0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let total_bytes = window.iter().map(|(_, n)| *n).sum::<usize>() as f64;
    total_bytes * 8.0 / span
}

/// Build the winit event loop. Returns the loop together with a proxy that
/// worker threads can use to push events.
///
/// On Windows the loop is built with `with_any_thread(true)` so the
/// caller can drive `run_app` from a non-main thread — required for
/// the Tauri shell, which reserves the main thread for its webview.
/// The binary uses the same path so behaviour stays uniform.
pub fn build_event_loop() -> Result<(EventLoop<UserEvent>, EventLoopProxy<UserEvent>)> {
    let mut builder = EventLoop::<UserEvent>::with_user_event();
    #[cfg(windows)]
    {
        use winit::platform::windows::EventLoopBuilderExtWindows;
        builder.with_any_thread(true);
    }
    let event_loop = builder.build().context("building winit event loop")?;
    let proxy = event_loop.create_proxy();
    Ok((event_loop, proxy))
}

// ============================================================================
// Debug overlay rendering
// ============================================================================

struct OverlayState {
    backend: Option<&'static str>,
    frames_presented: u64,
    last_lag_us: Option<u64>,
    last_snapshot: Option<LatencySnapshot>,
    fps: f64,
    bitrate_bps: f64,
    #[allow(dead_code)]
    show_hint: bool,
}

const OVERLAY_SCALE: usize = 2;
const OVERLAY_PAD: usize = 6;
const OVERLAY_LINE_GAP: usize = 2;
const OVERLAY_BG: [u8; 4] = [0, 0, 0, 220];
const OVERLAY_FG: [u8; 4] = [240, 240, 240, 255];
const OVERLAY_DIM: [u8; 4] = [160, 160, 160, 255];

fn render_overlay(buf: &mut [u8], width: u32, height: u32, st: &OverlayState) {
    let lines = build_lines(st);
    let max_chars = lines.iter().map(|(s, _)| s.chars().count()).max().unwrap_or(0);

    let line_h = 8 * OVERLAY_SCALE + OVERLAY_LINE_GAP;
    let panel_w = max_chars * 8 * OVERLAY_SCALE + 2 * OVERLAY_PAD;
    let panel_h = lines.len() * line_h + 2 * OVERLAY_PAD;
    let panel_x = 8;
    let panel_y = 8;

    let w = width as usize;
    let h = height as usize;

    fill_rect(buf, w, h, panel_x, panel_y, panel_w, panel_h, OVERLAY_BG);

    for (i, (text, color)) in lines.iter().enumerate() {
        let y = panel_y + OVERLAY_PAD + i * line_h;
        draw_text(
            buf,
            w,
            h,
            panel_x + OVERLAY_PAD,
            y,
            OVERLAY_SCALE,
            text,
            *color,
        );
    }
}

fn build_lines(st: &OverlayState) -> Vec<(String, [u8; 4])> {
    let mut lines: Vec<(String, [u8; 4])> = Vec::new();
    lines.push(("app  Ctrl+Alt+D".to_owned(), OVERLAY_DIM));
    lines.push((
        format!("backend  {}", st.backend.unwrap_or("?")),
        OVERLAY_FG,
    ));
    lines.push((format!("frames   {}", st.frames_presented), OVERLAY_FG));
    lines.push((format!("fps      {:.1}", st.fps), OVERLAY_FG));
    lines.push((
        format!("rx Mbps  {:.2}", st.bitrate_bps / 1_000_000.0),
        OVERLAY_FG,
    ));
    if let Some(us) = st.last_lag_us {
        lines.push((format!("lag ms   {}", us / 1000), OVERLAY_FG));
    } else {
        lines.push(("lag ms   -".to_owned(), OVERLAY_DIM));
    }
    if let Some(s) = st.last_snapshot {
        lines.push((
            format!(
                "p50/95/99/max ms  {}/{}/{}/{}",
                s.p50_us / 1000,
                s.p95_us / 1000,
                s.p99_us / 1000,
                s.max_us / 1000,
            ),
            OVERLAY_FG,
        ));
    } else {
        lines.push(("p50/95/99/max ms  -".to_owned(), OVERLAY_DIM));
    }
    lines
}

#[allow(clippy::too_many_arguments)] // small overlay helper; struct would be churn
fn fill_rect(
    buf: &mut [u8],
    w: usize,
    h: usize,
    x: usize,
    y: usize,
    rw: usize,
    rh: usize,
    color: [u8; 4],
) {
    let x_end = (x + rw).min(w);
    let y_end = (y + rh).min(h);
    for py in y..y_end {
        let row_off = py * w * 4;
        for px in x..x_end {
            let off = row_off + px * 4;
            // Bounds-checked once per row entry — `dst.copy_from_slice`
            // would also work but feels heavier than 4 byte writes.
            buf[off] = color[0];
            buf[off + 1] = color[1];
            buf[off + 2] = color[2];
            buf[off + 3] = color[3];
        }
    }
}

#[allow(clippy::too_many_arguments)] // small overlay helper; struct would be churn
fn draw_text(
    buf: &mut [u8],
    w: usize,
    h: usize,
    x: usize,
    y: usize,
    scale: usize,
    text: &str,
    fg: [u8; 4],
) {
    for (i, ch) in text.chars().enumerate() {
        let glyph = font8x8::BASIC_FONTS.get(ch).unwrap_or([0u8; 8]);
        let glyph_origin_x = x + i * 8 * scale;
        for (gy, row) in glyph.iter().enumerate() {
            for gx in 0..8 {
                if (row >> gx) & 1 == 0 {
                    continue;
                }
                let cell_x = glyph_origin_x + gx * scale;
                let cell_y = y + gy * scale;
                for sx in 0..scale {
                    for sy in 0..scale {
                        let dx = cell_x + sx;
                        let dy = cell_y + sy;
                        if dx >= w || dy >= h {
                            continue;
                        }
                        let off = (dy * w + dx) * 4;
                        buf[off] = fg[0];
                        buf[off + 1] = fg[1];
                        buf[off + 2] = fg[2];
                        buf[off + 3] = fg[3];
                    }
                }
            }
        }
    }
}
