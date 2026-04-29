//! NVENC H.264 encoding + MP4 muxing via `ffmpeg-the-third`.
//!
//! Milestone 2: takes BGRA frames from the capture stage, hands them to
//! `h264_nvenc` (which accepts packed BGRA input directly and converts to NV12
//! internally), and writes the result to an MP4 container.
//!
//! The CPU-readback intermediate (BGRA bytes) is the M2 trade-off so we don't
//! have to write a BGRA→NV12 D3D11 shader before having a working pipeline.
//! M5/M7 will revisit GPU-resident path once we're optimizing latency.

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
        "h264_nvenc found but it does not advertise itself as a video encoder \
        (this would indicate a broken ffmpeg build)"
    )]
    NotVideoEncoder,
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

/// Encodes BGRA frames to an MP4 file via h264_nvenc.
pub struct VideoRecorder {
    output: format::context::Output,
    encoder: codec::encoder::video::Encoder,
    encoder_time_base: Rational,
    stream_time_base: Rational,
    stream_index: usize,
    frame: frame::Video,
    packet: Packet,
    width: u32,
    height: u32,
}

impl VideoRecorder {
    /// Opens an MP4 at `path`, configures NVENC per the project spec
    /// (preset p1, tune ll, zerolatency, no B-frames), writes the format
    /// header, and returns a recorder ready to accept frames.
    pub fn new(
        path: &Path,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
    ) -> Result<Self, EncodeError> {
        ffmpeg::init().map_err(EncodeError::Init)?;

        let codec = codec::encoder::find_by_name("h264_nvenc")
            .ok_or(EncodeError::NvencNotFound)?;

        let mut output = format::output(&path).ff("format::output")?;
        let encoder_tb = Rational(1, fps as i32);

        // Add the stream and remember its index; the StreamMut borrows the
        // output, so we drop it before configuring the encoder.
        let stream_index = {
            let mut stream = output.add_stream(codec).ff("add_stream")?;
            stream.set_time_base(encoder_tb);
            stream.index()
        };

        // Whether the muxer wants codec extradata in the global header
        // (true for MP4) — the encoder must emit SPS/PPS out-of-band.
        let global_header = output
            .format()
            .flags()
            .contains(format::flag::Flags::GLOBAL_HEADER);

        // Build, configure, and open the encoder.
        let mut enc_ctx = codec::Context::new_with_codec(codec)
            .encoder()
            .video()
            .ff("encoder().video()")?;
        enc_ctx.set_width(width);
        enc_ctx.set_height(height);
        enc_ctx.set_time_base(encoder_tb);
        enc_ctx.set_frame_rate(Some(Rational(fps as i32, 1)));
        enc_ctx.set_format(Pixel::BGR0);
        enc_ctx.set_bit_rate(bitrate_bps as usize);
        enc_ctx.set_max_bit_rate(bitrate_bps as usize);
        enc_ctx.set_gop(fps); // 1-second keyframe interval
        enc_ctx.set_max_b_frames(0);
        if global_header {
            enc_ctx.set_flags(CodecFlags::GLOBAL_HEADER);
        }

        let mut opts = Dictionary::new();
        opts.set("preset", "p1");
        opts.set("tune", "ll");
        opts.set("zerolatency", "1");

        let encoder = enc_ctx.open_with(opts).ff("open_with(NVENC opts)")?;

        // Copy params (codec id, extradata, dimensions, etc.) into the stream.
        {
            let mut stream = output
                .stream_mut(stream_index)
                .expect("stream just added must be retrievable by index");
            stream.copy_parameters_from_context(&encoder);
        }

        output.write_header().ff("write_header")?;

        // After write_header the muxer may have rewritten the stream's time
        // base (MP4 typically uses 1/timescale, not 1/fps). Read what stuck.
        let stream_time_base = output
            .stream(stream_index)
            .expect("stream still present after write_header")
            .time_base();

        let mut frame = frame::Video::new(Pixel::BGR0, width, height);
        // Initial pts; per-frame push will overwrite.
        frame.set_pts(Some(0));

        Ok(Self {
            output,
            encoder,
            encoder_time_base: encoder_tb,
            stream_time_base,
            stream_index,
            frame,
            packet: Packet::empty(),
            width,
            height,
        })
    }

    /// Pushes one BGRA frame at the given pts (in encoder time base, i.e.
    /// frame number when the time base is `1/fps`). Drains any packets the
    /// encoder produces.
    pub fn push_bgra(&mut self, bgra: &[u8], pts: i64) -> Result<(), EncodeError> {
        let expected = (self.width as usize) * (self.height as usize) * 4;
        if bgra.len() != expected {
            return Err(EncodeError::BgraSize {
                expected,
                got: bgra.len(),
                width: self.width,
                height: self.height,
            });
        }

        let row_bytes = (self.width as usize) * 4;
        let stride = self.frame.stride(0);
        let height = self.height as usize;
        let dst = self.frame.data_mut(0);
        // ffmpeg's frame buffer may pad rows past `width*4`; copy row-by-row
        // so we always honor `linesize[0]`.
        for row in 0..height {
            let src_row = &bgra[row * row_bytes..(row + 1) * row_bytes];
            let dst_off = row * stride;
            dst[dst_off..dst_off + row_bytes].copy_from_slice(src_row);
        }

        self.frame.set_pts(Some(pts));
        self.encoder.send_frame(&self.frame).ff("send_frame")?;
        self.drain_packets()?;
        Ok(())
    }

    fn drain_packets(&mut self) -> Result<(), EncodeError> {
        loop {
            match self.encoder.receive_packet(&mut self.packet) {
                Ok(()) => {
                    self.packet.set_stream(self.stream_index);
                    self.packet
                        .rescale_ts(self.encoder_time_base, self.stream_time_base);
                    self.packet
                        .write_interleaved(&mut self.output)
                        .ff("write_interleaved")?;
                }
                // EAGAIN ("send more frames") and EOF (end of drain) both end
                // the receive loop. Any other Other{} we treat the same way:
                // ffmpeg's receive_packet contract reserves Other for those.
                Err(ffmpeg::Error::Eof) => break,
                Err(ffmpeg::Error::Other { .. }) => break,
                Err(e) => return Err(EncodeError::Ffmpeg { context: "receive_packet", source: e }),
            }
        }
        Ok(())
    }

    /// Flushes the encoder, drains remaining packets, writes the trailer,
    /// and closes the file. Consumes `self`.
    pub fn finish(mut self) -> Result<(), EncodeError> {
        self.encoder.send_eof().ff("send_eof")?;
        self.drain_packets()?;
        self.output.write_trailer().ff("write_trailer")?;
        Ok(())
    }
}
