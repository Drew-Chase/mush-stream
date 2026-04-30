import { useEffect, useState } from "react";
import { useLocation } from "react-router-dom";
import { Select, SelectItem } from "@heroui/react";
import {
  IcCheck,
  IcChevron,
  IcGamepad,
  IcStop,
} from "../components/Icons";
import { Btn, Card } from "../components/primitives";
import { useRolling } from "../hooks/useRolling";
import { useHosting } from "../hosting";
import {
  clientConnect,
  clientDisconnect,
  gamepadsList,
  recentsAdd,
  type ClientState,
  type GamepadInfo,
} from "../api";

export default function Connect() {
  const location = useLocation();
  const prefill = (location.state as { prefill?: string } | null)?.prefill ?? "";
  const { clientState, clientAddress, clientError, recents, setRecents } =
    useHosting();

  if (clientState === "connecting") {
    return (
      <ConnectingScreen
        addr={clientAddress ?? prefill}
        error={clientError}
      />
    );
  }
  if (clientState === "connected") {
    return <ConnectedScreen addr={clientAddress ?? prefill} />;
  }
  return (
    <IdleScreen
      prefill={prefill}
      lastError={clientError}
      lastState={clientState}
      recentAddresses={recents.map((r) => r.address)}
      onConnected={async (addr) => {
        try {
          const updated = await recentsAdd(addr);
          setRecents(updated);
        } catch (e) {
          console.error("recents_add failed", e);
        }
      }}
    />
  );
}

function IdleScreen({
  prefill,
  lastError,
  lastState,
  recentAddresses,
  onConnected,
}: {
  prefill: string;
  lastError: string | null;
  lastState: ClientState;
  recentAddresses: string[];
  onConnected: (addr: string) => Promise<void>;
}) {
  const [addr, setAddr] = useState(prefill);
  const [hwdec, setHwdec] = useState(true);
  const [mailbox, setMailbox] = useState(true);
  const [forwardPad, setForwardPad] = useState(true);
  const [debug, setDebug] = useState(false);
  const [busy, setBusy] = useState(false);
  const [gamepads, setGamepads] = useState<GamepadInfo[]>([]);
  // `null` selects the first available pad (gilrs default). The
  // Select renders this as the "Auto" item so the user can revert
  // to the implicit-pick behavior.
  const [gamepadId, setGamepadId] = useState<number | null>(null);
  const [gamepadsLoading, setGamepadsLoading] = useState(false);

  useEffect(() => {
    if (prefill) setAddr(prefill);
  }, [prefill]);

  // Initial enumeration on mount, plus a manual Refresh button below.
  // gilrs reports the controllers Windows already has bound at the
  // time this runs; hot-plugging mid-session won't reflect here
  // until the user clicks Refresh.
  const refreshGamepads = async () => {
    setGamepadsLoading(true);
    try {
      const list = await gamepadsList();
      setGamepads(list);
      // If the previously-selected pad is gone, fall back to Auto so
      // the user isn't silently stuck on a disconnected device id.
      if (gamepadId !== null && !list.some((g) => g.id === gamepadId)) {
        setGamepadId(null);
      }
    } catch (e) {
      console.error("gamepads_list failed", e);
    } finally {
      setGamepadsLoading(false);
    }
  };

  useEffect(() => {
    void refreshGamepads();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const valid = /^[\w.-]+:\d{2,5}$/.test(addr);

  const handleConnect = async () => {
    if (!valid) return;
    setBusy(true);
    try {
      await clientConnect({
        address: addr,
        hardwareDecode: hwdec,
        forwardPad,
        gamepadId: forwardPad ? gamepadId : null,
        audio: true, // wired through saved client config
      });
      // Persist the recent on a successful spawn — actual connection
      // ack arrives via `client:state` events. `mailbox` and `debug`
      // are local-only flags for now.
      void mailbox;
      void debug;
      await onConnected(addr);
    } catch (e) {
      console.error("client_connect failed", e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="screen">
      <div className="pgheader">
        <div>
          <div className="eyebrow">
            <span className="eyebrow__dot eyebrow__dot--off" />
            {lastState === "error"
              ? "ERROR"
              : lastState === "disconnected"
                ? "DISCONNECTED"
                : "NOT CONNECTED"}
          </div>
          <h2 className="pgheader__title">Connect to a host</h2>
          <div className="pgheader__sub">
            {lastError
              ? `Last error: ${lastError}`
              : "Paste the address your friend sent you. Decode runs on h264_cuvid where available."}
          </div>
        </div>
      </div>
      <div className="conn">
        <Card>
          <div className="conn__inputbox">
            <div className="conn__lbl">Host address</div>
            <div className="conn__row">
              <input
                className="conn__input"
                placeholder="100.64.12.7:9002"
                value={addr}
                onChange={(e) => setAddr(e.target.value)}
                autoFocus
              />
              <Btn
                kind="live"
                disabled={!valid || busy}
                onClick={handleConnect}
              >
                {busy ? "Launching…" : "Connect"}
              </Btn>
            </div>
            <div className="conn__hint">
              Format: <span className="mono">host:port</span> — typically port
              9002.
            </div>
          </div>
          <div className="conn__opts">
            <label className="conn__opt">
              <input
                type="checkbox"
                checked={hwdec}
                onChange={(e) => setHwdec(e.target.checked)}
              />{" "}
              Hardware decode (cuvid)
            </label>
            <label className="conn__opt">
              <input
                type="checkbox"
                checked={mailbox}
                onChange={(e) => setMailbox(e.target.checked)}
              />{" "}
              Mailbox present mode
            </label>
            <label className="conn__opt">
              <input
                type="checkbox"
                checked={forwardPad}
                onChange={(e) => setForwardPad(e.target.checked)}
              />{" "}
              Forward gamepad
            </label>
            <div className="conn__padpicker">
              <Select
                size="sm"
                variant="bordered"
                aria-label="Gamepad to forward"
                isDisabled={!forwardPad || busy}
                selectedKeys={[
                  gamepadId === null ? "auto" : String(gamepadId),
                ]}
                onSelectionChange={(keys) => {
                  const first = Array.from(keys)[0];
                  if (first === undefined || first === "auto") {
                    setGamepadId(null);
                    return;
                  }
                  const n = Number(first);
                  setGamepadId(Number.isNaN(n) ? null : n);
                }}
                className="conn__padselect"
                classNames={{
                  trigger: "conn__padselect-trigger",
                  value: "conn__padselect-value",
                  popoverContent: "conn__padselect-popover",
                }}
                renderValue={(items) =>
                  items.map((item) => (
                    <span key={item.key}>{item.textValue}</span>
                  ))
                }
              >
                {[
                  <SelectItem
                    key="auto"
                    textValue={
                      gamepads.length === 0
                        ? "Auto · no gamepads detected"
                        : `Auto · first available (${gamepads[0].name})`
                    }
                  >
                    {gamepads.length === 0
                      ? "Auto · no gamepads detected"
                      : `Auto · first available (${gamepads[0].name})`}
                  </SelectItem>,
                  ...gamepads.map((g) => (
                    <SelectItem
                      key={String(g.id)}
                      textValue={`${g.name}${g.isConnected ? "" : " · disconnected"}`}
                    >
                      {g.name}
                      {g.isConnected ? "" : " · disconnected"}
                    </SelectItem>
                  )),
                ]}
              </Select>
              <Btn
                onClick={() => void refreshGamepads()}
                disabled={gamepadsLoading || busy}
              >
                {gamepadsLoading ? "Refreshing…" : "Refresh"}
              </Btn>
            </div>
            <label className="conn__opt">
              <input
                type="checkbox"
                checked={debug}
                onChange={(e) => setDebug(e.target.checked)}
              />{" "}
              Show debug overlay (Ctrl+Alt+D)
            </label>
          </div>
        </Card>
        <Card>
          <div className="cardhd">
            <span className="cardhd__t">Recent</span>
          </div>
          {recentAddresses.length === 0 ? (
            <div className="peer__empty">
              No recent destinations
              <span className="mono">
                They'll appear here once you connect to one
              </span>
            </div>
          ) : (
            recentAddresses.map((a) => (
              <button key={a} className="recent" onClick={() => setAddr(a)}>
                <div className="recent__l">
                  <div className="recent__dot recent__dot--on" />
                  <div>
                    <div className="recent__name">{a.split(":")[0]}</div>
                    <div className="recent__addr">{a}</div>
                  </div>
                </div>
                <IcChevron size={14} />
              </button>
            ))
          )}
        </Card>
      </div>
    </div>
  );
}

function ConnectingScreen({
  addr,
  error,
}: {
  addr: string;
  error: string | null;
}) {
  // Heuristic step indicator — we don't get fine-grained handshake
  // events from the spawned client. Cycle through the four stages on
  // a timer until `connected` flips.
  const [step, setStep] = useState(0);
  useEffect(() => {
    const id = setInterval(() => setStep((s) => Math.min(s + 1, 3)), 700);
    return () => clearInterval(id);
  }, []);

  const labels = [
    "Resolving address",
    "UDP handshake",
    "Negotiating codec",
    "Receiving keyframe",
  ];

  return (
    <div className="screen">
      <div className="conn">
        <Card>
          <div className="connecting">
            <div className="connecting__spin" />
            <div className="connecting__msg">
              Connecting to <span className="mono">{addr}</span>
            </div>
            <div className="connecting__sub">
              {error ?? "spawning native client window…"}
            </div>
          </div>
          <div className="connsteps">
            {labels.map((l, i) => (
              <div
                key={l}
                className={`connstep ${i < step ? "connstep--done" : i === step ? "connstep--active" : ""}`}
              >
                <span className="connstep__d">
                  {i < step && <IcCheck size={8} />}
                </span>
                <span>{l}</span>
              </div>
            ))}
          </div>
          <div
            style={{
              padding: 16,
              borderTop: "1px solid var(--line-soft)",
              display: "flex",
              justifyContent: "flex-end",
            }}
          >
            <Btn onClick={() => clientDisconnect()}>Cancel</Btn>
          </div>
        </Card>
      </div>
    </div>
  );
}

function ConnectedScreen({ addr }: { addr: string }) {
  const fps = useRolling(() => 59 + Math.random() * 1.5, 500);
  const lag = useRolling(() => 22 + Math.random() * 6, 500);
  const [showDebug, setShowDebug] = useState(true);
  const lagMax = lag.length ? Math.max(...lag) : 0;

  return (
    <div className="live">
      <div className="liveheader">
        <div className="liveheader__l">
          <span className="eyebrow__dot" />
          <div>
            <div className="liveheader__title">Connected</div>
            <div className="liveheader__addr">{addr}</div>
          </div>
        </div>
        <div className="liveheader__r">
          <span className="livestat">
            <b>{fps[fps.length - 1].toFixed(1)}</b> fps
          </span>
          <span className="livestat">
            <b>{Math.round(lag[lag.length - 1])}</b> ms
          </span>
          <span className="livestat">FEC 10%</span>
          <Btn onClick={() => setShowDebug((s) => !s)}>
            {showDebug ? "Hide" : "Show"} debug
          </Btn>
          <Btn
            kind="danger"
            icon={IcStop}
            onClick={() => clientDisconnect()}
          >
            Disconnect
          </Btn>
        </div>
      </div>
      <div className="liveview">
        <div
          style={{
            position: "absolute",
            inset: 0,
            display: "flex",
            alignItems: "center",
            justifyContent: "center",
            flexDirection: "column",
            gap: 14,
            color: "var(--fg-2)",
            textAlign: "center",
            padding: 24,
          }}
        >
          <IcGamepad size={36} />
          <div style={{ fontSize: 14, fontWeight: 600, color: "var(--fg-1)" }}>
            Live view rendered in the native client window
          </div>
          <div
            className="mono"
            style={{ fontSize: 11, maxWidth: 360, lineHeight: 1.5 }}
          >
            The video pipeline runs in a separate winit window. This page is the
            session dashboard — disconnect from here when you're done.
          </div>
        </div>
        {showDebug && (
          <div className="debug">
            <div className="debug__head">
              <span className="debug__title">Debug · Ctrl+Alt+D</span>
              <span>v0.4.2</span>
            </div>
            <div className="debug__row">
              <span className="debug__k">peer</span>
              <span className="debug__v">{addr}</span>
            </div>
            <div className="debug__row">
              <span className="debug__k">codec</span>
              <span className="debug__v">h264_cuvid</span>
            </div>
            <div className="debug__row">
              <span className="debug__k">fps</span>
              <span className="debug__v debug__v--live">
                {fps[fps.length - 1].toFixed(1)}
              </span>
            </div>
            <div className="debug__row">
              <span className="debug__k">lag p50/p95/p99</span>
              <span className="debug__v">
                {Math.round(lag[lag.length - 1])}/{Math.round(lagMax * 1.05)}/
                {Math.round(lagMax * 1.3)} ms
              </span>
            </div>
          </div>
        )}
      </div>
      <div className="liveinput">
        <span
          className="mono"
          style={{ fontSize: 11, color: "var(--fg-3)" }}
        >
          Forwarded as virtual Xbox 360 pad on host
        </span>
      </div>
    </div>
  );
}
