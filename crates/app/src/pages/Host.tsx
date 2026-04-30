import { useEffect, useMemo, useRef, useState } from "react";
import {
  IcBroadcast,
  IcCheck,
  IcCopy,
  IcGamepad,
  IcMonitor,
  IcSpark,
  IcStop,
  IcZap,
} from "../components/Icons";
import { Btn, Card, Field, Sparkline, Stat, Tag } from "../components/primitives";
import { useRolling } from "../hooks/useRolling";
import { useHosting } from "../hosting";
import {
  configSaveHost,
  hostStart,
  hostStop,
  monitorScreenshot,
  monitorsList,
  type HostConfig,
  type MonitorInfo,
  type MonitorScreenshot,
} from "../api";

const SAVE_DEBOUNCE_MS = 300;
const MIN_CAPTURE_SIZE = 64;

const DEFAULT_CFG: HostConfig = {
  capture: { output_index: 0, x: 0, y: 0, width: 2560, height: 1440 },
  network: { listen_port: 9002, enable_upnp: false },
  encode: { bitrate_kbps: 9000, fps: 60 },
  audio: { enabled: true, bitrate_kbps: 96, blacklist: [] },
};

type DragMode = "move" | "n" | "s" | "e" | "w" | "ne" | "nw" | "se" | "sw";

interface DragState {
  mode: DragMode;
  /** capture rect at the start of the drag (monitor-pixel coords) */
  startCapture: HostConfig["capture"];
  /** preview-element's bounding rect at the start of the drag (CSS px) */
  pvRect: DOMRect;
  startClientX: number;
  startClientY: number;
  monitorW: number;
  monitorH: number;
}

function clamp(v: number, lo: number, hi: number): number {
  return Math.max(lo, Math.min(hi, v));
}

export default function Host() {
  const {
    hostState,
    hostError,
    hostConfig,
    hostAddresses: addresses,
    setHostConfig,
  } = useHosting();
  const live = hostState === "broadcasting";
  const starting = hostState === "starting";
  const stopping = hostState === "stopping";
  const interactive = !live && !starting && !stopping;

  const cfg = hostConfig ?? DEFAULT_CFG;
  const [copied, setCopied] = useState(false);
  const [monitors, setMonitors] = useState<MonitorInfo[]>([]);
  const [screenshot, setScreenshot] = useState<MonitorScreenshot | null>(null);
  const [shotLoading, setShotLoading] = useState(false);
  const [shotError, setShotError] = useState<string | null>(null);

  const monitor =
    monitors.find((m) => m.index === cfg.capture.output_index) ?? monitors[0];

  // Debounced auto-save: edits update local state immediately, then a
  // background timer flushes to disk.
  const saveTimer = useRef<number | null>(null);
  const updateCfg = (next: HostConfig) => {
    setHostConfig(next);
    if (saveTimer.current !== null) window.clearTimeout(saveTimer.current);
    saveTimer.current = window.setTimeout(() => {
      configSaveHost(next).catch((e) =>
        console.error("config_save_host failed", e),
      );
    }, SAVE_DEBOUNCE_MS);
  };

  const updateCapture = (patch: Partial<HostConfig["capture"]>) => {
    updateCfg({ ...cfg, capture: { ...cfg.capture, ...patch } });
  };

  useEffect(() => {
    return () => {
      if (saveTimer.current !== null) window.clearTimeout(saveTimer.current);
    };
  }, []);

  // Load monitor list once.
  useEffect(() => {
    void (async () => {
      try {
        const list = await monitorsList();
        setMonitors(list);
        // If the saved output_index isn't present (monitor unplugged
        // since the last run), fall back to the first one.
        if (
          list.length > 0 &&
          !list.find((m) => m.index === cfg.capture.output_index)
        ) {
          updateCapture({ output_index: list[0].index });
        }
      } catch (e) {
        console.error("monitors_list failed", e);
      }
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Live monitor preview: poll the screenshot endpoint at 1 Hz while
  // the page is mounted. Skips overlapping calls so a slow capture
  // can't pile up requests, and stops polling while the host
  // pipeline is broadcasting (DXGI Desktop Duplication is exclusive
  // to the running encoder; GDI is fine but the preview is moot
  // when the marquee is locked anyway). Refreshes immediately when
  // the user switches monitors.
  useEffect(() => {
    if (monitor === undefined) return;
    let cancelled = false;
    let inFlight = false;
    let firstShot = true;

    const tick = async () => {
      if (cancelled || inFlight) return;
      inFlight = true;
      if (firstShot) setShotLoading(true);
      try {
        const shot = await monitorScreenshot(monitor.index);
        if (!cancelled) {
          setScreenshot(shot);
          setShotError(null);
        }
      } catch (e) {
        if (!cancelled) setShotError(String(e));
      } finally {
        if (!cancelled && firstShot) setShotLoading(false);
        firstShot = false;
        inFlight = false;
      }
    };

    void tick();
    const intervalMs = live ? 0 : 1000;
    const id = intervalMs > 0 ? window.setInterval(tick, intervalMs) : null;
    return () => {
      cancelled = true;
      if (id !== null) window.clearInterval(id);
    };
  }, [monitor?.index, live]);

  // Visual-only telemetry while broadcasting.
  const fps = useRolling(() => 58 + Math.random() * 3);
  const mbps = useRolling(() => 8.4 + Math.random() * 1.4);
  const lag = useRolling(() => 14 + Math.random() * 8);
  const lagMax = lag.length ? Math.max(...lag) : 0;

  const primary = addresses?.primary ?? `0.0.0.0:${cfg.network.listen_port}`;
  const tailscale = addresses?.addresses.find((a) => a.kind === "tailscale");
  const lan = addresses?.addresses.find((a) => a.kind === "lan");

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(primary);
    } catch {
      /* clipboard unavailable */
    }
    setCopied(true);
    setTimeout(() => setCopied(false), 1300);
  };

  const onStart = async () => {
    try {
      await hostStart();
    } catch (e) {
      console.error("host_start failed", e);
    }
  };
  const onStop = async () => {
    try {
      await hostStop();
    } catch (e) {
      console.error("host_stop failed", e);
    }
  };

  // ------- Marquee interaction -------------------------------------

  const pvRef = useRef<HTMLDivElement | null>(null);
  const dragRef = useRef<DragState | null>(null);

  const startDrag = (e: React.PointerEvent, mode: DragMode) => {
    if (!interactive || !monitor || !pvRef.current) return;
    e.stopPropagation();
    e.preventDefault();
    e.currentTarget.setPointerCapture(e.pointerId);
    dragRef.current = {
      mode,
      startCapture: { ...cfg.capture },
      pvRect: pvRef.current.getBoundingClientRect(),
      startClientX: e.clientX,
      startClientY: e.clientY,
      monitorW: monitor.width,
      monitorH: monitor.height,
    };
  };

  const onPointerMove = (e: React.PointerEvent) => {
    const drag = dragRef.current;
    if (!drag) return;
    const dx = e.clientX - drag.startClientX;
    const dy = e.clientY - drag.startClientY;
    // Map preview-pixel deltas to monitor-pixel deltas.
    const sx = drag.monitorW / drag.pvRect.width;
    const sy = drag.monitorH / drag.pvRect.height;
    const dxM = Math.round(dx * sx);
    const dyM = Math.round(dy * sy);

    let { x, y, width, height } = drag.startCapture;
    switch (drag.mode) {
      case "move":
        x += dxM;
        y += dyM;
        break;
      case "n":
        y += dyM;
        height -= dyM;
        break;
      case "s":
        height += dyM;
        break;
      case "w":
        x += dxM;
        width -= dxM;
        break;
      case "e":
        width += dxM;
        break;
      case "nw":
        x += dxM;
        y += dyM;
        width -= dxM;
        height -= dyM;
        break;
      case "ne":
        y += dyM;
        width += dxM;
        height -= dyM;
        break;
      case "sw":
        x += dxM;
        width -= dxM;
        height += dyM;
        break;
      case "se":
        width += dxM;
        height += dyM;
        break;
    }
    // Resize handles can produce negative widths if the user drags
    // past the opposite edge; clamp to a minimum so the rect stays
    // valid and the encoder doesn't get a 0×0.
    width = Math.max(MIN_CAPTURE_SIZE, width);
    height = Math.max(MIN_CAPTURE_SIZE, height);
    x = clamp(x, 0, drag.monitorW - width);
    y = clamp(y, 0, drag.monitorH - height);
    width = Math.min(width, drag.monitorW - x);
    height = Math.min(height, drag.monitorH - y);

    updateCapture({ x, y, width, height });
  };

  const onPointerUp = (e: React.PointerEvent) => {
    if (e.currentTarget.hasPointerCapture(e.pointerId)) {
      e.currentTarget.releasePointerCapture(e.pointerId);
    }
    dragRef.current = null;
  };

  // Map monitor-pixel rect → percentages of the preview area.
  const cropX = monitor ? (cfg.capture.x / monitor.width) * 100 : 0;
  const cropY = monitor ? (cfg.capture.y / monitor.height) * 100 : 0;
  const cropW = monitor
    ? (cfg.capture.width / monitor.width) * 100
    : 100;
  const cropH = monitor
    ? (cfg.capture.height / monitor.height) * 100
    : 100;

  const monitorTagLabel = useMemo(() => {
    if (!monitor) return "monitor —";
    return monitor.name;
  }, [monitor]);

  return (
    <div className="screen">
      <div className="hostpage">
      <div className="pgheader">
        <div>
          <div className="eyebrow">
            <span
              className={`eyebrow__dot ${live ? "eyebrow__dot--host" : "eyebrow__dot--off"}`}
            />
            {live
              ? "BROADCASTING"
              : starting
                ? "STARTING…"
                : stopping
                  ? "STOPPING…"
                  : "IDLE — CONFIGURE"}
          </div>
          <h2 className="pgheader__title">
            {live ? "Streaming live" : "Host a stream"}
          </h2>
          <div className="pgheader__sub">
            {live
              ? "One client may connect using the address below. Fast-forward jitter handling is on the receiving end — your encoder ships frames as fast as NVENC can produce them."
              : hostError
                ? `Last error: ${hostError}`
                : "Pick a region, encode it on NVENC, share the address."}
          </div>
        </div>
        <div>
          {live ? (
            <Btn kind="danger" icon={IcStop} onClick={onStop} disabled={stopping}>
              Stop streaming
            </Btn>
          ) : (
            <Btn kind="host" icon={IcBroadcast} onClick={onStart} disabled={starting}>
              Start streaming
            </Btn>
          )}
        </div>
      </div>

      <div className="hostgrid">
        <Card>
          <div className="cardhd">
            <span className="cardhd__t">
              <IcMonitor size={13} /> Capture region
            </span>
            <div className="cardhd__r">
              {monitors.length > 0 ? (
                <select
                  className="monitor-select"
                  value={cfg.capture.output_index}
                  onChange={(e) =>
                    updateCapture({ output_index: +e.target.value })
                  }
                  disabled={!interactive}
                  aria-label="Capture monitor"
                >
                  {monitors.map((m) => (
                    <option key={m.index} value={m.index}>
                      {m.name}
                      {m.primary ? " · primary" : ""}
                    </option>
                  ))}
                </select>
              ) : (
                <Tag>{monitorTagLabel}</Tag>
              )}
              <Tag>
                {cfg.capture.width}×{cfg.capture.height}
              </Tag>
              <Tag>{cfg.encode.fps} fps</Tag>
            </div>
          </div>
          <div className="pv" ref={pvRef}>
            {screenshot ? (
              <img
                src={screenshot.dataUrl}
                alt="Monitor preview"
                className="pv__shot"
                draggable={false}
              />
            ) : (
              <div className="pv__placeholder">
                {shotLoading
                  ? "Capturing screenshot…"
                  : shotError
                    ? `Screenshot failed: ${shotError}`
                    : "No preview available"}
              </div>
            )}
            <div
              className={`crop ${live ? "crop--live" : ""} ${interactive ? "crop--interactive" : ""}`}
              style={{
                left: cropX + "%",
                top: cropY + "%",
                width: cropW + "%",
                height: cropH + "%",
                color: live ? "var(--live)" : "var(--host)",
              }}
              onPointerDown={(e) => startDrag(e, "move")}
              onPointerMove={onPointerMove}
              onPointerUp={onPointerUp}
              onPointerCancel={onPointerUp}
            >
              <div className="crop__lbl">
                <span>
                  {cfg.capture.width} × {cfg.capture.height}
                </span>
                <span>
                  @ {cfg.capture.x},{cfg.capture.y}
                </span>
              </div>
              {live && <div className="crop__rec">● REC</div>}
              {live && <div className="pv__sweep" />}
              {interactive && (
                <>
                  {(["nw", "n", "ne", "e", "se", "s", "sw", "w"] as const).map(
                    (h) => (
                      <span
                        key={h}
                        className={`crop__handle crop__handle--${h}`}
                        onPointerDown={(e) => startDrag(e, h)}
                        onPointerMove={onPointerMove}
                        onPointerUp={onPointerUp}
                        onPointerCancel={onPointerUp}
                      />
                    ),
                  )}
                </>
              )}
            </div>
          </div>
          <div className="host__regrow">
            <Field label="X" mono>
              <input
                type="number"
                value={cfg.capture.x}
                onChange={(e) =>
                  updateCapture({ x: +e.target.value || 0 })
                }
                disabled={!interactive}
              />
            </Field>
            <Field label="Y" mono>
              <input
                type="number"
                value={cfg.capture.y}
                onChange={(e) =>
                  updateCapture({ y: +e.target.value || 0 })
                }
                disabled={!interactive}
              />
            </Field>
            <Field label="Width" mono>
              <input
                type="number"
                value={cfg.capture.width}
                onChange={(e) =>
                  updateCapture({ width: +e.target.value || 0 })
                }
                disabled={!interactive}
              />
            </Field>
            <Field label="Height" mono>
              <input
                type="number"
                value={cfg.capture.height}
                onChange={(e) =>
                  updateCapture({ height: +e.target.value || 0 })
                }
                disabled={!interactive}
              />
            </Field>
          </div>
        </Card>

        <div className="host__side">
          <Card>
            <div className="cardhd">
              <span className="cardhd__t">Share address</span>
              <Tag kind={live ? "live" : "def"}>
                {live ? "listening" : "offline"}
              </Tag>
            </div>
            <div className="share__addr">
              <code>{primary}</code>
              <button className="share__copy" onClick={copy}>
                {copied ? <IcCheck size={13} /> : <IcCopy size={13} />}
                <span>{copied ? "Copied" : "Copy"}</span>
              </button>
            </div>
            <div className="share__hint">
              Hand this string to your friend over chat. They paste it on the
              Connect screen — first packet teaches the host who they are.
            </div>
            <div className="share__row">
              <span className="share__rk">Tailscale</span>
              <span className="share__rv">
                {tailscale ? `${tailscale.ip}:${tailscale.port}` : "—"}
              </span>
            </div>
            <div className="share__row">
              <span className="share__rk">LAN</span>
              <span className="share__rv">
                {lan ? `${lan.ip}:${lan.port}` : "—"}
              </span>
            </div>
            <div className="share__row">
              <span className="share__rk">UPnP</span>
              <span className="share__rv">
                {addresses?.upnpEnabled ? (
                  "forwarded"
                ) : (
                  <span className="warn">not forwarded</span>
                )}
              </span>
            </div>
          </Card>

          <Card>
            <div className="cardhd">
              <span className="cardhd__t">
                <IcZap size={13} /> Encode
              </span>
              <Tag>NVENC h264</Tag>
            </div>
            <div className="encgrid">
              <Field label="Bitrate" mono suffix="kbps">
                <input
                  type="number"
                  value={cfg.encode.bitrate_kbps}
                  onChange={(e) =>
                    updateCfg({
                      ...cfg,
                      encode: {
                        ...cfg.encode,
                        bitrate_kbps: +e.target.value || 0,
                      },
                    })
                  }
                />
              </Field>
              <Field label="FPS" mono>
                <input
                  type="number"
                  value={cfg.encode.fps}
                  onChange={(e) =>
                    updateCfg({
                      ...cfg,
                      encode: {
                        ...cfg.encode,
                        fps: +e.target.value || 0,
                      },
                    })
                  }
                />
              </Field>
            </div>
            <div className="chips">
              <span className="chip chip--on">p4</span>
              <span className="chip chip--on">tune=ll</span>
              <span className="chip chip--on">zerolatency</span>
              <span className="chip chip--on">CBR</span>
              <span className="chip">multipass=qres</span>
              <span className="chip">no B-frames</span>
              <span className="chip chip--on">FEC 10%</span>
            </div>
          </Card>

          <Card>
            <div className="cardhd">
              <span className="cardhd__t">
                <IcGamepad size={13} /> Connected client
              </span>
              <Tag kind="def">none</Tag>
            </div>
            <div className="peer__empty">
              {live ? "Waiting for first packet…" : "Stream is offline"}
              <span className="mono">
                Host learns peer address from inbound UDP
              </span>
            </div>
          </Card>
        </div>

        <Card className="tel">
          <div className="cardhd">
            <span className="cardhd__t">
              <IcSpark size={13} /> Live telemetry
            </span>
            <span
              className="mono"
              style={{ fontSize: 11, color: "var(--fg-3)" }}
            >
              {live ? "1s window · last 30s" : "paused"}
            </span>
          </div>
          <div className="telgrid">
            <div className="telcell">
              <Stat
                label="FRAMES / SEC"
                value={live ? fps[fps.length - 1].toFixed(1) : "—"}
                unit={`/ ${cfg.encode.fps}`}
                accent="live"
              />
              <Sparkline
                data={live ? fps : Array(40).fill(0)}
                color="var(--live)"
              />
            </div>
            <div className="telcell">
              <Stat
                label="BITRATE"
                value={live ? mbps[mbps.length - 1].toFixed(2) : "—"}
                unit="Mbps"
                accent="host"
              />
              <Sparkline
                data={live ? mbps : Array(40).fill(0)}
                color="var(--host)"
              />
            </div>
            <div className="telcell">
              <Stat
                label="LAG (p50)"
                value={live ? Math.round(lag[lag.length - 1]) : "—"}
                unit="ms"
                hint={
                  live
                    ? `p95 ${Math.round(lagMax * 1.05)} · p99 ${Math.round(lagMax * 1.3)}`
                    : ""
                }
              />
              <Sparkline
                data={live ? lag : Array(40).fill(0)}
                color="var(--fg-1)"
              />
            </div>
            <div className="telcell">
              <Stat
                label="STATUS"
                value={live ? "ON AIR" : "—"}
                hint={live ? `port ${cfg.network.listen_port}` : ""}
              />
              <div
                style={{
                  height: 36,
                  background: "var(--bg-2)",
                  borderRadius: 4,
                  marginTop: 8,
                  overflow: "hidden",
                  position: "relative",
                }}
              >
                <div
                  style={{
                    position: "absolute",
                    inset: 0,
                    background:
                      "linear-gradient(90deg, var(--live), transparent)",
                    opacity: live ? 0.4 : 0.1,
                  }}
                />
              </div>
            </div>
          </div>
          <div className="pipe">
            <div className={`pipe__node ${live ? "pipe__node--ok" : ""}`}>
              <span className="pipe__node-l">DXGI</span>
              <span className="pipe__node-s">capture · {cfg.encode.fps}Hz</span>
            </div>
            <span className="pipe__arrow">→</span>
            <div className={`pipe__node ${live ? "pipe__node--ok" : ""}`}>
              <span className="pipe__node-l">NVENC</span>
              <span className="pipe__node-s">
                h264 · {(cfg.encode.bitrate_kbps / 1000).toFixed(1)} Mbps
              </span>
            </div>
            <span className="pipe__arrow">→</span>
            <div className={`pipe__node ${live ? "pipe__node--ok" : ""}`}>
              <span className="pipe__node-l">FEC</span>
              <span className="pipe__node-s">+10%</span>
            </div>
            <span className="pipe__arrow">→</span>
            <div className={`pipe__node ${live ? "pipe__node--ok" : ""}`}>
              <span className="pipe__node-l">UDP</span>
              <span className="pipe__node-s">
                :{cfg.network.listen_port}
              </span>
            </div>
          </div>
        </Card>
      </div>
      </div>
    </div>
  );
}
