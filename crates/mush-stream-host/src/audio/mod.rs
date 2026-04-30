//! Host-side audio: WASAPI loopback capture + Opus encode + transport.
//!
//! Two capture paths, picked by config:
//!
//! - **No blacklist** → single [`LoopbackCapture`] against the default
//!   render endpoint. Captures the full system mix (cheap, simplest).
//! - **Non-empty blacklist** → [`mixer::Mixer`] opens a per-process
//!   WASAPI loopback for every non-blacklisted session and software-
//!   mixes them into one stream. Costs one extra capture per app and
//!   periodic session re-enumeration, but the blacklist actually
//!   filters.
//!
//! Threading: a single dedicated `std::thread` runs the chosen capture
//! loop, the Opus encoder, and the wire-formatting. Encoded datagrams
//! are pushed through the same `datagram_tx` mpsc as video, so they
//! flow through the host's send pacer like everything else.

mod capture;
mod encoder;
mod mixer;
mod process_loopback;
mod sessions;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use mush_stream_common::protocol::{audio, video::HEADER_SIZE};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::config::AudioConfig;

pub use self::capture::{CaptureError, LoopbackCapture};
pub use self::encoder::{EncoderError, OpusEncoder};
pub use self::mixer::{Mixer, MixerError};
pub use self::sessions::list_audio_sessions;

/// 48 kHz is Opus's native rate; libopus accepts {8, 12, 16, 24, 48}
/// but resamples internally for anything but 48. Picking 48 lets us
/// hand WASAPI's auto-converted output straight to libopus.
pub const SAMPLE_RATE: u32 = 48_000;
/// Stereo. Game audio is essentially always stereo or upmixed; if a
/// host has surround output, WASAPI's mix-format auto-conversion
/// downmixes to stereo when we ask for it.
pub const CHANNELS: u16 = 2;
/// 20 ms frames = 960 samples per channel. Opus's sweet spot for
/// low-latency streaming.
pub const FRAME_SAMPLES: usize = 960;

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("WASAPI capture: {0}")]
    Capture(#[from] CaptureError),
    #[error("Opus encoder: {0}")]
    Encoder(#[from] EncoderError),
    #[error("audio mixer: {0}")]
    Mixer(#[from] MixerError),
}

/// Run the audio capture+encode loop on a dedicated thread until
/// `shutdown` flips. Pushes one Opus packet per ~20 ms onto
/// `datagram_tx`.
#[allow(clippy::needless_pass_by_value)] // dedicated thread entry point — owns its inputs
pub fn run_audio_loop(
    cfg: AudioConfig,
    datagram_tx: mpsc::Sender<Vec<u8>>,
    shutdown: Arc<AtomicBool>,
) -> Result<(), AudioError> {
    let mut encoder = OpusEncoder::new(SAMPLE_RATE, CHANNELS, cfg.bitrate_kbps * 1000)?;
    let frame_total: usize = FRAME_SAMPLES * CHANNELS as usize;
    let mut accum: Vec<f32> = Vec::with_capacity(frame_total * 4);

    // Pick the capture path based on blacklist. With no blacklist we
    // grab the system mix straight from the default endpoint (one
    // capture, cheapest). With any blacklist entry we run a
    // per-process mixer instead.
    let mut source: AudioSource = if cfg.blacklist.is_empty() {
        tracing::info!(
            sample_rate = SAMPLE_RATE,
            channels = CHANNELS,
            bitrate_kbps = cfg.bitrate_kbps,
            "audio: system-mix loopback path"
        );
        AudioSource::System(LoopbackCapture::open()?)
    } else {
        tracing::info!(
            sample_rate = SAMPLE_RATE,
            channels = CHANNELS,
            bitrate_kbps = cfg.bitrate_kbps,
            blacklist = ?cfg.blacklist,
            "audio: per-process mixer path with blacklist enforcement"
        );
        let mut mixer = Mixer::new(cfg.blacklist.clone());
        // Initial enumeration so we don't ship a few seconds of
        // silence while waiting for the first refresh tick.
        if let Err(e) = mixer.refresh() {
            tracing::warn!(error = %e, "initial mixer refresh failed");
        }
        AudioSource::Mixer(mixer)
    };

    let mut sequence: u32 = 0;
    let mut datagram_buf = vec![0u8; HEADER_SIZE + audio::MAX_OPUS_PAYLOAD];
    let mut packets_sent: u64 = 0;
    let mut last_log = std::time::Instant::now();
    let mut last_refresh = std::time::Instant::now();

    while !shutdown.load(Ordering::Acquire) {
        // Periodically reconcile the mixer's source set with currently
        // active audio sessions: pick up new apps, drop ones that
        // exited. Cheap (<5 ms typical), once per second.
        if let AudioSource::Mixer(m) = &mut source
            && last_refresh.elapsed() >= std::time::Duration::from_secs(1)
        {
            if let Err(e) = m.refresh() {
                tracing::warn!(error = %e, "mixer refresh failed");
            }
            last_refresh = std::time::Instant::now();
        }

        match &mut source {
            AudioSource::System(c) => {
                c.read_into(&mut accum)?;
            }
            AudioSource::Mixer(m) => {
                // Pull at most one frame worth at a time so the loop
                // stays responsive to shutdown flags and refresh ticks.
                m.read_into(&mut accum, frame_total);
            }
        }

        while accum.len() >= frame_total {
            let pcm: Vec<f32> = accum.drain(..frame_total).collect();
            let opus_packet = encoder.encode(&pcm)?;
            if opus_packet.len() > audio::MAX_OPUS_PAYLOAD {
                tracing::warn!(
                    bytes = opus_packet.len(),
                    "Opus packet exceeds wire MAX_OPUS_PAYLOAD; dropping"
                );
                continue;
            }
            let timestamp_us = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_micros() as u64)
                .unwrap_or(0);

            let header_buf: &mut [u8; HEADER_SIZE] = (&mut datagram_buf[..HEADER_SIZE])
                .try_into()
                .expect("header slice");
            audio::write_header(
                sequence,
                timestamp_us,
                opus_packet.len() as u16,
                header_buf,
            );
            datagram_buf[HEADER_SIZE..HEADER_SIZE + opus_packet.len()]
                .copy_from_slice(&opus_packet);
            let total = HEADER_SIZE + opus_packet.len();
            // Best-effort: drop on full or closed channel.
            let _ = datagram_tx.try_send(datagram_buf[..total].to_vec());
            sequence = sequence.wrapping_add(1);
            packets_sent += 1;
        }

        if last_log.elapsed() >= std::time::Duration::from_secs(1) {
            let active = match &source {
                AudioSource::System(_) => 1,
                AudioSource::Mixer(m) => m.active_sources(),
            };
            tracing::debug!(packets_sent, active_sources = active, "audio throughput (1s)");
            packets_sent = 0;
            last_log = std::time::Instant::now();
        }
    }

    tracing::info!("audio loop exiting");
    Ok(())
}

/// Either path the audio loop can pull samples from. Each method
/// produces interleaved stereo f32 at [`SAMPLE_RATE`] for the encoder.
enum AudioSource {
    System(LoopbackCapture),
    Mixer(Mixer),
}
