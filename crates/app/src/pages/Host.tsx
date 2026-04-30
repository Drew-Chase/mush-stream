import { useCallback, useEffect, useRef, useState } from "react";
import { useHosting } from "../hosting";
import {
  configSaveHost,
  monitorsList,
  type HostConfig,
  type MonitorInfo,
} from "../api";
import { DEFAULT_CFG, SAVE_DEBOUNCE_MS } from "./host/helpers";
import PageHeader from "./host/PageHeader";
import CaptureRegionCard from "./host/CaptureRegionCard";
import AudioSourcesCard from "./host/AudioSourcesCard";
import ShareAddressCard from "./host/ShareAddressCard";
import EncodeCard from "./host/EncodeCard";
import ConnectedClientCard from "./host/ConnectedClientCard";
import TelemetryCard from "./host/TelemetryCard";

/**
 * Host page orchestrator. Each card lives in its own file under
 * `pages/host/`; this module's job is just to:
 *   1. read `hostConfig` from app state and seed the skeleton
 *      `DEFAULT_CFG` while the backend's `config_load_host` is
 *      still in flight,
 *   2. own the single debounce timer that persists edits via
 *      `configSaveHost`,
 *   3. fetch the monitor list once on mount and resolve the active
 *      monitor (so both `CaptureRegionCard` and `EncodeCard` agree
 *      on which display they're describing),
 *   4. arrange the cards in the host grid layout.
 */
export default function Host() {
  const { hostConfig, setHostConfig } = useHosting();
  const cfg = hostConfig ?? DEFAULT_CFG;

  // Single debounce timer shared across all cards. Each card calls
  // `updateCfg(next)` with a complete `HostConfig`; the timer
  // flushes the latest value to disk SAVE_DEBOUNCE_MS after the
  // last edit.
  const saveTimer = useRef<number | null>(null);
  useEffect(() => {
    return () => {
      if (saveTimer.current !== null) window.clearTimeout(saveTimer.current);
    };
  }, []);
  const updateCfg = useCallback(
    (next: HostConfig) => {
      setHostConfig(next);
      if (saveTimer.current !== null) window.clearTimeout(saveTimer.current);
      saveTimer.current = window.setTimeout(() => {
        configSaveHost(next).catch((e) =>
          console.error("config_save_host failed", e),
        );
      }, SAVE_DEBOUNCE_MS);
    },
    [setHostConfig],
  );

  // Monitors list is shared between CaptureRegionCard (dropdown +
  // screenshot polling) and EncodeCard (refresh-rate-aware FPS
  // recommendation). Fetch once on mount.
  const [monitors, setMonitors] = useState<MonitorInfo[]>([]);
  useEffect(() => {
    void (async () => {
      try {
        const list = await monitorsList();
        setMonitors(list);
        // If the saved output_index isn't present (monitor unplugged
        // since the last run), fall back to the first available one.
        if (
          list.length > 0 &&
          !list.find((m) => m.index === cfg.capture.output_index)
        ) {
          updateCfg({
            ...cfg,
            capture: { ...cfg.capture, output_index: list[0].index },
          });
        }
      } catch (e) {
        console.error("monitors_list failed", e);
      }
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const monitor =
    monitors.find((m) => m.index === cfg.capture.output_index) ?? monitors[0];

  return (
    <div className="screen">
      <div className="hostpage">
        <PageHeader />

        <div className="hostgrid">
          <div className="host__main">
            <CaptureRegionCard
              cfg={cfg}
              monitors={monitors}
              monitor={monitor}
              updateCfg={updateCfg}
            />
            <AudioSourcesCard cfg={cfg} updateCfg={updateCfg} />
          </div>

          <div className="host__side">
            <ShareAddressCard cfg={cfg} />
            <EncodeCard cfg={cfg} monitor={monitor} updateCfg={updateCfg} />
            <ConnectedClientCard />
          </div>

          <TelemetryCard cfg={cfg} />
        </div>
      </div>
    </div>
  );
}
