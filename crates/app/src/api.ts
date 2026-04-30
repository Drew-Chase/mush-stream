/**
 * Thin typed wrappers around the Tauri command + event surface.
 *
 * Every command returns `Promise<T>` and throws a string on backend
 * error (Tauri serializes our `Result<_, String>` as a thrown string).
 * Events are exposed via `on*` helpers that return an unlisten thunk.
 */
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getVersion } from "@tauri-apps/api/app";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";

// --- Probe -----------------------------------------------------------

export type ProbeStatus = "ok" | "mid" | "bad";

export interface ProbeRow {
  status: ProbeStatus;
  detail: string;
}

export interface SystemProbe {
  ready: boolean;
  nvenc: ProbeRow;
  nvdec: ProbeRow;
  vigem: ProbeRow;
  ffmpeg: ProbeRow;
  udpPort: ProbeRow;
  upnp: ProbeRow;
  gpuLabel: string;
}

export const systemProbe = () => invoke<SystemProbe>("system_probe");

// --- Monitors --------------------------------------------------------

export interface MonitorInfo {
  index: number;
  name: string;
  virtualX: number;
  virtualY: number;
  width: number;
  height: number;
  primary: boolean;
  /** Active refresh rate in Hz (e.g. 60, 120, 144). Defaults to 60
   *  when the OS lookup fails. */
  refreshHz: number;
}

export interface MonitorScreenshot {
  width: number;
  height: number;
  /** `data:image/png;base64,...` ready to drop into an `<img>`. */
  dataUrl: string;
}

export const monitorsList = () => invoke<MonitorInfo[]>("monitors_list");
export const monitorScreenshot = (index: number) =>
  invoke<MonitorScreenshot>("monitor_screenshot", { index });

// --- Addresses -------------------------------------------------------

export type AddressKind = "lan" | "public";

export interface LocalAddress {
  kind: AddressKind;
  ip: string;
  port: number;
}

export interface ShareAddresses {
  primary: string | null;
  addresses: LocalAddress[];
  upnpEnabled: boolean;
}

export const hostAddresses = () => invoke<ShareAddresses>("host_addresses");

// --- Audio sessions --------------------------------------------------

export interface AudioSessionInfo {
  pid: number;
  /** Leaf exe name (e.g. `chrome.exe`) or `"System"` for the
   *  system-sounds session. Matches what goes into `host.toml`'s
   *  `[audio].blacklist`. */
  processName: string;
  /** Friendly display name; empty for sessions that don't set one. */
  displayName: string;
  isSystem: boolean;
  state: "Active" | "Inactive" | "Expired" | "Unknown";
}

export const audioSessionsList = () =>
  invoke<AudioSessionInfo[]>("audio_sessions_list");

// --- Gamepads --------------------------------------------------------

export interface GamepadInfo {
  /** gilrs gamepad id (`usize::from(GamepadId)` narrowed to u32).
   *  Stable for the lifetime of the host process; pass back via
   *  `ConnectOptions.gamepadId` to pin which controller forwards. */
  id: number;
  name: string;
  isConnected: boolean;
}

export const gamepadsList = () => invoke<GamepadInfo[]>("gamepads_list");

// --- Recents ---------------------------------------------------------

export interface RecentEntry {
  address: string;
  name: string;
  lastUsed: number;
}

export const recentsList = () => invoke<RecentEntry[]>("recents_list");
export const recentsAdd = (address: string) =>
  invoke<RecentEntry[]>("recents_add", { address });
export const recentsClear = () => invoke<void>("recents_clear");

// --- Configs ---------------------------------------------------------
// Mirrors the host/client crate's serde shapes verbatim. `audio` etc.
// are optional in some places; we keep them required here since both
// crates apply Default::default() during deserialization.

export interface HostConfig {
  capture: {
    output_index: number;
    x: number;
    y: number;
    width: number;
    height: number;
  };
  network: {
    listen_port: number;
    enable_upnp: boolean;
  };
  encode: {
    bitrate_kbps: number;
    fps: number;
  };
  audio: {
    enabled: boolean;
    bitrate_kbps: number;
    blacklist: string[];
  };
}

export interface ClientConfig {
  network: {
    host: string;
  };
  display: {
    width: number;
    height: number;
    title: string;
    fullscreen: boolean;
  };
  decode: {
    prefer_hardware: boolean;
  };
  audio: {
    enabled: boolean;
  };
  input: {
    forward_pad: boolean;
    /** gilrs gamepad id to forward, or null for "first available". */
    gamepad_id: number | null;
  };
}

export const configLoadHost = () => invoke<HostConfig>("config_load_host");
export const configSaveHost = (cfg: HostConfig) =>
  invoke<void>("config_save_host", { cfg });
export const configLoadClient = () => invoke<ClientConfig>("config_load_client");
export const configSaveClient = (cfg: ClientConfig) =>
  invoke<void>("config_save_client", { cfg });

// --- Sessions --------------------------------------------------------

export type HostState = "idle" | "starting" | "broadcasting" | "stopping";

export interface HostStateEvent {
  state: HostState;
  error: string | null;
}

export const hostStart = () => invoke<void>("host_start");
export const hostStop = () => invoke<void>("host_stop");
export const hostStatus = () => invoke<HostState>("host_status");

/**
 * Push notification fired by the backend when the bound client peer
 * for the active host session changes. `address` is `"ip:port"` once
 * the host sees a client's first packet (or when the peer rotates to
 * a new ephemeral port), and `null` at session end so the UI can
 * clear any "currently connected" indicator.
 */
export interface HostPeerEvent {
  address: string | null;
}

/** Pull the current host-session peer (returns null when no session
 *  is running, or when running but no client packet has arrived yet).
 *  Used on mount to recover state after a page reload. */
export const hostPeer = () => invoke<string | null>("host_peer");

export type ClientState =
  | "idle"
  | "connecting"
  | "connected"
  /** Previously connected; the host went silent (or the initial
   *  connect failed) and the runner is sleeping before its next
   *  retry. The native client window stays open so the session can
   *  resume in place once the host returns. */
  | "reconnecting"
  | "disconnected"
  | "error";

export interface ClientStateEvent {
  state: ClientState;
  address: string | null;
  error: string | null;
}

export interface ConnectOptions {
  address: string;
  hardwareDecode: boolean;
  forwardPad: boolean;
  /** gilrs gamepad id to forward exclusively, or null for "first
   *  available". Surfaced from the Connect page's gamepad dropdown. */
  gamepadId: number | null;
  audio: boolean;
}

export const clientConnect = (options: ConnectOptions) =>
  invoke<void>("client_connect", { options });
export const clientDisconnect = () => invoke<void>("client_disconnect");
export const clientStatus = () => invoke<string | null>("client_status");

// --- Logs ------------------------------------------------------------

export interface LogLine {
  ts: string;
  level: string;
  target: string;
  message: string;
}

export const logsBuffer = () => invoke<LogLine[]>("logs_buffer");

// --- Event subscribers -----------------------------------------------

export const onHostState = (cb: (e: HostStateEvent) => void): Promise<UnlistenFn> =>
  listen<HostStateEvent>("host:state", (ev) => cb(ev.payload));

export const onHostPeer = (cb: (e: HostPeerEvent) => void): Promise<UnlistenFn> =>
  listen<HostPeerEvent>("host:peer", (ev) => cb(ev.payload));

export const onClientState = (
  cb: (e: ClientStateEvent) => void,
): Promise<UnlistenFn> =>
  listen<ClientStateEvent>("client:state", (ev) => cb(ev.payload));

export const onAppLog = (cb: (line: LogLine) => void): Promise<UnlistenFn> =>
  listen<LogLine>("app:log", (ev) => cb(ev.payload));

// --- Updater ---------------------------------------------------------
// Re-export the plugin types verbatim so callers don't need to know
// which package they live in. The state context (hosting.tsx) holds
// the resolved Update object until either a re-check or an install.

export type { Update } from "@tauri-apps/plugin-updater";

/** Returns the running app's `Cargo.toml` / `tauri.conf.json` version. */
export const appVersion = (): Promise<string> => getVersion();

/**
 * Probe the configured updater endpoint. Returns `null` when the
 * running build is already at the latest version, or an `Update`
 * object describing the available release.
 *
 * The plugin verifies the manifest signature against the public key
 * embedded in `tauri.conf.json`'s `plugins.updater.pubkey` — a
 * tampered or unsigned manifest yields a thrown error here.
 */
export const checkForUpdate = (): Promise<Update | null> => check();

/**
 * Download + install the queued update, then relaunch the app. The
 * NSIS installer runs in `passive` mode (configured in
 * tauri.conf.json) so the user sees a brief progress UI.
 */
export const installUpdateAndRelaunch = async (update: Update): Promise<void> => {
  await update.downloadAndInstall();
  await relaunch();
};
