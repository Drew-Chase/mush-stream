//! H.264 decoding via `ffmpeg-the-third`.
//!
//! Tries `h264_cuvid` (NVIDIA hardware decode) first when configured, falls
//! back to the bundled software h264 decoder. Output is colour-converted
//! to packed RGBA via libswscale at the *source* resolution — no scaling
//! happens here. Window fitting (and the corresponding letterbox /
//! pillarbox) is the display layer's job, since it's the only side that
//! knows the current window size and can preserve aspect ratio across
//! resizes.

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

/// H.264 decoder. Internally lazy: the swscale converter and its
/// destination frame are built on the first decoded frame because the
/// stream's actual width/height/pixel format aren't known until the
/// first IDR is decoded.
pub struct VideoDecoder {
    decoder: codec::decoder::Video,
    scaler: Option<scaling::Context>,
    /// Reusable destination frame for swscale output. `None` until the
    /// first frame arrives; thereafter sized to the source's dimensions
    /// and re-allocated only if the source resolution changes.
    rgba_frame: Option<frame::Video>,
    /// Accelerator name actually opened (e.g. "h264_cuvid" or "h264").
    backend: &'static str,
}

impl VideoDecoder {
    /// Try to open `h264_cuvid` if `prefer_hardware`, otherwise (or on
    /// failure) the software h264 decoder.
    pub fn new(prefer_hardware: bool) -> Result<Self, DecodeError> {
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
                    return Ok(Self {
                        decoder,
                        scaler: None,
                        rgba_frame: None,
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
    /// Decode `nal` purely to advance the reference chain. The decoded
    /// frame is *not* colour-converted or yielded to a caller — the
    /// expensive [`Self::scale_to_rgba`] step is skipped entirely.
    /// Returns the encoded byte count consumed so callers can keep
    /// bitrate accounting accurate when fast-forwarding through a
    /// backlog.
    pub fn decode_without_present(&mut self, nal: &[u8]) -> Result<usize, DecodeError> {
        let bytes = nal.len();
        let packet = Packet::copy(nal);
        self.decoder.send_packet(&packet).ff("send_packet")?;
        let mut decoded = frame::Video::empty();
        loop {
            match self.decoder.receive_frame(&mut decoded) {
                Ok(()) => { /* discard; we only want reference state */ }
                Err(ffmpeg::Error::Eof | ffmpeg::Error::Other { .. }) => break,
                Err(e) => {
                    return Err(DecodeError::Ffmpeg {
                        context: "receive_frame",
                        source: e,
                    });
                }
            }
        }
        Ok(bytes)
    }

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
                    let (width, height, rgba) = self.scale_to_rgba(&decoded)?;
                    on_frame(DecodedFrame {
                        width,
                        height,
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

    /// Convert the decoded frame to packed RGBA at the *source*
    /// resolution. Returns `(width, height, rgba)` so the display
    /// layer can size its framebuffer to the source's aspect ratio.
    fn scale_to_rgba(
        &mut self,
        decoded: &frame::Video,
    ) -> Result<(u32, u32, Vec<u8>), DecodeError> {
        let src_format = decoded.format();
        let src_width = decoded.width();
        let src_height = decoded.height();

        // (Re)build the scaler + destination frame if the stream's
        // source params changed (or it's the first frame). We scale to
        // the same dimensions — swscale here is just doing the colour
        // conversion (typically YUV → RGBA); resizing is the display
        // layer's responsibility.
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
                    src_width,
                    src_height,
                    scaling::Flags::BILINEAR,
                )
                .ff("scaling::Context::get")?,
            );
            self.rgba_frame =
                Some(frame::Video::new(Pixel::RGBA, src_width, src_height));
            tracing::debug!(
                src_w = src_width,
                src_h = src_height,
                ?src_format,
                "swscale (re)initialized at source resolution"
            );
        }
        let scaler = self.scaler.as_mut().expect("scaler must be Some");
        let rgba_frame = self
            .rgba_frame
            .as_mut()
            .expect("rgba_frame must be Some after rebuild");
        scaler.run(decoded, rgba_frame).ff("scaling run")?;

        // Pack the destination RGBA into a tightly-packed Vec, stripping
        // any line-pitch padding swscale may have emitted.
        let stride = rgba_frame.stride(0);
        let row_bytes = (src_width as usize) * 4;
        let height = src_height as usize;
        let mut out = Vec::with_capacity(row_bytes * height);
        let src = rgba_frame.data(0);
        for row in 0..height {
            let off = row * stride;
            out.extend_from_slice(&src[off..off + row_bytes]);
        }
        Ok((src_width, src_height, out))
    }
}
