import { useNavigate } from "react-router-dom";
import { Card, Tag } from "../components/primitives";
import {
  IcBroadcast,
  IcChevron,
  IcConnect,
} from "../components/Icons";
import { useHosting } from "../hosting";
import { recentsAdd, recentsClear, type ProbeStatus, type SystemProbe } from "../api";

type SystemRow = readonly [label: string, detail: string, status: ProbeStatus];

function systemRows(system: SystemProbe | null): SystemRow[] {
  if (!system) {
    return [
      ["NVENC encoder", "probing…", "mid"],
      ["NVDEC decoder", "probing…", "mid"],
      ["ViGEmBus", "probing…", "mid"],
      ["ffmpeg", "probing…", "mid"],
      ["UPnP", "probing…", "mid"],
    ];
  }
  return [
    ["NVENC encoder", system.gpuLabel || system.nvenc.detail, system.nvenc.status],
    ["NVDEC decoder", system.nvdec.detail, system.nvdec.status],
    ["ViGEmBus", system.vigem.detail, system.vigem.status],
    ["ffmpeg", system.ffmpeg.detail, system.ffmpeg.status],
    ["UPnP", system.upnp.detail, system.upnp.status],
  ];
}

function timeAgo(ms: number): string {
  const delta = Date.now() - ms;
  if (delta < 60_000) return "just now";
  if (delta < 3_600_000) return `${Math.floor(delta / 60_000)}m ago`;
  if (delta < 86_400_000) return `${Math.floor(delta / 3_600_000)}h ago`;
  return `${Math.floor(delta / 86_400_000)}d ago`;
}

export default function Home() {
  const navigate = useNavigate();
  const { system, recents, setRecents } = useHosting();
  const eyebrowText = system?.ready
    ? "READY · NVENC · ViGEmBus loaded"
    : "PROBING SYSTEM…";

  const onClickRecent = async (address: string) => {
    try {
      const updated = await recentsAdd(address);
      setRecents(updated);
    } catch (e) {
      console.error("recents_add failed", e);
    }
    navigate("/connect", { state: { prefill: address } });
  };

  const onClear = async () => {
    try {
      await recentsClear();
      setRecents([]);
    } catch (e) {
      console.error("recents_clear failed", e);
    }
  };

  return (
    <div className="screen">
      <div className="home">
        <div className="home__hero">
          <div className="eyebrow">
            <span
              className={`eyebrow__dot ${system?.ready ? "" : "eyebrow__dot--off"}`}
            />
            {eyebrowText}
          </div>
          <h1 className="home__title">
            Two players,
            <br />
            <span className="home__title--accent">one machine.</span>
          </h1>
          <p className="home__sub">
            Direct-connect, low-latency desktop streaming over UDP. No accounts,
            no friend list — run a server, share an address, plug in a controller.
          </p>
        </div>
        <div className="home__cards">
          <button
            className="rolecard rolecard--host"
            onClick={() => navigate("/host")}
          >
            <div className="rolecard__head">
              <div
                className="rolecard__icon"
                style={{ color: "var(--host)" }}
              >
                <IcBroadcast size={22} />
              </div>
              <Tag kind="host">SERVER</Tag>
            </div>
            <div className="rolecard__title">Host a stream</div>
            <div className="rolecard__desc">
              Capture a region of this PC, encode on NVENC, hand out an address.
              Your friend's controller plugs in here as a virtual Xbox 360 pad.
            </div>
            <div className="rolecard__chips">
              <span>NVENC h264</span>
              <i />
              <span>ViGEm pad</span>
              <i />
              <span>UDP :9002</span>
            </div>
            <div className="rolecard__cta">
              Start hosting <IcChevron size={14} />
            </div>
          </button>
          <button
            className="rolecard rolecard--client"
            onClick={() => navigate("/connect")}
          >
            <div className="rolecard__head">
              <div
                className="rolecard__icon"
                style={{ color: "var(--live)" }}
              >
                <IcConnect size={22} />
              </div>
              <Tag kind="live">CLIENT</Tag>
            </div>
            <div className="rolecard__title">Connect to a host</div>
            <div className="rolecard__desc">
              Paste an address. Hardware decode preferred, software fallback.
              Decoder fast-forwards backlog so jitter shows as a single jump,
              never slow-motion.
            </div>
            <div className="rolecard__chips">
              <span>h264_cuvid</span>
              <i />
              <span>250 Hz pad</span>
              <i />
              <span>FEC 10%</span>
            </div>
            <div className="rolecard__cta">
              Connect <IcChevron size={14} />
            </div>
          </button>
        </div>
        <div className="home__row">
          <Card>
            <div className="cardhd">
              <span className="cardhd__t">Recent destinations</span>
              <button
                style={{
                  background: "transparent",
                  border: 0,
                  color: "var(--fg-2)",
                  fontSize: 11,
                  cursor: "pointer",
                }}
                onClick={onClear}
              >
                Clear
              </button>
            </div>
            <div className="recents__list">
              {recents.length === 0 ? (
                <div className="peer__empty">
                  No recent destinations
                  <span className="mono">
                    They'll appear here once you connect to one
                  </span>
                </div>
              ) : (
                recents.map((r) => (
                  <button
                    key={r.address}
                    className="recent"
                    onClick={() => onClickRecent(r.address)}
                  >
                    <div className="recent__l">
                      <div className="recent__dot recent__dot--on" />
                      <div>
                        <div className="recent__name">{r.name}</div>
                        <div className="recent__addr">{r.address}</div>
                      </div>
                    </div>
                    <div className="recent__r">
                      <span className="mono">{timeAgo(r.lastUsed)}</span>
                      <IcChevron size={14} />
                    </div>
                  </button>
                ))
              )}
            </div>
          </Card>
          <Card>
            <div className="cardhd">
              <span className="cardhd__t">System check</span>
              <Tag kind={system?.ready ? "live" : "def"}>
                {system?.ready ? "all green" : "checking"}
              </Tag>
            </div>
            {systemRows(system).map(([l, v, s]) => (
              <div key={l} className="diag__row">
                <div className={`diag__dot ${s}`} />
                <div className="diag__l">{l}</div>
                <div className="diag__v">{v}</div>
              </div>
            ))}
          </Card>
        </div>
      </div>
    </div>
  );
}
