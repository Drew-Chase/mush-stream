//! Host-side audio: WASAPI loopback capture + Opus encode + transport.
//!
//! v1 captures the *full* system audio mix (default render endpoint
//! loopback). The per-app blacklist field on [`crate::config::AudioConfig`]
//! is parsed and warned-on-startup but not yet enforced — that's
//! per-process [`IAudioClient`] activation via WASAPI process loopback,
//! tracked as a follow-up.
//!
//! Threading: a single dedicated `std::thread` runs the WASAPI capture
//! loop, the Opus encoder, and the wire-formatting. Encoded datagrams
//! are pushed through the same `datagram_tx` mpsc as video, so they
//! flow through the host's send pacer like everything else.

mod capture;
mod encoder;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use mush_stream_common::protocol::{audio, video::HEADER_SIZE};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::config::AudioConfig;

pub use self::capture::{LoopbackCapture, CaptureError};
pub use self::encoder::{OpusEncoder, EncoderError};

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
    if !cfg.blacklist.is_empty() {
        tracing::warn!(
            blacklist = ?cfg.blacklist,
            "audio blacklist configured; v1 captures full system audio. \
            Per-app exclude lands in a follow-up."
        );
    }

    let mut capture = LoopbackCapture::open()?;
    let mut encoder = OpusEncoder::new(SAMPLE_RATE, CHANNELS, cfg.bitrate_kbps * 1000)?;
    tracing::info!(
        sample_rate = SAMPLE_RATE,
        channels = CHANNELS,
        bitrate_kbps = cfg.bitrate_kbps,
        "audio capture + Opus encoder ready"
    );

    // Interleaved stereo f32 ring. WASAPI returns variable buffer sizes;
    // we accumulate to FRAME_SAMPLES * CHANNELS samples before passing
    // to the encoder.
    let frame_total: usize = FRAME_SAMPLES * CHANNELS as usize;
    let mut accum: Vec<f32> = Vec::with_capacity(frame_total * 4);
    let mut sequence: u32 = 0;
    let mut datagram_buf = vec![0u8; HEADER_SIZE + audio::MAX_OPUS_PAYLOAD];
    let mut packets_sent: u64 = 0;
    let mut last_log = std::time::Instant::now();

    while !shutdown.load(Ordering::Acquire) {
        // ~10 ms wait inside read_into when there's nothing to read.
        capture.read_into(&mut accum)?;

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
            tracing::debug!(packets_sent, "audio throughput (1s)");
            packets_sent = 0;
            last_log = std::time::Instant::now();
        }
    }

    tracing::info!("audio loop exiting");
    Ok(())
}
