import { useLocation, useNavigate } from "react-router-dom";
import {
  IcBroadcast,
  IcConnect,
  IcHome,
  IcLogs,
  IcSettings,
} from "./Icons";
import type { ComponentType } from "react";
import { useHosting } from "../hosting";
import type { ProbeStatus, SystemProbe } from "../api";

type Item = {
  k: string;
  l: string;
  path: string;
  i: ComponentType<{ size?: number }>;
};

const ITEMS: Item[] = [
  { k: "home", l: "Home", path: "/", i: IcHome },
  { k: "host", l: "Host", path: "/host", i: IcBroadcast },
  { k: "connect", l: "Connect", path: "/connect", i: IcConnect },
  { k: "logs", l: "Logs", path: "/logs", i: IcLogs },
  { k: "settings", l: "Settings", path: "/settings", i: IcSettings },
];

type DiagRow = { label: string; status: ProbeStatus };

/** Build the four sidebar diagnostic rows from the system probe. The
 *  `null` system case (probe still pending) keeps the labels but
 *  shows `mid` dots so the UI doesn't flash colors during boot. */
function diagRows(system: SystemProbe | null): DiagRow[] {
  if (!system) {
    return [
      { label: "GPU probe…", status: "mid" },
      { label: "ViGEm probe…", status: "mid" },
      { label: "UDP/9002 probe…", status: "mid" },
      { label: "UPnP probe…", status: "mid" },
    ];
  }
  const gpuLabel =
    system.nvenc.status === "ok"
      ? `NVENC ${shortGpu(system.gpuLabel)}`
      : "NVENC unavailable";
  return [
    { label: gpuLabel, status: system.nvenc.status },
    {
      label:
        system.vigem.status === "ok"
          ? labelWithDetail("ViGEmBus", system.vigem.detail)
          : "ViGEmBus missing",
      status: system.vigem.status,
    },
    {
      label: `UDP/${currentPortLabel(system)} ${system.udpPort.detail}`,
      status: system.udpPort.status,
    },
    { label: `UPnP ${system.upnp.detail}`, status: system.upnp.status },
  ];
}

/** Trim "NVIDIA GeForce" / "AMD Radeon" prefixes so the sidebar row
 *  matches the design's compact "NVENC RTX 4070" style. */
function shortGpu(name: string): string {
  return name
    .replace(/^NVIDIA\s+(GeForce\s+)?/i, "")
    .replace(/^AMD\s+(Radeon\s+)?/i, "")
    .replace(/^Intel\(R\)\s+/i, "")
    .trim();
}

function labelWithDetail(name: string, detail: string): string {
  // ViGEm registry returns "Virtual Gamepad Emulation Bus"; pull the
  // bare version number off the end if present, otherwise show "ok".
  const m = detail.match(/(\d+\.\d+(\.\d+)?)/);
  return m ? `${name} ${m[1]}` : `${name} ok`;
}

function currentPortLabel(system: SystemProbe): number {
  // The probe always tries 9002 — the host config defaults to that
  // port too. The UI rounds back to the canonical default rather
  // than threading the live host config into every render.
  void system;
  return 9002;
}

const dotClass = (s: ProbeStatus) => (s === "ok" ? "dotok" : "dotmid");

export default function Sidebar() {
  const navigate = useNavigate();
  const { pathname } = useLocation();
  const { hosting, system } = useHosting();

  const isActive = (path: string) =>
    path === "/" ? pathname === "/" : pathname.startsWith(path);
  const rows = diagRows(system);

  return (
    <div className="side">
      <div className="side__sect">Workspace</div>
      <div className="side__nav">
        {ITEMS.map((it) => {
          const on = isActive(it.path);
          const Icon = it.i;
          const showHostDot = it.k === "host" && hosting;
          return (
            <button
              key={it.k}
              className={`side__item ${on ? "side__item--on" : ""}`}
              onClick={() => navigate(it.path)}
            >
              <span className="side__item-ic">
                <Icon size={15} />
              </span>
              <span>{it.l}</span>
              {showHostDot && (
                <span
                  className="side__item-r"
                  style={{ color: "var(--host)" }}
                >
                  ●
                </span>
              )}
            </button>
          );
        })}
      </div>
      <div className="side__foot">
        <div className="side__sect" style={{ padding: "0 0 8px" }}>
          System
        </div>
        <div className="side__diag">
          {rows.map((r) => (
            <div className="side__drow" key={r.label}>
              <span className={dotClass(r.status)} />
              {r.label}
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
