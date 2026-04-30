# mush-stream-host

The host binary of [mush-stream](../../README.md). Captures a configurable
rectangular region of the desktop, hardware-encodes it via NVENC, and
streams it over UDP to a connected client. Receives client input on the
same socket and applies it to a virtual Xbox 360 controller via ViGEmBus.

## Modes

```sh
app-host                      # default: stream
app-host --stream             # explicit stream mode
app-host --mp4                # record 5s of capture to ./capture-debug.mp4
app-host --png                # capture one frame to ./capture-debug.png

# All modes accept a positional config path:
app-host --stream ./host.toml
```

The MP4 and PNG modes are verification helpers — useful for confirming
the capture rectangle before going live, or for debugging encoder
settings without a client connected.

## Configuration

`./host.toml` (or whatever path you pass on the command line):

```toml
[capture]
# Index of the DXGI output to capture. 0 = primary monitor.
output_index = 0
# Top-left corner of the capture region, in pixels, relative to the chosen output.
x = 2560
y = 0
# Size of the capture region, in pixels.
width  = 2560
height = 1440

[network]
# UDP port the host listens on. The host learns each client's address
# from the first packet it receives — there's no peer field; the
# client's `host` config points at this port.
listen_port = 9002
# When true, ask the local router to forward `listen_port` via UPnP at
# startup so a client across NAT can reach this host without manual
# port forwarding.
enable_upnp = false

[encode]
# H.264 NVENC target bitrate in kbps. 9 Mbps is the default — paired
# with the encoder's preset=p4 + spatial-aq + multipass=qres tuning,
# this gives clean output at 1440p60 over a typical home upload link.
bitrate_kbps = 9000
# Capture/encode framerate. Match (or be a divisor of) the host's
# display refresh. DXGI delivers at the display rate; setting fps
# higher than that won't produce more real frames.
fps = 60
```

## Requirements

- **NVIDIA GPU with NVENC.** Any GTX 600-series or newer.
- **ffmpeg dev libraries** with NVENC support. See the [main
  README](../../README.md#setup) for setup. The crate's `build.rs` copies
  the runtime DLLs alongside the binary at build time so you don't need
  to touch PATH.
- **[ViGEmBus](https://github.com/nefarius/ViGEmBus) driver** for virtual
  gamepad injection. The host degrades gracefully if it's missing —
  video keeps streaming, input is dropped with a warning logged once.
- Windows 10/11 x86_64.

## How streaming works internally

1. **Capture thread** (sync, `std::thread`):
   - DXGI Desktop Duplication acquires the desktop image
   - `CopySubresourceRegion` crops to the configured rectangle on the GPU
   - the cropped texture is read back to a CPU staging buffer (BGRA)
2. **Encode** (same thread, called per captured frame):
   - NVENC encodes BGRA → H.264, gop=fps for 1 s IDR cadence
   - the framer produces N data + ⌈N×0.10⌉ Reed-Solomon parity packets
3. **Send** (tokio task, single UDP socket):
   - per-second token bucket sized at 256 KiB so a full keyframe drains
     in one burst; refill at `bitrate × 1.25`
   - destination address learned from the first inbound packet
4. **Receive** (tokio task, same socket):
   - 16-byte datagrams → `InputPacket`, forwarded to the ViGEm thread
   - 1-byte datagrams → `ControlMessage`, forwarded to the encode thread
     for keyframe-on-loss
5. **ViGEm thread** (sync, `std::thread`):
   - drains an mpsc of `InputPacket`s, plugs the `XButtons`-layout
     bits into a wired Xbox 360 target

The capture+encode loop is paced to the configured fps with
`std::thread::sleep` at the bottom of each iteration. DXGI hands off
frames at the host monitor's refresh rate (often 144/165 Hz on a gaming
rig); without the limiter NVENC's CBR controller would be fed faster
than its time_base assumes, causing the decoder-side ghosting you'd see
otherwise.

## Logs

At `RUST_LOG=info` (default), once per second:

```
INFO host throughput (1s) frames=60 bytes=412345 dropped_full_channel=0 keyframes_forced=0 max_iter_us=4823
```

| field | meaning |
|---|---|
| `frames` | encoded frames in the past second; should track configured fps |
| `bytes` | total NAL bytes produced (pre-FEC, pre-header) |
| `dropped_full_channel` | sender backpressure events; should be 0 |
| `keyframes_forced` | client `RequestKeyframe`s honoured; bumps on every reported gap |
| `max_iter_us` | longest single iteration of the capture+encode loop. Useful for spotting stalls |

`RUST_LOG=debug` also logs every `seconds=N` hash-mark on each
1-second boundary and the FEC encode-fail fallback (which only fires
when a single NAL exceeds 256 packets, ~300 KB).

## What to look at when something's wrong

- **Encoder didn't initialise**: `find_by_name("h264_nvenc")` returned
  None. Either the ffmpeg build doesn't include nvenc, or the NVIDIA
  driver isn't loaded.
- **Stream black or stuck**: check `frames=` in the throughput log. If
  it's 0, capture is the issue (DXGI access lost? wrong output_index?).
  If it's healthy, the issue is downstream — see the client side.
- **`max_iter_us` is huge**: a single capture+encode iteration is
  taking much longer than the frame budget. GPU contention with another
  process is the usual cause; check Task Manager.
- **`keyframes_forced` is high**: the client keeps detecting gaps and
  asking for IDRs. Likely network packet loss (drops on the link) or
  receiver-side buffer overflow.
- **ViGEm warning at startup**: install the ViGEmBus driver. The host
  will keep streaming video; only gamepad injection is disabled.

## Mode notes

- **`--mp4`**: records exactly 5 seconds (`fps × 5` frames) to
  `./capture-debug.mp4`. Same NVENC settings as streaming, but with the
  MP4 muxer's global-header SPS/PPS extradata. Verify quality with VLC
  or `ffprobe`.
- **`--png`**: captures one frame and writes it to `./capture-debug.png`.
  Useful for confirming the capture rectangle before going live.
