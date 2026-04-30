import { IcSpark } from "../../components/Icons";
import { Card, Sparkline, Stat } from "../../components/primitives";
import { useRolling } from "../../hooks/useRolling";
import { useHosting } from "../../hosting";
import type { HostConfig } from "../../api";

interface Props {
  cfg: HostConfig;
}

/**
 * Live-telemetry strip + pipeline diagram. Spans the full width of
 * the host grid via the `tel` class.
 *
 * Telemetry values are visual placeholders right now: the host
 * pipeline runs in-process via `runner::run_stream_blocking` but
 * doesn't yet emit `host:telemetry` events to the Tauri shell.
 * `useRolling` produces a steady fake stream so the sparklines
 * scroll while broadcasting; flipping `live=false` clears them.
 */
export default function TelemetryCard({ cfg }: Props) {
  const { hostState } = useHosting();
  const live = hostState === "broadcasting";

  const fps = useRolling(() => 58 + Math.random() * 3);
  const mbps = useRolling(() => 8.4 + Math.random() * 1.4);
  const lag = useRolling(() => 14 + Math.random() * 8);
  const lagMax = lag.length ? Math.max(...lag) : 0;

  return (
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
                background: "linear-gradient(90deg, var(--live), transparent)",
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
  );
}
