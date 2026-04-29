# mush-stream

Low-latency Windows-to-Windows desktop streaming for two-player split-screen
gaming. The host captures a configurable rectangular region of its desktop,
hardware-encodes it via NVENC, and streams it over UDP to a remote client. The
client decodes and displays the video, captures the friend's gamepad, and sends
input back to the host where it is injected as a virtual Xbox 360 controller.

Targets Windows 10/11 x86_64 only.

## What works (milestone 1)

- Cargo workspace with three crates: `mush-stream-common`, `mush-stream-host`,
  `mush-stream-client`.
- TOML configuration loader (`host.toml`).
- DXGI Desktop Duplication capture in the host crate.
- GPU-side crop with `CopySubresourceRegion` into a sub-texture sized to the
  configured rectangle.
- One-shot capture-to-PNG verification path: the host binary acquires a single
  cropped frame and writes it to `./capture-debug.png` so you can visually
  confirm the rectangle is correct.

The PNG-save path is milestone-1 only. Milestone 2 will keep the cropped
texture GPU-resident and feed it directly into NVENC.

## How to run (milestone 1)

```sh
# 1. From repo root, copy and edit the example config:
cp host.toml.example host.toml
# Edit [capture] to point at the screen region you want to verify.

# 2. Build and run the host:
cargo run -p mush-stream-host
# Or pass a config path explicitly:
cargo run -p mush-stream-host -- ./my-config.toml

# 3. Open ./capture-debug.png and confirm the cropped region looks right.
```

Set `RUST_LOG=debug` for verbose tracing output.

## Roadmap

1. **Workspace skeleton + capture-to-PNG.** ✅ (this milestone)
2. Capture + encode to file (NVENC → MP4).
3. UDP transport layer with framing and reassembly.
4. End-to-end video over loopback.
5. Latency measurement and NVENC tuning.
6. Gamepad passthrough (gilrs → ViGEm).
7. Robustness: keyframe requests on loss, FEC, send-side pacing.

## Networking

No NAT traversal is built in. Use [Tailscale](https://tailscale.com/) — both
sides bind to a configurable interface, and Tailscale handles encryption and
peer discovery.

## Dependencies

- Host: NVIDIA GPU with NVENC support (milestone 2+); ViGEmBus driver for
  virtual gamepad injection (milestone 6+).
- Client: any GPU; D3D11-capable for hardware decode (milestone 4+).
