# mush-stream-client

The client binary of [mush-stream](../../README.md). Receives streamed
video over UDP, decodes via ffmpeg (`h264_cuvid` with software h264
fallback), presents in a `winit` window via `pixels`, and reads the
local gamepad to send input back to the host.

## Usage

```sh
mush-stream-client                  # loads ./client.toml
mush-stream-client ./other.toml     # explicit config path
```

Press **Ctrl+Alt+D** in the client window to toggle a debug overlay.

## Configuration

`./client.toml`:

```toml
[network]
# Host's UDP address — the only thing this client needs to know about
# the network. The client connects() its UDP socket here; the host
# learns the client's address from that first packet (UDP hole-punch)
# and starts sending video back.
host = "192.168.1.100:9002"

[display]
# Pixel buffer dimensions. Match the host's capture rect to avoid
# scaling on the client.
width  = 2560
height = 1440
title  = "mush-stream"
fullscreen = false

[decode]
# Try h264_cuvid first; if it fails to initialise, fall back to the
# bundled software h264 decoder. Set false if you're sure you don't
# have an NVIDIA GPU (skips the failed-open warning).
prefer_hardware = true
```

## Requirements

- **ffmpeg dev libraries** with h264 decode support (cuvid optional but
  recommended). See the [main README](../../README.md#setup). The
  crate's `build.rs` copies the runtime DLLs alongside the binary at
  build time, so you don't need to touch PATH.
- **A GPU.** Anything that can drive a `wgpu` surface is fine —
  `pixels` falls back gracefully.
- Windows 10/11 x86_64.

No driver installs required for this side specifically — gamepad input
goes through `gilrs`, which uses the OS's standard HID stack.

## How playback works internally

1. **Network thread** (tokio runtime, 2 workers, single UDP socket):
   - on startup: bind ephemeral, `connect()` to host, send one
     `RequestKeyframe` as a discovery probe (also requests the IDR a
     fresh client always needs)
   - **video receive** task: drives a `VideoReassembler`, forwards
     completed frames (each annotated with the local `Instant` of its
     first packet) to the decode channel; on detected gaps requests a
     keyframe rate-limited at 1 / 200 ms
   - **input send** task: drains an mpsc and writes packets back to the
     host on the same socket
2. **Decode thread** (sync, `std::thread`):
   - blocking-recvs from the recv→decode channel (capacity 4)
   - if more frames are pending, fast-forwards through them via
     `decode_without_present` — advances NVDEC's reference state
     cheaply, only colour-converts the freshest frame
   - sends `UserEvent::Frame` to the winit event loop via
     `EventLoopProxy::send_event`
3. **Gamepad thread** (sync, `std::thread`):
   - polls gilrs at 250 Hz; snapshots state into an `InputPacket` whose
     `buttons: u32` lower 16 bits use the same layout as
     `vigem-client::XButtons` so the host can plug them in directly
4. **Main thread** (winit):
   - `ApplicationHandler::user_event` updates `last_frame`,
     `request_redraw`s
   - `RedrawRequested` blits RGBA into `pixels.frame_mut()` and calls
     `pixels.render()` (Mailbox present mode — never vsync-blocks)

The combination of fast-forward + Mailbox keeps the latency tail
tight: any pipeline stall surfaces as a single jump-forward when it
ends, not as slow-motion catch-up at vsync rate.

## Debug overlay (Ctrl+Alt+D)

```
mush-stream  Ctrl+Alt+D
backend  h264_cuvid
frames   12345
fps      59.7
rx Mbps  9.42
lag ms   18
p50/95/99/max ms  17/22/29/33
```

| line | meaning |
|---|---|
| `backend` | which ffmpeg decoder opened (cuvid, h264, ...) |
| `frames` | total frames presented since startup |
| `fps` | rolling 2-second window |
| `rx Mbps` | rolling 2-second receive rate (encoded NAL bytes × 8) |
| `lag ms` | most recent client-side lag |
| `p50/95/99/max ms` | percentile snapshot from the last 60-frame window |

### About `lag ms`

`lag` is measured locally with `std::time::Instant` between the time
the *first* packet of a frame arrived and the time that frame is on
screen. It captures **network arrival → reassembly → decode → render**
for that specific frame. The capture/encode/wire portion of true
glass-to-glass is hidden but constant, so the variance you see is what
matters for tuning.

The metric doesn't require synchronised clocks across machines —
`Instant` is monotonic and process-local — so the number is honest
even on a fresh install with no NTP setup.

## Logs

At `RUST_LOG=info` (default):

Once per second from the receive loop:
```
INFO client recv throughput (1s) packets=520 bytes=730000 frames=60 gaps=0 keyframe_requests=1
```

Once per 60 frames from the display thread:
```
INFO client lag window count=60 min_us=15041 p50_us=17104 p95_us=22011 p99_us=28944 max_us=33102 avg_us=18327
```

Plus startup lines for the chosen decoder backend, UPnP-or-not, and
window creation.

`RUST_LOG=debug` adds:
- per-event reasons for every `RequestKeyframe` sent
- fast-forward backlog drains and their sizes
- duplicate-packet drops at the reassembler

## What to look at when something's wrong

- **Black window, no frames**: check the client's recv-throughput log.
  If `packets=0`, nothing is arriving — host not running, wrong
  address/port, or NAT path not open. If `packets > 0` but `frames=0`,
  the reassembler is rejecting everything (header mismatch, oversize
  datagrams) — check stderr for `malformed` warnings.
- **Stutter or pause**: compare `host throughput` and `client recv
  throughput` for the same wall-clock second. If host produced 60 but
  client received 0, the network dropped them. If client received 60
  but the overlay's `fps` shows < 60, the issue is decode/render —
  check `fast_forward_events` in the shutdown log to see how often
  the backlog path fired.
- **Gamepad not detected**: gilrs prints at info level when a pad
  connects/disconnects. If you see "gamepad polling at 250Hz; waiting
  for a connected pad" but never the "gamepad connected" line,
  Windows isn't surfacing the pad through HID — try a different
  USB port or a controller-specific driver (DS4Windows, etc.).
- **Lag percentiles are wide (e.g. p99 ≫ p50)**: usually contention.
  Common causes: another GPU-heavy program (browsers with hardware
  acceleration, a recording tool, OBS), or the host machine's
  encoder competing for GPU with the host's own desktop. The
  client's [`fast_forward_events`] counter on shutdown tells you
  how often this happened.
