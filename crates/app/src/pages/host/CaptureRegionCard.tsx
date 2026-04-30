import { useEffect, useMemo, useRef, useState } from "react";
import { IcMonitor } from "../../components/Icons";
import { Card, Tag } from "../../components/primitives";
import { useHosting } from "../../hosting";
import {
  monitorScreenshot,
  type HostConfig,
  type MonitorInfo,
  type MonitorScreenshot,
} from "../../api";
import type { DragMode } from "./helpers";

const MIN_CAPTURE_SIZE = 64;

interface DragState {
  mode: DragMode;
  /** capture rect at the start of the drag (monitor-pixel coords) */
  startCapture: HostConfig["capture"];
  /** preview-element bounding rect at the start of the drag (CSS px) */
  pvRect: DOMRect;
  startClientX: number;
  startClientY: number;
  monitorW: number;
  monitorH: number;
}

function clamp(v: number, lo: number, hi: number): number {
  return Math.max(lo, Math.min(hi, v));
}

interface Props {
  cfg: HostConfig;
  monitors: MonitorInfo[];
  monitor: MonitorInfo | undefined;
  updateCfg: (next: HostConfig) => void;
}

/**
 * Capture-region picker. Owns the live monitor preview, the
 * draggable + resizable marquee, the floating bottom-right pill
 * that mirrors X/Y/W/H as numbers, and the monitor dropdown in the
 * card header.
 */
export default function CaptureRegionCard({
  cfg,
  monitors,
  monitor,
  updateCfg,
}: Props) {
  const { hostState } = useHosting();
  const live = hostState === "broadcasting";
  const starting = hostState === "starting";
  const stopping = hostState === "stopping";
  const interactive = !live && !starting && !stopping;

  const [screenshot, setScreenshot] = useState<MonitorScreenshot | null>(null);
  const [shotLoading, setShotLoading] = useState(false);
  const [shotError, setShotError] = useState<string | null>(null);

  const updateCapture = (patch: Partial<HostConfig["capture"]>) => {
    updateCfg({ ...cfg, capture: { ...cfg.capture, ...patch } });
  };

  // Live monitor preview: poll the screenshot endpoint at 1 Hz
  // while the page is mounted. Stops while broadcasting (DXGI
  // Desktop Duplication is exclusive to the running encoder).
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
    const sx = drag.monitorW / drag.pvRect.width;
    const sy = drag.monitorH / drag.pvRect.height;
    let dxM = Math.round(dx * sx);
    let dyM = Math.round(dy * sy);

    // Shift held on a corner handle: lock to the aspect ratio captured
    // at drag start so the marquee scales diagonally. Pick whichever
    // axis the user pushed harder (in aspect-corrected terms) as the
    // dominant one, then derive the other from it.
    const isCorner =
      drag.mode === "nw" ||
      drag.mode === "ne" ||
      drag.mode === "sw" ||
      drag.mode === "se";
    if (e.shiftKey && isCorner && drag.startCapture.height > 0) {
      const aspect = drag.startCapture.width / drag.startCapture.height;
      // Sign convention: +dxM grows width on east-anchored corners,
      // shrinks it on west-anchored corners (and same for dyM/south).
      const wDir = drag.mode === "ne" || drag.mode === "se" ? 1 : -1;
      const hDir = drag.mode === "sw" || drag.mode === "se" ? 1 : -1;
      const dW = wDir * dxM;
      const dH = hDir * dyM;
      if (Math.abs(dW) > Math.abs(dH * aspect)) {
        dyM = hDir * Math.round(dW / aspect);
      } else {
        dxM = wDir * Math.round(dH * aspect);
      }
    }

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
  const cropW = monitor ? (cfg.capture.width / monitor.width) * 100 : 100;
  const cropH = monitor ? (cfg.capture.height / monitor.height) * 100 : 100;

  const monitorTagLabel = useMemo(
    () => monitor?.name ?? "monitor —",
    [monitor],
  );

  return (
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
        <div
          className="pv__inputs"
          // pointerdown on the pill (or any of its children) would
          // otherwise bubble to the .crop drag handler and start a
          // marquee move when the user clicks an input field.
          onPointerDown={(e) => e.stopPropagation()}
        >
          <label className="pv__inputs-field">
            <span>X</span>
            <input
              type="number"
              value={cfg.capture.x}
              onChange={(e) =>
                updateCapture({ x: +e.target.value || 0 })
              }
              disabled={!interactive}
            />
          </label>
          <label className="pv__inputs-field">
            <span>Y</span>
            <input
              type="number"
              value={cfg.capture.y}
              onChange={(e) =>
                updateCapture({ y: +e.target.value || 0 })
              }
              disabled={!interactive}
            />
          </label>
          <label className="pv__inputs-field">
            <span>W</span>
            <input
              type="number"
              value={cfg.capture.width}
              onChange={(e) =>
                updateCapture({ width: +e.target.value || 0 })
              }
              disabled={!interactive}
            />
          </label>
          <label className="pv__inputs-field">
            <span>H</span>
            <input
              type="number"
              value={cfg.capture.height}
              onChange={(e) =>
                updateCapture({ height: +e.target.value || 0 })
              }
              disabled={!interactive}
            />
          </label>
        </div>
      </div>
    </Card>
  );
}
