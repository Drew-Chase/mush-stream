//! Opus decoder via ffmpeg-the-third (`libopus`). Output is interleaved
//! f32 stereo, ready to push into a cpal output stream.

use ffmpeg_the_third as ffmpeg;
use ffmpeg::{
    Packet,
    codec,
    format::Sample,
    frame,
    util::channel_layout::ChannelLayoutMask,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DecoderError {
    #[error("could not find decoder `libopus`")]
    OpusNotFound,
    #[error("ffmpeg call failed: {context}: {source}")]
    Ffmpeg {
        context: &'static str,
        #[source]
        source: ffmpeg::Error,
    },
    #[error("ffmpeg global init failed: {0}")]
    Init(#[source] ffmpeg::Error),
}

trait FfErrCtx<T> {
    fn ff(self, context: &'static str) -> Result<T, DecoderError>;
}
impl<T> FfErrCtx<T> for Result<T, ffmpeg::Error> {
    fn ff(self, context: &'static str) -> Result<T, DecoderError> {
        self.map_err(|source| DecoderError::Ffmpeg { context, source })
    }
}

pub struct OpusDecoder {
    decoder: codec::decoder::Audio,
    channels: u16,
}

impl OpusDecoder {
    pub fn new(sample_rate: u32, channels: u16) -> Result<Self, DecoderError> {
        ffmpeg::init().map_err(DecoderError::Init)?;
        let codec =
            codec::decoder::find_by_name("libopus").ok_or(DecoderError::OpusNotFound)?;

        let ctx = codec::Context::new_with_codec(codec);
        // Set expected output rate / channel layout so the decoder
        // doesn't need to discover them from the stream.
        // Audio decoders configure via the underlying AVCodecContext;
        // ffmpeg-the-third doesn't expose audio-side setters as widely
        // as it does for video, so we lean on libopus's defaults
        // (matches the encoder).
        let _ = sample_rate;
        let _ = ChannelLayoutMask::STEREO;

        let decoder = ctx
            .decoder()
            .open_as(codec)
            .and_then(codec::decoder::Opened::audio)
            .ff("decoder().open_as.audio()")?;

        Ok(Self { decoder, channels })
    }

    /// Decode one Opus packet into interleaved f32 stereo samples.
    pub fn decode(&mut self, opus_bytes: &[u8]) -> Result<Vec<f32>, DecoderError> {
        let packet = Packet::copy(opus_bytes);
        self.decoder.send_packet(&packet).ff("send_packet")?;

        let mut decoded = frame::Audio::empty();
        let mut out: Vec<f32> = Vec::new();
        loop {
            match self.decoder.receive_frame(&mut decoded) {
                Ok(()) => append_f32_as_interleaved(
                    &decoded,
                    self.channels as usize,
                    &mut out,
                ),
                Err(ffmpeg::Error::Eof | ffmpeg::Error::Other { .. }) => break,
                Err(e) => {
                    return Err(DecoderError::Ffmpeg {
                        context: "receive_frame",
                        source: e,
                    });
                }
            }
        }
        Ok(out)
    }
}

/// i16 -> f32 in [-1.0, 1.0]. 32768 (= |i16::MIN|) maps the most-
/// negative input to exactly -1.0 — the cleanest symmetric mapping for
/// cpal's f32 stream.
const I16_TO_F32_SCALE: f32 = 1.0 / 32_768.0;

/// Append decoded f32 samples to `out` in interleaved order.
///
/// The libopus build that ships with our ffmpeg emits `flt` (interleaved
/// f32) from its decoder, but the wrapper handles both layouts so we
/// don't depend on the build's behavior.
fn append_f32_as_interleaved(frame: &frame::Audio, channels: usize, out: &mut Vec<f32>) {
    let samples = frame.samples();
    if samples == 0 {
        return;
    }
    match frame.format() {
        Sample::F32(ffmpeg::format::sample::Type::Packed) => {
            let bytes = frame.data(0);
            // SAFETY: packed f32 — bytes is `samples * channels * 4` long.
            // AVFrame buffers honor AV_INPUT_BUFFER alignment.
            #[allow(clippy::cast_ptr_alignment)]
            let interleaved = unsafe {
                std::slice::from_raw_parts(
                    bytes.as_ptr().cast::<f32>(),
                    samples * channels,
                )
            };
            out.extend_from_slice(interleaved);
        }
        Sample::F32(ffmpeg::format::sample::Type::Planar) => {
            out.reserve(samples * channels);
            let planes: Vec<&[f32]> = (0..channels)
                .map(|c| {
                    let bytes = frame.data(c);
                    // SAFETY: planar f32 — bytes is `samples * 4` long.
                    #[allow(clippy::cast_ptr_alignment)]
                    unsafe {
                        std::slice::from_raw_parts(bytes.as_ptr().cast::<f32>(), samples)
                    }
                })
                .collect();
            for s in 0..samples {
                for plane in &planes {
                    out.push(plane[s]);
                }
            }
        }
        Sample::I16(ffmpeg::format::sample::Type::Packed) => {
            let bytes = frame.data(0);
            // SAFETY: packed i16 — bytes is `samples * channels * 2` long.
            #[allow(clippy::cast_ptr_alignment)]
            let interleaved = unsafe {
                std::slice::from_raw_parts(
                    bytes.as_ptr().cast::<i16>(),
                    samples * channels,
                )
            };
            out.reserve(interleaved.len());
            for &s in interleaved {
                out.push(f32::from(s) * I16_TO_F32_SCALE);
            }
        }
        Sample::I16(ffmpeg::format::sample::Type::Planar) => {
            out.reserve(samples * channels);
            let planes: Vec<&[i16]> = (0..channels)
                .map(|c| {
                    let bytes = frame.data(c);
                    // SAFETY: planar i16 — bytes is `samples * 2` long.
                    #[allow(clippy::cast_ptr_alignment)]
                    unsafe {
                        std::slice::from_raw_parts(bytes.as_ptr().cast::<i16>(), samples)
                    }
                })
                .collect();
            for s in 0..samples {
                for plane in &planes {
                    out.push(f32::from(plane[s]) * I16_TO_F32_SCALE);
                }
            }
        }
        _ => {
            let fmt = frame.format();
            tracing::warn!(?fmt, "unexpected Opus decoded format; skipping");
        }
    }
}
