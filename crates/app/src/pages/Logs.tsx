import { useEffect, useRef, useState } from "react";
import { Btn, Card } from "../components/primitives";
import { useHosting } from "../hosting";

const LEVEL_COLORS: Record<string, string> = {
  ERROR: "var(--crit)",
  WARN: "var(--warn)",
  INFO: "var(--live)",
  DEBUG: "var(--fg-2)",
  TRACE: "var(--fg-3)",
};

/** Pull just the time portion off an ISO timestamp for the log column. */
function shortTime(iso: string): string {
  const m = iso.match(/T(\d{2}:\d{2}:\d{2}(?:\.\d+)?)/);
  return m ? m[1].slice(0, 12) : iso.slice(11, 23);
}

export default function Logs() {
  const { logs } = useHosting();
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const [stickyBottom, setStickyBottom] = useState(true);

  // Auto-scroll: only follow the tail when the user is already at the
  // bottom. If they scrolled up, leave the position alone.
  useEffect(() => {
    if (!stickyBottom) return;
    const el = scrollRef.current;
    if (!el) return;
    el.scrollTop = el.scrollHeight;
  }, [logs, stickyBottom]);

  const onScroll = (e: React.UIEvent<HTMLDivElement>) => {
    const el = e.currentTarget;
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
    setStickyBottom(atBottom);
  };

  const onCopyAll = async () => {
    const text = logs
      .map(
        (l) =>
          `${shortTime(l.ts)} ${l.level.padEnd(5)} ${l.target.padEnd(12)} ${l.message}`,
      )
      .join("\n");
    try {
      await navigator.clipboard.writeText(text);
    } catch (e) {
      console.error("clipboard write failed", e);
    }
  };

  return (
    <div className="screen">
      <div className="pgheader">
        <div>
          <div className="eyebrow">DIAGNOSTICS</div>
          <h2 className="pgheader__title">Logs</h2>
          <div className="pgheader__sub">
            Rolling buffer · last 1024 lines · stored in memory only.
          </div>
        </div>
        <div style={{ display: "flex", gap: 8 }}>
          <Btn onClick={onCopyAll}>Copy all</Btn>
          <Btn
            title="Save to disk is not yet wired up"
            onClick={() => {
              /* TODO: logs_save command */
            }}
          >
            Save…
          </Btn>
        </div>
      </div>
      <Card>
        <div
          ref={scrollRef}
          onScroll={onScroll}
          style={{
            padding: 14,
            fontFamily: "var(--mono)",
            fontSize: 12,
            lineHeight: 1.7,
            maxHeight: "calc(100vh - 220px)",
            overflowY: "auto",
          }}
        >
          {logs.length === 0 ? (
            <div style={{ color: "var(--fg-3)" }}>
              Waiting for log events…
            </div>
          ) : (
            logs.map((l, i) => (
              <div key={i} style={{ display: "flex", gap: 12 }}>
                <span style={{ color: "var(--fg-3)", flexShrink: 0 }}>
                  {shortTime(l.ts)}
                </span>
                <span
                  style={{
                    color: LEVEL_COLORS[l.level] ?? "var(--fg-2)",
                    width: 50,
                    flexShrink: 0,
                  }}
                >
                  {l.level}
                </span>
                <span
                  style={{
                    color: "var(--fg-3)",
                    width: 110,
                    flexShrink: 0,
                    overflow: "hidden",
                    textOverflow: "ellipsis",
                    whiteSpace: "nowrap",
                  }}
                >
                  {l.target}
                </span>
                <span
                  style={{
                    color: "var(--fg-1)",
                    flex: 1,
                    wordBreak: "break-word",
                  }}
                >
                  {l.message}
                </span>
              </div>
            ))
          )}
        </div>
      </Card>
    </div>
  );
}
