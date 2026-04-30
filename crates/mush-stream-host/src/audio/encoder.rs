//! Opus encoder via ffmpeg-the-third (`libopus`). Reuses the ffmpeg
//! we're already linked against — no new system DLL.

use ffmpeg_the_third as ffmpeg;
use ffmpeg::{
    Dictionary, Packet,
    codec,
    format::Sample,
    frame,
};
use ffmpeg::util::channel_layout::{ChannelLayout, ChannelLayoutMask};
use thiserror::Error;

use super::FRAME_SAMPLES;

#[derive(Debug, Error)]
pub enum EncoderError {
    #[error("could not find encoder `libopus` — ensure ffmpeg was built with libopus support")]
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
    fn ff(self, context: &'static str) -> Result<T, EncoderError>;
}
impl<T> FfErrCtx<T> for Result<T, ffmpeg::Error> {
    fn ff(self, context: &'static str) -> Result<T, EncoderError> {
        self.map_err(|source| EncoderError::Ffmpeg { context, source })
    }
}

pub struct OpusEncoder {
    encoder: codec::encoder::audio::Encoder,
    frame: frame::Audio,
    packet: Packet,
    channels: u16,
    pts: i64,
}

impl OpusEncoder {
    /// Build a `libopus` encoder configured for 48 kHz / `channels` /
    /// `bitrate_bps` CBR. Input format: f32 planar (Opus's preferred).
    /// Frames are 20 ms (`FRAME_SAMPLES` per channel).
    pub fn new(sample_rate: u32, channels: u16, bitrate_bps: u32) -> Result<Self, EncoderError> {
        ffmpeg::init().map_err(EncoderError::Init)?;
        let codec = codec::encoder::find_by_name("libopus").ok_or(EncoderError::OpusNotFound)?;

        let layout_mask = match channels {
            1 => ChannelLayoutMask::MONO,
            _ => ChannelLayoutMask::STEREO,
        };

        let mut enc_ctx = codec::Context::new_with_codec(codec)
            .encoder()
            .audio()
            .ff("encoder().audio()")?;
        let sample_rate_i32 = i32::try_from(sample_rate).unwrap_or(48_000);
        enc_ctx.set_rate(sample_rate_i32);
        enc_ctx.set_format(Sample::F32(ffmpeg::format::sample::Type::Planar));
        // ffmpeg-the-third 5 / ffmpeg 8 prefer the new ChannelLayout API
        // (set_ch_layout) over the deprecated mask-based set_channels +
        // set_channel_layout.
        enc_ctx.set_ch_layout(if channels == 1 {
            ChannelLayout::MONO
        } else {
            ChannelLayout::STEREO
        });
        enc_ctx.set_bit_rate(bitrate_bps as usize);
        enc_ctx.set_time_base(ffmpeg::Rational(1, sample_rate_i32));

        let mut opts = Dictionary::new();
        // Application "audio" (vs "voip"): preserves music/sfx fidelity
        // for game streaming. Lowdelay tuning trades a tiny bit of
        // efficiency for tighter encode/decode timing — what we want.
        opts.set("application", "audio");
        opts.set("frame_duration", "20");
        opts.set("vbr", "off"); // CBR, predictable bandwidth

        let encoder = enc_ctx.open_with(opts).ff("open_with(libopus opts)")?;

        let frame = frame::Audio::new(
            Sample::F32(ffmpeg::format::sample::Type::Planar),
            FRAME_SAMPLES,
            layout_mask,
        );

        Ok(Self {
            encoder,
            frame,
            packet: Packet::empty(),
            channels,
            pts: 0,
        })
    }

    /// Encode `pcm` (interleaved f32 stereo, length = `FRAME_SAMPLES *
    /// channels`) into a single Opus packet. Returns the packet's bytes.
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<u8>, EncoderError> {
        debug_assert_eq!(pcm.len(), FRAME_SAMPLES * self.channels as usize);

        // libopus encoder expects PLANAR f32: one slab of L samples
        // followed by one slab of R samples. Deinterleave from the
        // capture buffer into the AVFrame's per-channel planes.
        for ch in 0..self.channels as usize {
            let plane = self.frame.data_mut(ch);
            // SAFETY: AVFrame planar buffers are AV_INPUT_BUFFER alignment
            // (32 bytes) — well past f32's 4-byte alignment requirement.
            #[allow(clippy::cast_ptr_alignment)]
            let plane_f32 = unsafe {
                std::slice::from_raw_parts_mut(plane.as_mut_ptr().cast::<f32>(), FRAME_SAMPLES)
            };
            for s in 0..FRAME_SAMPLES {
                plane_f32[s] = pcm[s * self.channels as usize + ch];
            }
        }
        self.frame.set_pts(Some(self.pts));
        // FRAME_SAMPLES = 960; comfortably within i64 — no wrap risk.
        #[allow(clippy::cast_possible_wrap)]
        let frame_samples_i64 = FRAME_SAMPLES as i64;
        self.pts += frame_samples_i64;

        self.encoder.send_frame(&self.frame).ff("send_frame")?;

        // libopus emits exactly one packet per 20 ms input frame.
        match self.encoder.receive_packet(&mut self.packet) {
            Ok(()) => {
                let data = self.packet.data().unwrap_or(&[]);
                Ok(data.to_vec())
            }
            Err(ffmpeg::Error::Eof | ffmpeg::Error::Other { .. }) => Ok(Vec::new()),
            Err(e) => Err(EncoderError::Ffmpeg {
                context: "receive_packet",
                source: e,
            }),
        }
    }
}
