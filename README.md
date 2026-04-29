# mush-stream

Low-latency Windows-to-Windows desktop streaming for two-player split-screen
gaming. The host captures a configurable rectangular region of its desktop,
hardware-encodes it via NVENC, and streams it over UDP to a remote client. The
client decodes and displays the video, captures the friend's gamepad, and sends
input back to the host where it is injected as a virtual Xbox 360 controller.

Targets Windows 10/11 x86_64 only.

## What works

### Milestone 1 — capture-to-PNG

- Cargo workspace with three crates: `mush-stream-common`, `mush-stream-host`,
  `mush-stream-client`.
- TOML configuration loader (`host.toml`).
- DXGI Desktop Duplication capture; GPU-side crop with `CopySubresourceRegion`
  into a sub-texture sized to the configured rectangle.
- `mush-stream-host --png` writes one cropped frame to `./capture-debug.png`
  for visual verification of the capture rectangle.

### Milestone 2 — capture + encode to MP4

- `h264_nvenc` encoder via `ffmpeg-the-third`, configured per spec: preset
  `p1`, tune `ll`, `zerolatency=1`, no B-frames, gop = fps (1-second keyframe
  interval), configurable bitrate.
- MP4 muxing with global-header SPS/PPS extradata.
- `mush-stream-host` (default mode) records 5 seconds to
  `./capture-debug.mp4`. Verify by playing it in VLC or running
  `ffprobe ./capture-debug.mp4`.

The encode path currently CPU-roundtrips BGRA between capture's staging
texture and ffmpeg's encoder input. Milestone 5/7 will revisit the
GPU-resident path (D3D11 hwframes → NVENC) once latency is the focus.

### Milestone 3 — wire protocol

- `mush-stream-common` ships the wire formats: 20-byte little-endian video
  packet header (≤1200 byte NAL fragments, ≤1400 byte UDP datagrams),
  16-byte input packets, 1-byte control tags (`request_keyframe`,
  `disconnect`).
- `VideoFramer` splits encoder NAL output into UDP datagrams with a
  caller-supplied emit closure (no per-frame allocation in the hot path).
- `VideoReassembler` accumulates packets per `frame_id`, tolerates packet
  reorder, drops stale frames after newer ones complete, evicts oldest
  pending frames when `max_pending` is exceeded, and rejects malformed
  inputs (`packet_count == 0`, `packet_index >= packet_count`,
  inconsistent `packet_count` across packets, oversize datagrams).
- `InputReceiver` enforces drop-on-stale-sequence using 16-bit wrapping
  comparison so the second-or-so of in-flight sequence space we'd
  plausibly see survives the wraparound.
- 22 unit tests cover header roundtrip, single/multi-packet roundtrip,
  reorder, drop, duplicate, stale-after-newer, eviction, malformed
  inputs, sequence wraparound, and the control tag enum.

The actual UDP socket plumbing (`tokio::UdpSocket`) lands in milestone 4
when the framer/reassembler are wired into the capture-encode-decode
pipeline.

## Setup

### One-time: install ffmpeg dev libraries (host only)

`mush-stream-host` links against ffmpeg 7.x with NVENC support. The host
crate **will not compile** until ffmpeg headers and import libraries are
available to the cargo build.

Recommended path on Windows:

1. Install `pkg-config` (e.g. `winget install bloodrock.pkgconfiglite` or
   the choco/scoop equivalent) and ensure it's on `PATH`.
2. Download an ffmpeg "shared" build that includes NVENC, e.g. from
   <https://www.gyan.dev/ffmpeg/builds/> (the `release-shared` package) or
   the BtbN GitHub releases. NVENC support is included by default in
   modern Windows builds.
3. Extract somewhere stable, e.g. `C:\ffmpeg\`.
4. Set, in System or User environment variables:
   - `FFMPEG_DIR=C:\ffmpeg`
   - `PKG_CONFIG_PATH=C:\ffmpeg\lib\pkgconfig`
   - Add `C:\ffmpeg\bin` to `PATH` (so the runtime DLLs are findable).
5. Restart your shell / IDE so the env vars take effect, then
   `cargo build -p mush-stream-host`.

Alternatively, vcpkg works:
`vcpkg install ffmpeg[nvcodec]:x64-windows`, then set `VCPKG_ROOT`.

### Other host requirements

- NVIDIA GPU with NVENC (any GTX 600-series or newer; for low-latency
  H.264 encoding you almost certainly already have this).
- ViGEmBus driver (milestone 6+, for virtual gamepad injection).

## How to run

```sh
# One-time:
cp host.toml.example host.toml
# Edit [capture] to your screen region; check [encode] fps and bitrate.

# Milestone 2 (default): record 5 seconds of video to MP4.
cargo run -p mush-stream-host
# Verify by opening ./capture-debug.mp4 in VLC.

# Milestone 1 path: write a single cropped PNG for crop-rect verification.
cargo run -p mush-stream-host -- --png
# Verify by opening ./capture-debug.png.

# Either mode accepts an explicit config path as a positional arg:
cargo run -p mush-stream-host -- ./my-config.toml
cargo run -p mush-stream-host -- --png ./my-config.toml
```

Set `RUST_LOG=debug` for verbose tracing output.

## Roadmap

1. **Workspace skeleton + capture-to-PNG.** ✅
2. **Capture + encode to file (NVENC → MP4).** ✅
3. **UDP transport layer with framing and reassembly.** ✅
4. End-to-end video over loopback.
5. Latency measurement and NVENC tuning (likely lifts capture→encode to
   GPU-resident D3D11 hwframes).
6. Gamepad passthrough (gilrs → ViGEm).
7. Robustness: keyframe requests on loss, FEC, send-side pacing.

## Networking

No NAT traversal is built in. Use [Tailscale](https://tailscale.com/) — both
sides bind to a configurable interface, and Tailscale handles encryption and
peer discovery.
