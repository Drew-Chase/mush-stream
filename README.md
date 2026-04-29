# mush-stream

Low-latency Windows-to-Windows desktop streaming for two-player split-screen
gaming. The host captures a configurable rectangular region of its desktop,
hardware-encodes it via NVENC, and streams it over UDP to a remote client. The
client decodes and displays the video, captures the friend's gamepad, and sends
input back to the host where it is injected as a virtual Xbox 360 controller.

Targets Windows 10/11 x86_64 only.

## What works

### Milestone 1 — capture-to-PNG ✅

- Cargo workspace with three crates: `mush-stream-common`, `mush-stream-host`,
  `mush-stream-client`.
- TOML configuration loader (`host.toml`).
- DXGI Desktop Duplication capture; GPU-side crop with `CopySubresourceRegion`
  into a sub-texture sized to the configured rectangle.
- `mush-stream-host --png` writes one cropped frame to `./capture-debug.png`
  for visual verification of the capture rectangle.

### Milestone 2 — capture + encode to MP4 ✅

- `h264_nvenc` encoder via `ffmpeg-the-third`, configured per spec: preset
  `p1`, tune `ll`, `zerolatency=1`, no B-frames, gop = fps (1-second keyframe
  interval), configurable bitrate. Plus low-latency-friendly options
  `rc=cbr`, `delay=0`, `rc-lookahead=0`, `no-scenecut=1`.
- MP4 muxing with global-header SPS/PPS extradata.
- `mush-stream-host --mp4` records 5 seconds to `./capture-debug.mp4`.
  Verify by playing it in VLC or running `ffprobe ./capture-debug.mp4`.

### Milestone 3 — wire protocol ✅

- `mush-stream-common::protocol` ships the wire formats: 20-byte little-endian
  video packet header (≤1200 byte NAL fragments, ≤1400 byte UDP datagrams),
  16-byte input packets, 1-byte control tags (`request_keyframe`, `disconnect`).
- `VideoFramer` splits encoder NAL output into UDP datagrams with a
  caller-supplied emit closure (no per-frame allocation in the hot path).
- `VideoReassembler` accumulates packets per `frame_id`, tolerates reorder,
  drops stale frames after newer ones complete, evicts oldest pending frames
  when `max_pending` is exceeded, rejects malformed inputs.
- `InputReceiver` enforces drop-on-stale-sequence using 16-bit wrapping
  comparison.

### Milestone 4 — end-to-end video over UDP ✅

- `mush-stream-host` (default mode now `--stream`) spawns a tokio runtime,
  drives capture+encode in a dedicated `std::thread` (DXGI/NVENC are sync
  APIs), and `tokio::UdpSocket`s the framed datagrams to the configured peer.
- `mush-stream-client` binds the configured port, drains UDP into the
  reassembler, decodes via `h264_cuvid` (NVIDIA hardware decode) with software
  h264 fallback, blits decoded RGBA into a `pixels` framebuffer presented by
  a `winit` window.
- Threading: main runs `winit` (winit requires it); two `std::thread`s host
  the tokio runtime and the sync ffmpeg decoder; tokio mpsc bridges them.

### Milestone 5 — latency measurement + NVENC tuning ✅

- Wire header carries `timestamp_us` (host capture wallclock); the client
  computes glass-to-glass latency at present time and logs a rolling
  60-frame percentile snapshot (min/p50/p95/p99/max/avg) once per second.
- Cumulative min/max/avg dumped on shutdown.
- NVENC private options tuned for low-latency live (see M2 above).

### Milestone 6 — gamepad passthrough ✅

- Client polls a connected gamepad via `gilrs` in a dedicated `std::thread`
  at the project-spec 250 Hz cadence. Each tick snapshots state into an
  `InputPacket` packed with `vigem-client::XButtons` bit layout (= XINPUT)
  and pushes via tokio mpsc to the network thread, which UDP-sends to the
  host.
- Host receives input/control on the input port, dispatches by datagram
  size (16 = input, 1 = control), forwards inputs to a ViGEm thread that
  applies them to a virtual Xbox 360 wired controller. Degrades gracefully
  if ViGEmBus driver is missing — video keeps streaming, input is dropped
  with a logged warning.

### Milestone 7 — robustness ✅

- **Keyframe-on-loss recovery**: client video receiver watches the
  `frame_id` sequence; on a forward gap (one or more frames lost in transit)
  or a non-IDR first frame (mid-stream join), pushes
  `ControlMessage::RequestKeyframe` to the host. Rate-limited to one
  request per 200 ms. Host's encode loop drains the channel and forces an
  IDR on the next frame via NVENC's `pict_type = AV_PICTURE_TYPE_I`.
- **FEC at 10% redundancy**: `VideoFramer::frame_with_fec(parity_ratio)`
  computes Reed-Solomon parity shards via `reed-solomon-erasure`. Wire
  format extends the header with `parity_count` (u8) and `last_data_size`
  (u16) using the M3 pad bytes — when FEC is off, byte-equivalent to M3.
  Receiver collects any N of N+K shards per frame and reconstructs missing
  data.
- **Send-side pacing**: host UDP sender awaits a token bucket sized in
  bytes (capacity ~12 packets, refill rate = encoder bitrate × 1.25
  headroom) before each `socket.send`, so encoder bursts don't micro-flood
  the receiver.

## Setup

### One-time: install ffmpeg dev libraries (host **and** client)

Both `mush-stream-host` and `mush-stream-client` link against ffmpeg with
NVENC + h264 decode support. Neither will compile until ffmpeg headers and
import libraries are available to the cargo build.

Recommended path on Windows:

1. Install `pkg-config` (e.g. `winget install bloodrock.pkgconfiglite` or
   the choco/scoop equivalent) and ensure it's on `PATH`.
2. Download an ffmpeg "shared" build that includes NVENC, e.g. from
   <https://www.gyan.dev/ffmpeg/builds/> (the `release-shared` package) or
   the BtbN GitHub releases.
3. Extract somewhere stable, e.g. `C:\ffmpeg\`.
4. Set, in System or User environment variables:
   - `FFMPEG_DIR=C:\ffmpeg`
   - `PKG_CONFIG_PATH=C:\ffmpeg\lib\pkgconfig`
   - Add `C:\ffmpeg\bin` to `PATH` (so the runtime DLLs are findable).
5. Restart your shell / IDE so the env vars take effect, then
   `cargo build --workspace`.

Alternatively, vcpkg works:
`vcpkg install ffmpeg[nvcodec]:x64-windows`, then set `VCPKG_ROOT`.

### Other requirements

- **Host**: NVIDIA GPU with NVENC; ViGEmBus driver
  (<https://github.com/nefarius/ViGEmBus>) for virtual gamepad injection.
- **Client**: any GPU; D3D11-capable for hardware decode (otherwise the
  software h264 path is used).

## How to run

```sh
# One-time:
cp host.toml.example host.toml      # on the host machine
cp client.toml.example client.toml  # on the client machine
# Edit each to point at the right [network] addresses for your setup.
```

### Stream end-to-end on localhost (M4 verification)

Two terminals, same machine:

```sh
# Terminal 1 (client) — open the window first:
cargo run -p mush-stream-client

# Terminal 2 (host) — start streaming once the client is listening:
cargo run -p mush-stream-host
```

The client window should display the host's configured capture region
in real time. Set `RUST_LOG=info` (default) to see the rolling
glass-to-glass latency snapshots; `RUST_LOG=debug` for verbose tracing.

### Other host modes

```sh
# Record a 5-second MP4 verification clip (M2):
cargo run -p mush-stream-host -- --mp4

# Single-frame PNG of the configured crop (M1):
cargo run -p mush-stream-host -- --png

# Pass a config path (works in any mode):
cargo run -p mush-stream-host -- --stream ./host.toml
```

## Networking

The recommended path is [Tailscale](https://tailscale.com/) — both sides
bind to a configurable interface, and Tailscale handles encryption and
peer discovery. No further configuration needed beyond pointing each
side's config at the other's Tailscale IP.

### Optional: UPnP port forwarding

If you're not on Tailscale and your router supports UPnP, set
`[network] enable_upnp = true` in either `host.toml` or `client.toml`
and the corresponding side will request a UDP port mapping at startup
(host forwards `input_bind`, client forwards `video_bind`). The mapping
is removed on graceful shutdown via an RAII guard. UPnP failures are
logged and otherwise ignored — the binary continues to run, you just
won't be reachable from outside the LAN until you configure forwarding
manually.

UPnP traffic is unencrypted; if you care about that, stick with
Tailscale.

## Tests

```sh
cargo test -p mush-stream-common
```

The protocol crate has 31 unit tests covering header roundtrip,
single/multi-packet roundtrip, reorder, drop, duplicate, stale-after-newer,
eviction, malformed inputs, sequence wraparound, control messages, FEC
roundtrip, FEC reconstruct from data drop, FEC fast-path on parity drop,
and FEC unrecoverable loss. The host transport's `TokenBucket` has 2 timing
tests using `tokio::test(start_paused = true)`.
