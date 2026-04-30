# mush-stream

Low-latency Windows-to-Windows desktop streaming for two-player split-screen
gaming. The host captures a configurable rectangular region of its desktop,
hardware-encodes it via NVENC, and streams it over UDP to a remote client.
The client decodes and displays the video, captures the friend's gamepad,
and sends input back to the host where it is injected as a virtual Xbox 360
controller via ViGEmBus.

Targets Windows 10/11 x86_64 only.

## What works

- **Capture** — DXGI Desktop Duplication, GPU-side crop with
  `CopySubresourceRegion` to a sub-texture sized to the configured
  rectangle. Rate-limited to the configured fps so high-refresh host
  monitors don't over-drive NVENC.
- **Encode** — `h264_nvenc` via `ffmpeg-the-third`. Tuned for low-latency
  live: preset p4, `tune=ll`, `zerolatency=1`, no B-frames, no lookahead,
  CBR, `multipass=qres`, spatial AQ. Default 9 Mbps at 60 fps for 1440p
  streams looks clean.
- **Wire protocol** — single UDP socket per side. The host learns the
  client's address from the first packet it receives; UDP hole-punching
  takes care of the NAT return path. 20-byte little-endian video packet
  header, ≤1200-byte NAL fragments, ≤1400-byte UDP datagrams. Reed-Solomon
  FEC at 10% redundancy so single-packet drops recover without an IDR.
- **Decode** — `h264_cuvid` (NVIDIA hardware) with software h264 fallback.
- **Display** — `winit` + `pixels` with Mailbox present mode (no vsync
  blocking). Decoder fast-forwards through any frame backlog so a
  pipeline stall surfaces as a single jump-forward, never as slow-motion
  catch-up.
- **Input** — gamepad polling at 250 Hz via `gilrs`, packed into the wire
  format using `vigem-client`'s `XButtons` bit layout so the host plugs
  the bits straight into a virtual Xbox 360 controller via ViGEmBus.
- **Resilience** — keyframe-on-loss recovery, send-side token-bucket
  pacing sized to absorb a full keyframe, 8 MiB UDP buffers on both
  sides, optional UPnP forwarding for the host's listen port.
- **Diagnostics** — Ctrl+Alt+D toggles a debug overlay on the client
  showing backend, fps, bitrate, lag, and rolling p50/p95/p99/max.
  Both sides emit a per-second throughput log line at info level.
- **Distribution** — `just build` produces `target/dist/{host,client}/`
  containing the binary, every ffmpeg DLL, the `.toml.example`, and the
  README. Send the folder to a friend.

## Quickstart (localhost)

```sh
cp host.toml.example   host.toml
cp client.toml.example client.toml

just build
target/dist/host/app-host.exe       # in one terminal
target/dist/client/client.exe   # in another
```

Press **Ctrl+Alt+D** in the client window to see the debug overlay.

## Setup

### ffmpeg dev libraries (one-time, both sides)

`mush-stream-host` and `mush-stream-client` both link against ffmpeg with
NVENC + h264 decode support. Neither will compile until ffmpeg headers
and import libraries are available.

1. Install `pkg-config` (e.g. `winget install bloodrock.pkgconfiglite`)
   and ensure it's on `PATH`.
2. Download an ffmpeg "shared" build, e.g. the **release-shared** package
   from <https://www.gyan.dev/ffmpeg/builds/> or a `*-shared.zip` from
   the BtbN GitHub releases. Pick the variant that has `bin/`, `lib/`,
   and `include/` folders.
3. Extract somewhere stable (e.g. `C:\ffmpeg\`).
4. Set, in System or User environment variables:
   - `FFMPEG_DIR=C:\ffmpeg`  *(point at the folder above the `bin/`)*
   - `PKG_CONFIG_PATH=C:\ffmpeg\lib\pkgconfig`
   - Add `C:\ffmpeg\bin` to `PATH` for runtime DLL discovery.
5. Restart your shell / IDE.

The `build.rs` in each binary crate auto-copies `$FFMPEG_DIR/bin/*.dll`
into `target/{profile}/` and `target/{profile}/deps/` on every build,
so `cargo run` and the produced `.exe` find the runtime DLLs without
any further PATH fiddling. `just build` propagates the same DLLs into
the dist subfolders.

`vcpkg install ffmpeg[nvcodec]:x64-windows` works as an alternative if
you prefer.

### Host requirements

- NVIDIA GPU with NVENC support (any GTX 600-series or newer; almost
  certainly already present if you're considering streaming).
- [ViGEmBus](https://github.com/nefarius/ViGEmBus) driver for virtual
  gamepad injection. The host degrades gracefully without it — video
  keeps streaming, input is dropped with a logged warning.

### Client requirements

- Any GPU. h264_cuvid is preferred when available (NVIDIA), software
  h264 otherwise.

## Configuration

Each side reads its own TOML. Detailed per-field documentation lives in
the crate-level READMEs:

- [`crates/mush-stream-host/README.md`](crates/mush-stream-host/README.md)
- [`crates/mush-stream-client/README.md`](crates/client/README.md)

The minimal configs:

```toml
# host.toml
[capture]
output_index = 0
x = 2560
y = 0
width  = 2560
height = 1440

[network]
listen_port = 9002
enable_upnp = false

[encode]
bitrate_kbps = 9000
fps          = 60
```

```toml
# client.toml
[network]
host = "192.168.1.100:9002"   # host's address — only field the client needs

[display]
width  = 2560
height = 1440
title  = "mush-stream"
fullscreen = false

[decode]
prefer_hardware = true
```

## Networking

The recommended path is [Tailscale](https://tailscale.com/) — both sides
get a routable IP, encryption is handled, no UPnP fiddling. Set the
client's `host` to the host machine's Tailscale address.

Without Tailscale, set `enable_upnp = true` in `host.toml` so the host's
listen port is forwarded automatically through the host's router. The
client doesn't need any port forwarding — its outbound `connect()` opens
the NAT return path for free (UDP hole-punching). UPnP failures are
logged and ignored; the binary keeps running but won't be reachable from
outside the LAN.

## Debug overlay

Press **Ctrl+Alt+D** in the client window to toggle:

```
mush-stream  Ctrl+Alt+D
backend  h264_cuvid
frames   12345
fps      59.7
rx Mbps  9.42
lag ms   18
p50/95/99/max ms  17/22/29/33
```

`lag ms` is monotonic-clock measured: the time between the first packet
of a frame arriving at the client and that frame being on screen. It
captures network-arrival → render latency. The capture/encode/wire
portion is hidden but constant, so the variance you see here is what
matters for tuning.

## Distribution

```sh
just build
```

Produces `target/dist/host/` and `target/dist/client/`, each
self-contained with the binary, every ffmpeg DLL, the `.toml.example`,
and the README. Zip up either subfolder and send it to a friend; they
edit the `.toml`, run the `.exe`, done.

## Architecture

Three crates:

- **`mush-stream-common`** — wire protocol (video framer/reassembler,
  input + control packets, FEC encode/decode). No I/O. 27 unit tests.
- **`mush-stream-host`** — capture + encode + UDP transport + ViGEm.
  Two threads beyond tokio: one for the sync DXGI/NVENC capture loop,
  one for the sync ViGEm device updates.
- **`mush-stream-client`** — UDP transport + decode + display + gamepad.
  Three threads beyond tokio: one for the sync ffmpeg decoder, one for
  gilrs polling, one for winit on main.

Both sides use a single UDP socket. The host's socket is bound to the
configured `listen_port` and learns its peer from the first received
packet. The client's socket is `connect()`-ed to `host`; the kernel
filters incoming traffic to that peer, and the same socket carries
input/control out to the host.

Two stages of latency control:
- **Token-bucket pacer** on the host's send side, sized at 256 KiB
  (~one keyframe) so IDR bursts clear without throttling, refill
  rate at `bitrate × 1.25`.
- **Decoder fast-forward** on the client: any backlog of pending
  frames is reference-decoded cheaply through NVDEC and only the
  freshest is colour-converted + presented.

## Tests

```sh
cargo test --workspace
```

`mush-stream-common` ships 27 unit tests covering header roundtrip,
single/multi-packet roundtrip, reorder, drop, duplicate,
stale-after-newer, eviction, malformed inputs, sequence wraparound,
control messages, FEC encode + reconstruct, fallback when FEC frame
exceeds galois_8 limit, and the input drop-on-stale-sequence logic.
`mush-stream-host` ships 2 deterministic timing tests for the
`TokenBucket` using `tokio::test(start_paused = true)`.

## Logs

`RUST_LOG=info` (default) prints once-per-second throughput summaries
on each side:

```
INFO host throughput (1s) frames=60 bytes=412345 dropped_full_channel=0 keyframes_forced=0 max_iter_us=4823
INFO client recv throughput (1s) packets=520 bytes=730000 frames=60 gaps=0 keyframe_requests=1
```

Plus the client emits a percentile snapshot every 60 frames:

```
INFO client lag window count=60 min_us=15041 p50_us=17104 p95_us=22011 p99_us=28944 max_us=33102 avg_us=18327
```

When something stalls these are usually enough to localise it. If
the host's `frames` counter goes to 0 for a second, the host stopped
producing; if the host stays healthy and the client's `packets`
counter goes to 0, the network's the issue; if both stay healthy and
the overlay's fps drops, the decode/render side is at fault.

`RUST_LOG=debug` adds per-event detail: keyframe-request reasons,
backlog fast-forward events, duplicate-packet drops, etc.
