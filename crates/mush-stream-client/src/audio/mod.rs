//! Client-side audio: receive Opus packets, decode via ffmpeg's libopus
//! decoder, push f32 PCM into a ring buffer, play it through the
//! default output device via cpal.
//!
//! Threading: a dedicated `std::thread` for the decode loop;
//! cpal's data callback runs on its own audio thread and pulls samples
//! from a shared ring buffer.

mod decoder;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use mush_stream_common::protocol::audio::AudioPacket;
use thiserror::Error;
use tokio::sync::mpsc;

pub use self::decoder::{DecoderError, OpusDecoder};

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("Opus decoder: {0}")]
    Decoder(#[from] DecoderError),
    #[error("cpal: no default output device")]
    NoDefaultOutput,
    #[error("cpal: {0}")]
    Cpal(String),
}

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
/// 250 ms of headroom — plenty to absorb any jitter without adding much
/// latency. Tuned along with the host's 20 ms encoder frame size.
const RING_CAPACITY_SAMPLES: usize = SAMPLE_RATE as usize / 4 * CHANNELS as usize;

/// Run the audio decode + playback pipeline. Drains `packet_rx`, decodes
/// each packet into the ring, and lets cpal pump the ring into the
/// default output device. Returns when `packet_rx` closes.
pub fn run_audio_loop(mut packet_rx: mpsc::Receiver<AudioPacket>) -> Result<(), AudioError> {
    // Ring shared between this thread and the cpal data callback.
    let ring: Arc<Mutex<VecDeque<f32>>> =
        Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAPACITY_SAMPLES)));

    let _stream = build_output_stream(ring.clone())?;

    let mut decoder = OpusDecoder::new(SAMPLE_RATE, CHANNELS)?;
    let mut last_seq: Option<u32> = None;
    let mut packets_decoded: u64 = 0;
    let mut packets_dropped: u64 = 0;
    let mut last_log = std::time::Instant::now();

    while let Some(packet) = packet_rx.blocking_recv() {
        // Drop on stale-sequence so reorder doesn't replay older audio.
        if let Some(prev) = last_seq {
            let diff = packet.sequence.wrapping_sub(prev);
            if diff == 0 || diff > u32::MAX / 2 {
                packets_dropped += 1;
                continue;
            }
        }
        last_seq = Some(packet.sequence);

        match decoder.decode(&packet.payload) {
            Ok(samples) => {
                let mut ring_guard = ring.lock().unwrap();
                // If the ring is past capacity, drop old samples. The
                // playback callback is faster than us; hanging on to
                // stale audio would just add latency.
                if ring_guard.len() + samples.len() > RING_CAPACITY_SAMPLES {
                    let drop_count =
                        ring_guard.len() + samples.len() - RING_CAPACITY_SAMPLES;
                    for _ in 0..drop_count.min(ring_guard.len()) {
                        ring_guard.pop_front();
                    }
                }
                ring_guard.extend(samples.iter().copied());
                packets_decoded += 1;
            }
            Err(e) => {
                packets_dropped += 1;
                tracing::warn!(error = %e, "Opus decode failed");
            }
        }

        if last_log.elapsed() >= std::time::Duration::from_secs(1) {
            tracing::debug!(packets_decoded, packets_dropped, "audio decode (1s)");
            packets_decoded = 0;
            packets_dropped = 0;
            last_log = std::time::Instant::now();
        }
    }

    tracing::info!("audio loop exiting");
    Ok(())
}

fn build_output_stream(ring: Arc<Mutex<VecDeque<f32>>>) -> Result<Stream, AudioError> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or(AudioError::NoDefaultOutput)?;
    if let Ok(name) = device.name() {
        tracing::info!(device = name, "cpal default output");
    }

    let config = StreamConfig {
        channels: CHANNELS,
        sample_rate: cpal::SampleRate(SAMPLE_RATE),
        buffer_size: cpal::BufferSize::Default,
    };

    // Build for f32 only — cpal will say if the device doesn't support
    // it, in which case we'd need to add format adaption.
    let err_fn = |err| tracing::warn!(error = %err, "cpal stream error");
    let stream = device
        .build_output_stream(
            &config,
            move |data: &mut [f32], _info| {
                let mut ring_guard = ring.lock().unwrap();
                for sample in data.iter_mut() {
                    *sample = ring_guard.pop_front().unwrap_or(0.0);
                }
            },
            err_fn,
            None,
        )
        .map_err(|e| AudioError::Cpal(format!("build_output_stream: {e}")))?;
    let _ = SampleFormat::F32; // silence unused-import warning if any
    stream
        .play()
        .map_err(|e| AudioError::Cpal(format!("stream.play: {e}")))?;
    tracing::info!(
        sample_rate = SAMPLE_RATE,
        channels = CHANNELS,
        "audio output stream playing"
    );
    Ok(stream)
}
