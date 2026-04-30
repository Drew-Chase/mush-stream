//! NVENC H.264 encoding via `ffmpeg-the-third`.
//!
//! Two consumers:
//! - [`VideoEncoder`] is the low-level pipe — BGRA in, encoded packets out
//!   via a caller-supplied closure. Used by the M4 streaming path.
//! - [`Mp4Recorder`] wraps a `VideoEncoder` together with an MP4 muxer for
//!   the M2 capture-to-file verification mode.
//!
//! NVENC accepts packed BGRA input directly (`AV_PIX_FMT_BGR0`) and converts
//! to NV12 internally on the GPU. The CPU-side BGRA round-trip from the
//! capture stage will be lifted to D3D11 hwframes in M5/M7 once latency is
//! the focus.

use std::path::Path;

use ffmpeg_the_third as ffmpeg;
use ffmpeg::{
    Dictionary, Packet, Rational,
    codec::{self, Flags as CodecFlags},
    format::{self, Pixel},
    frame,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EncodeError {
    #[error(
        "could not find encoder `h264_nvenc` — ensure ffmpeg was built with NVENC support \
        and an NVIDIA GPU driver is installed"
    )]
    NvencNotFound,
    #[error(
        "BGRA frame size mismatch: expected {expected} bytes ({width}x{height}*4), got {got}"
    )]
    BgraSize {
        expected: usize,
        got: usize,
        width: u32,
        height: u32,
    },
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
    fn ff(self, context: &'static str) -> Result<T, EncodeError>;
}

impl<T> FfErrCtx<T> for Result<T, ffmpeg::Error> {
    fn ff(self, context: &'static str) -> Result<T, EncodeError> {
        self.map_err(|source| EncodeError::Ffmpeg { context, source })
    }
}

/// Low-level NVENC encoder. Takes BGRA frames in, emits encoded
/// packets via a closure on each `push_bgra` call.
pub struct VideoEncoder {
    encoder: codec::encoder::video::Encoder,
    encoder_time_base: Rational,
    frame: frame::Video,
    packet: Packet,
    width: u32,
    height: u32,
    /// Set by [`Self::request_keyframe`]; consumed and cleared by the next
    /// `push_bgra` to mark the input frame as an I-frame so NVENC emits an
    /// IDR. Used by M7's keyframe-on-loss recovery.
    force_keyframe_next: bool,
}

impl VideoEncoder {
    /// Create and open a fresh NVENC encoder.
    ///
    /// `global_header = true` causes SPS/PPS to live in the codec context's
    /// `extradata` (required for MP4 muxing). For UDP streaming, leave this
    /// `false` so SPS/PPS are emitted inline at every IDR — the client's
    /// decoder can then bootstrap from any keyframe without needing to
    /// receive a separate "extradata" channel.
    pub fn new(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        global_header: bool,
    ) -> Result<Self, EncodeError> {
        ffmpeg::init().map_err(EncodeError::Init)?;

        let codec = codec::encoder::find_by_name("h264_nvenc")
            .ok_or(EncodeError::NvencNotFound)?;

        let fps_i32 = i32::try_from(fps).unwrap_or(i32::MAX);
        let encoder_tb = Rational(1, fps_i32);

        let mut enc_ctx = codec::Context::new_with_codec(codec)
            .encoder()
            .video()
            .ff("encoder().video()")?;
        enc_ctx.set_width(width);
        enc_ctx.set_height(height);
        enc_ctx.set_time_base(encoder_tb);
        enc_ctx.set_frame_rate(Some(Rational(fps_i32, 1)));
        enc_ctx.set_format(Pixel::BGRZ);
        enc_ctx.set_bit_rate(bitrate_bps as usize);
        enc_ctx.set_max_bit_rate(bitrate_bps as usize);
        enc_ctx.set_gop(fps); // 1-second keyframe interval
        enc_ctx.set_max_b_frames(0);
        if global_header {
            enc_ctx.set_flags(CodecFlags::GLOBAL_HEADER);
        }

        let mut opts = Dictionary::new();
        // Tuned for "9 Mbps at 60fps shouldn't look like minecraft":
        // - preset p4 (balanced) instead of p1 (fastest). NVENC on any
        //   RTX-class card handles 1440p60 at p4 well within frame
        //   budget. p1 was throwing away too much detail at low bitrates,
        //   especially in motion.
        // - tune=ll keeps the low-latency path (no B-frames, no
        //   reordering). zerolatency=1 also OFF the lookahead.
        // - rc=cbr with delay=0, rc-lookahead=0, no-scenecut=1 — same
        //   stream-shape constraints as before.
        // - multipass=qres adds a quarter-resolution analysis pass for
        //   smarter QP allocation at modest GPU cost; almost no latency
        //   impact in practice.
        // - spatial-aq=1 / aq-strength=8 distribute bits toward visually
        //   important regions (faces, edges, smooth gradients) — big
        //   visible quality bump at low bitrates.
        // - profile=high gives the encoder access to the H.264 features
        //   that compress efficiently. cuvid + sw h264 both decode high.
        opts.set("preset", "p4");
        opts.set("tune", "ll");
        opts.set("zerolatency", "1");
        opts.set("rc", "cbr");
        opts.set("delay", "0");
        opts.set("rc-lookahead", "0");
        opts.set("no-scenecut", "1");
        opts.set("multipass", "qres");
        opts.set("spatial-aq", "1");
        opts.set("aq-strength", "8");
        opts.set("profile", "high");

        let encoder = enc_ctx.open_with(opts).ff("open_with(NVENC opts)")?;

        let mut frame = frame::Video::new(Pixel::BGRZ, width, height);
        frame.set_pts(Some(0));

        Ok(Self {
            encoder,
            encoder_time_base: encoder_tb,
            frame,
            packet: Packet::empty(),
            width,
            height,
            force_keyframe_next: false,
        })
    }

    pub fn time_base(&self) -> Rational {
        self.encoder_time_base
    }

    /// Mark the next encoded frame as an IDR. The encoder will emit
    /// SPS/PPS plus an I-frame on the very next `push_bgra` regardless
    /// of GOP timing. Used by M7 to recover from packet loss without
    /// waiting for the next scheduled keyframe.
    pub fn request_keyframe(&mut self) {
        self.force_keyframe_next = true;
    }

    /// Encode one BGRA frame at the given pts (in encoder time base, i.e.
    /// frame index when the time base is `1/fps`). For each encoded packet
    /// produced, invokes `on_packet` with `&mut Packet`. The caller may read
    /// the packet bytes (`packet.data()`) for streaming, or mutate
    /// `stream_index`/`pts`/`dts` and call `write_interleaved` for muxing.
    pub fn push_bgra<F>(
        &mut self,
        bgra: &[u8],
        pts: i64,
        on_packet: F,
    ) -> Result<(), EncodeError>
    where
        F: FnMut(&mut Packet) -> Result<(), EncodeError>,
    {
        let expected = (self.width as usize) * (self.height as usize) * 4;
        if bgra.len() != expected {
            return Err(EncodeError::BgraSize {
                expected,
                got: bgra.len(),
                width: self.width,
                height: self.height,
            });
        }

        // Copy BGRA into the AVFrame respecting any line-pitch padding.
        let row_bytes = (self.width as usize) * 4;
        let stride = self.frame.stride(0);
        let height = self.height as usize;
        let dst = self.frame.data_mut(0);
        for row in 0..height {
            let src_row = &bgra[row * row_bytes..(row + 1) * row_bytes];
            let dst_off = row * stride;
            dst[dst_off..dst_off + row_bytes].copy_from_slice(src_row);
        }

        self.frame.set_pts(Some(pts));

        // Forced-keyframe path for M7. ffmpeg-the-third's `frame::Video`
        // doesn't expose `set_pict_type` directly, so we poke the AVFrame
        // through the raw pointer. SAFETY: we hold `&mut self.frame` and
        // the pointer is valid for the lifetime of the frame.
        //
        // ffmpeg 8 removed the deprecated `AVFrame::key_frame` field; the
        // replacement is the `AV_FRAME_FLAG_KEY` bit on `AVFrame::flags`.
        // The encoder honours pict_type=I to force an IDR; the flag bit is
        // for symmetry on the read side.
        if self.force_keyframe_next {
            unsafe {
                let raw = self.frame.as_mut_ptr();
                (*raw).pict_type = ffmpeg::ffi::AVPictureType::I;
                (*raw).flags |= ffmpeg::ffi::AV_FRAME_FLAG_KEY;
            }
            self.force_keyframe_next = false;
        } else {
            // Reset to NONE so the encoder is free to choose.
            unsafe {
                let raw = self.frame.as_mut_ptr();
                (*raw).pict_type = ffmpeg::ffi::AVPictureType::NONE;
                (*raw).flags &= !ffmpeg::ffi::AV_FRAME_FLAG_KEY;
            }
        }

        self.encoder.send_frame(&self.frame).ff("send_frame")?;
        self.drain_packets(on_packet)
    }

    /// Flush pending packets after `send_eof`. Call exactly once at the end
    /// of the stream; the encoder is left in a closed state.
    pub fn finish<F>(&mut self, on_packet: F) -> Result<(), EncodeError>
    where
        F: FnMut(&mut Packet) -> Result<(), EncodeError>,
    {
        self.encoder.send_eof().ff("send_eof")?;
        self.drain_packets(on_packet)
    }

    fn drain_packets<F>(&mut self, mut on_packet: F) -> Result<(), EncodeError>
    where
        F: FnMut(&mut Packet) -> Result<(), EncodeError>,
    {
        loop {
            match self.encoder.receive_packet(&mut self.packet) {
                Ok(()) => on_packet(&mut self.packet)?,
                // EAGAIN ("send more frames") and EOF (end of drain) both
                // end the receive loop; ffmpeg's contract on receive_packet
                // reserves Other{} for those.
                Err(ffmpeg::Error::Eof | ffmpeg::Error::Other { .. }) => break,
                Err(e) => {
                    return Err(EncodeError::Ffmpeg {
                        context: "receive_packet",
                        source: e,
                    });
                }
            }
        }
        Ok(())
    }
}

/// MP4-recording wrapper used by `--mp4` mode. Owns a [`VideoEncoder`] plus
/// an MP4 output muxer.
pub struct Mp4Recorder {
    encoder: VideoEncoder,
    output: format::context::Output,
    stream_index: usize,
    stream_time_base: Rational,
}

impl Mp4Recorder {
    pub fn new(
        path: &Path,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
    ) -> Result<Self, EncodeError> {
        ffmpeg::init().map_err(EncodeError::Init)?;

        // Need to know whether the muxer wants global_header before we
        // configure the encoder. Open the output first.
        let mut output = format::output(path).ff("format::output")?;
        let global_header = output
            .format()
            .flags()
            .contains(format::flag::Flags::GLOBAL_HEADER);

        let encoder = VideoEncoder::new(width, height, fps, bitrate_bps, global_header)?;
        let encoder_tb = encoder.time_base();

        // Add the stream after the encoder is built (need codec ptr from
        // find_by_name; available via ffmpeg's global registry).
        let codec_unknown = codec::encoder::find_by_name("h264_nvenc")
            .ok_or(EncodeError::NvencNotFound)?;
        let stream_index = {
            let mut stream = output.add_stream(codec_unknown).ff("add_stream")?;
            stream.set_time_base(encoder_tb);
            stream.index()
        };

        // Copy SPS/PPS extradata from the opened encoder into the stream.
        {
            let mut stream = output
                .stream_mut(stream_index)
                .expect("stream just added must be retrievable by index");
            stream.copy_parameters_from_context(&encoder.encoder);
        }

        output.write_header().ff("write_header")?;

        let stream_time_base = output
            .stream(stream_index)
            .expect("stream still present after write_header")
            .time_base();

        Ok(Self {
            encoder,
            output,
            stream_index,
            stream_time_base,
        })
    }

    pub fn push_bgra(&mut self, bgra: &[u8], pts: i64) -> Result<(), EncodeError> {
        let stream_index = self.stream_index;
        let encoder_tb = self.encoder.encoder_time_base;
        let stream_tb = self.stream_time_base;
        let output = &mut self.output;
        self.encoder.push_bgra(bgra, pts, |packet| {
            packet.set_stream(stream_index);
            packet.rescale_ts(encoder_tb, stream_tb);
            packet
                .write_interleaved(output)
                .ff("write_interleaved")
        })
    }

    pub fn finish(mut self) -> Result<(), EncodeError> {
        let stream_index = self.stream_index;
        let encoder_tb = self.encoder.encoder_time_base;
        let stream_tb = self.stream_time_base;
        let output = &mut self.output;
        self.encoder.finish(|packet| {
            packet.set_stream(stream_index);
            packet.rescale_ts(encoder_tb, stream_tb);
            packet
                .write_interleaved(output)
                .ff("write_interleaved")
        })?;
        self.output.write_trailer().ff("write_trailer")?;
        Ok(())
    }
}
