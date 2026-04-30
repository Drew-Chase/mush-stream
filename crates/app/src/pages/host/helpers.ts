/**
 * Shared bits for the Host-page card components.
 *
 * Each card lives in its own file under `pages/host/` and uses these
 * helpers + constants. The page itself (`pages/Host.tsx`) imports
 * from each card module and arranges them in the layout grid.
 */
import type { HostConfig } from "../../api";

/**
 * Skeleton config rendered while `useHosting().hostConfig` is still
 * `null` (first paint before the backend's `config_load_host`
 * resolves). Mirrors `host.toml.example` and the Rust crate's
 * `default_host_config()` so the UI never flashes empty rows.
 */
export const DEFAULT_CFG: HostConfig = {
  capture: { output_index: 0, x: 0, y: 0, width: 2560, height: 1440 },
  network: { listen_port: 9002, enable_upnp: false },
  encode: { bitrate_kbps: 9000, fps: 60 },
  audio: { enabled: true, bitrate_kbps: 96, blacklist: [] },
};

/**
 * FPS values offered in the Encode card's dropdown. The host's
 * NVENC pipeline works at any positive integer rate but these are
 * the targets users typically pick on Windows displays.
 */
export const FPS_OPTIONS = [30, 60, 90, 120, 144] as const;

/**
 * Map a bitrate (kbps) to a HeroUI `<Slider color>` zone. The bands
 * match the "low → smeary / high → no-perceptible-gain" sweet-spot
 * for 1440p NVENC streaming:
 * - 1-3 Mbps   danger  (likely smeary)
 * - 3-5 Mbps   warning (acceptable for static content)
 * - 5-12 Mbps  success (sweet spot)
 * - 12-16 Mbps warning (diminishing returns)
 * - 16-20 Mbps danger  (wasteful at 1440p60)
 */
export function bitrateZone(
  kbps: number,
): "danger" | "warning" | "success" {
  const mbps = kbps / 1000;
  if (mbps < 3 || mbps >= 16) return "danger";
  if (mbps < 5 || mbps >= 12) return "warning";
  return "success";
}

/**
 * Highest FPS from `FPS_OPTIONS` that doesn't exceed `refreshHz`.
 * e.g. 85 Hz monitor → 60 (the 90 fps option would be a half-frame-
 * per-vsync miss). Falls back to `FPS_OPTIONS[0]` for sub-30Hz
 * displays (rare).
 */
export function recommendFps(refreshHz: number): number {
  const valid = FPS_OPTIONS.filter((f) => f <= refreshHz);
  return valid.length > 0 ? Math.max(...valid) : FPS_OPTIONS[0];
}

/**
 * Crop-marquee drag mode — `move` for the body, `n`/`s`/`e`/`w` for
 * edge handles, `nw`/`ne`/`sw`/`se` for corner handles.
 */
export type DragMode =
  | "move"
  | "n"
  | "s"
  | "e"
  | "w"
  | "ne"
  | "nw"
  | "se"
  | "sw";

export const SAVE_DEBOUNCE_MS = 300;
