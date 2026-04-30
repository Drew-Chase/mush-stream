//! H.264 decoding via `ffmpeg-the-third`.
//!
//! Tries `h264_cuvid` (NVIDIA hardware decode) first when configured, falls
//! back to the bundled software h264 decoder. Output is upscaled / colour-
//! converted to packed RGBA via libswscale so the display layer can blit
//! straight into a `pixels` framebuffer without per-pixel work.

use ffmpeg_the_third as ffmpeg;
use ffmpeg::{
    Packet,
    codec::{self, Id},
    format::Pixel,
    frame,
    software::scaling,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("could not find any H.264 decoder (looked for h264_cuvid + sw h264)")]
    NoDecoder,
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
    fn ff(self, context: &'static str) -> Result<T, DecodeError>;
}

impl<T> FfErrCtx<T> for Result<T, ffmpeg::Error> {
    fn ff(self, context: &'static str) -> Result<T, DecodeError> {
        self.map_err(|source| DecodeError::Ffmpeg { context, source })
    }
}

/// One decoded RGBA frame, ready to blit into a `pixels` framebuffer.
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    /// Tightly-packed RGBA, length = `width * height * 4`.
    pub rgba: Vec<u8>,
    /// `Instant` at which the *first* packet of this frame's NAL arrived
    /// at the client. Used for client-side lag measurement: present-time
    /// minus this gives the network-arrival → display latency, in a way
    /// that doesn't require synchronized clocks across machines.
    pub first_packet_instant: std::time::Instant,
    /// Size in bytes of the encoded NAL the decoder consumed to produce
    /// this frame. The display thread uses these to compute a rolling
    /// bitrate for the debug overlay.
    pub encoded_bytes: usize,
}

impl std::fmt::Debug for DecodedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Elide the rgba bytes — at 2560×1440 they're ~14 MB and would
        // flood any logger that printed the enclosing UserEvent.
        f.debug_struct("DecodedFrame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("rgba_len", &self.rgba.len())
            .field("first_packet_instant", &self.first_packet_instant)
            .field("encoded_bytes", &self.encoded_bytes)
            .finish()
    }
}

/// H.264 decoder. Internally lazy: the swscale converter is built on the
/// first decoded frame because the stream's actual width/height/pixel format
/// aren't known until the first IDR is decoded.
pub struct VideoDecoder {
    decoder: codec::decoder::Video,
    scaler: Option<scaling::Context>,
    /// Display target size; swscale will scale-and-convert decoded frames
    /// to fit. Set from client config.
    dst_width: u32,
    dst_height: u32,
    /// Reusable destination frame for swscale output.
    rgba_frame: frame::Video,
    /// Accelerator name actually opened (e.g. "h264_cuvid" or "h264").
    backend: &'static str,
}

impl VideoDecoder {
    /// Try to open `h264_cuvid` if `prefer_hardware`, otherwise (or on
    /// failure) the software h264 decoder.
    pub fn new(
        prefer_hardware: bool,
        dst_width: u32,
        dst_height: u32,
    ) -> Result<Self, DecodeError> {
        ffmpeg::init().map_err(DecodeError::Init)?;

        // Build the open-decoder candidates list, hardware first when asked.
        let mut tried: Vec<(&'static str, Option<codec::Codec>)> = Vec::new();
        if prefer_hardware {
            tried.push(("h264_cuvid", codec::decoder::find_by_name("h264_cuvid")));
        }
        tried.push(("h264", codec::decoder::find(Id::H264)));

        let mut last_err: Option<ffmpeg::Error> = None;
        for (name, codec_opt) in tried {
            let Some(codec) = codec_opt else {
                tracing::debug!(decoder = name, "decoder not found in this ffmpeg build");
                continue;
            };
            match codec::Context::new_with_codec(codec)
                .decoder()
                .open_as(codec)
                .and_then(codec::decoder::Opened::video)
            {
                Ok(decoder) => {
                    tracing::info!(backend = name, "video decoder opened");
                    let rgba_frame = frame::Video::new(Pixel::RGBA, dst_width, dst_height);
                    return Ok(Self {
                        decoder,
                        scaler: None,
                        dst_width,
                        dst_height,
                        rgba_frame,
                        backend: name,
                    });
                }
                Err(e) => {
                    tracing::warn!(decoder = name, error = %e, "decoder failed to open; trying next");
                    last_err = Some(e);
                }
            }
        }
        if let Some(e) = last_err {
            return Err(DecodeError::Ffmpeg {
                context: "open decoder",
                source: e,
            });
        }
        Err(DecodeError::NoDecoder)
    }

    pub fn backend(&self) -> &'static str {
        self.backend
    }

    /// Push one reassembled NAL unit (Annex-B with inline SPS/PPS for IDR).
    /// `first_packet_instant` is the local `Instant` at which the first
    /// packet of this NAL arrived at the client; the resulting
    /// [`DecodedFrame`] propagates it to the display layer for lag
    /// measurement.
    ///
    /// May yield 0 or more decoded frames (SPS-only inputs yield nothing;
    /// at startup the decoder buffers a few frames before producing
    /// output).
    pub fn push_nal<F>(
        &mut self,
        nal: &[u8],
        first_packet_instant: std::time::Instant,
        mut on_frame: F,
    ) -> Result<(), DecodeError>
    where
        F: FnMut(DecodedFrame),
    {
        let encoded_bytes = nal.len();
        let packet = Packet::copy(nal);
        self.decoder.send_packet(&packet).ff("send_packet")?;

        let mut decoded = frame::Video::empty();
        loop {
            match self.decoder.receive_frame(&mut decoded) {
                Ok(()) => {
                    let rgba = self.scale_to_rgba(&decoded)?;
                    on_frame(DecodedFrame {
                        width: self.dst_width,
                        height: self.dst_height,
                        rgba,
                        first_packet_instant,
                        encoded_bytes,
                    });
                }
                // EAGAIN ("send more packets") and EOF (drained on shutdown)
                // both end the receive loop.
                Err(ffmpeg::Error::Eof | ffmpeg::Error::Other { .. }) => break,
                Err(e) => {
                    return Err(DecodeError::Ffmpeg {
                        context: "receive_frame",
                        source: e,
                    });
                }
            }
        }
        Ok(())
    }

    fn scale_to_rgba(&mut self, decoded: &frame::Video) -> Result<Vec<u8>, DecodeError> {
        let src_format = decoded.format();
        let src_width = decoded.width();
        let src_height = decoded.height();

        // (Re)build the scaler if the stream's source params changed (or
        // it's the first frame).
        let needs_rebuild = self.scaler.as_ref().is_none_or(|s| {
            s.input().format != src_format
                || s.input().width != src_width
                || s.input().height != src_height
        });
        if needs_rebuild {
            self.scaler = Some(
                scaling::Context::get(
                    src_format,
                    src_width,
                    src_height,
                    Pixel::RGBA,
                    self.dst_width,
                    self.dst_height,
                    scaling::Flags::BILINEAR,
                )
                .ff("scaling::Context::get")?,
            );
            tracing::debug!(
                src_w = src_width,
                src_h = src_height,
                ?src_format,
                dst_w = self.dst_width,
                dst_h = self.dst_height,
                "swscale (re)initialized"
            );
        }
        let scaler = self.scaler.as_mut().expect("scaler must be Some");
        scaler
            .run(decoded, &mut self.rgba_frame)
            .ff("scaling run")?;

        // Pack the destination RGBA into a tightly-packed Vec, stripping
        // any line-pitch padding swscale may have emitted.
        let stride = self.rgba_frame.stride(0);
        let row_bytes = (self.dst_width as usize) * 4;
        let height = self.dst_height as usize;
        let mut out = Vec::with_capacity(row_bytes * height);
        let src = self.rgba_frame.data(0);
        for row in 0..height {
            let off = row * stride;
            out.extend_from_slice(&src[off..off + row_bytes]);
        }
        Ok(out)
    }
}
