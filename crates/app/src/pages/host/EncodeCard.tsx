import { Select, SelectItem, Slider } from "@heroui/react";
import { IcZap } from "../../components/Icons";
import { Card, Tag } from "../../components/primitives";
import type { HostConfig, MonitorInfo } from "../../api";
import { bitrateZone, FPS_OPTIONS, recommendFps } from "./helpers";

interface Props {
  cfg: HostConfig;
  monitor: MonitorInfo | undefined;
  updateCfg: (next: HostConfig) => void;
}

/**
 * Encode-settings card. HeroUI Slider for the bitrate (with
 * dynamic colour zones from `bitrateZone`) + HeroUI Select for the
 * frame rate (with a warning chip when the chosen FPS exceeds the
 * active monitor's refresh rate). Below those, the static chips
 * row showing the NVENC preset / CBR / FEC settings.
 */
export default function EncodeCard({ cfg, monitor, updateCfg }: Props) {
  const fpsAboveRefresh =
    monitor !== undefined && cfg.encode.fps > monitor.refreshHz;

  return (
    <Card>
      <div className="cardhd">
        <span className="cardhd__t">
          <IcZap size={13} /> Encode
        </span>
        <Tag>NVENC h264</Tag>
      </div>
      <div className="encbody">
        <Slider
          size="sm"
          label="Bitrate"
          minValue={1000}
          maxValue={20000}
          step={250}
          value={cfg.encode.bitrate_kbps}
          color={bitrateZone(cfg.encode.bitrate_kbps)}
          
          onChange={(v) => {
            const kbps = typeof v === "number" ? v : v[0];
            updateCfg({
              ...cfg,
              encode: { ...cfg.encode, bitrate_kbps: kbps },
            });
          }}
          getValue={(v) => {
            const n = typeof v === "number" ? v : v[0];
            return `${(n / 1000).toFixed(1)} Mbps`;
          }}
          classNames={{
            base: "encbody__slider",
            label: "encbody__label",
            value: "encbody__value",
          }}
        />
        <Select
          size="sm"
          label="Frame rate"
          selectedKeys={[String(cfg.encode.fps)]}
          onSelectionChange={(keys) => {
            const first = Array.from(keys)[0];
            const fps = Number(first);
            if (!Number.isNaN(fps)) {
              updateCfg({
                ...cfg,
                encode: { ...cfg.encode, fps },
              });
            }
          }}
          classNames={{ base: "encbody__select" }}
        >
          {FPS_OPTIONS.map((fps) => (
            <SelectItem key={String(fps)}>{`${fps} fps`}</SelectItem>
          ))}
        </Select>
      </div>
      <div className="chips">
        {fpsAboveRefresh && monitor ? (
          <span
            className="chip"
            style={{
              color: "var(--warn)",
              borderColor: "oklch(0.78 0.16 80 / 0.4)",
              background: "oklch(0.78 0.16 80 / 0.10)",
            }}
            title={`Display reports ${monitor.refreshHz} Hz; ${cfg.encode.fps} fps will tear or drop frames.`}
          >
            ⚠ above {monitor.refreshHz} Hz · try {recommendFps(monitor.refreshHz)} fps
          </span>
        ) : null}
        <span className="chip chip--on">p4</span>
        <span className="chip chip--on">tune=ll</span>
        <span className="chip chip--on">zerolatency</span>
        <span className="chip chip--on">CBR</span>
        <span className="chip">multipass=qres</span>
        <span className="chip">no B-frames</span>
        <span className="chip chip--on">FEC 10%</span>
      </div>
    </Card>
  );
}
