import { Select, SelectItem, Slider } from "@heroui/react";
import { IcZap } from "../../components/Icons";
import { Card, Tag } from "../../components/primitives";
import type { HostConfig, MonitorInfo } from "../../api";
import { FPS_OPTIONS, recommendFps } from "./helpers";

interface Props {
  cfg: HostConfig;
  monitor: MonitorInfo | undefined;
  updateCfg: (next: HostConfig) => void;
}

/**
 * Tone + explanatory copy shown beneath the bitrate slider when the
 * user is outside the 5-12 Mbps "sweet spot" for 1440p NVENC. The
 * thresholds mirror the colour bands painted on the slider track in
 * `index.css` so the message lines up with the visual zone the
 * thumb is in.
 */
function bitrateHint(
  kbps: number,
): { tone: "warn" | "crit"; text: string } | null {
  const mbps = kbps / 1000;
  if (mbps < 3)
    return {
      tone: "crit",
      text: "Very low bitrate — heavy compression artifacts. The picture will smear and block under motion.",
    };
  if (mbps < 5)
    return {
      tone: "warn",
      text: "Below the sweet spot — fast motion (panning, FPS games) may smear or pixelate.",
    };
  if (mbps < 12) return null;
  if (mbps < 16)
    return {
      tone: "warn",
      text: "Above the sweet spot — diminishing returns. More network and system usage with little visible quality gain.",
    };
  return {
    tone: "crit",
    text: "Very high bitrate — heavy network and CPU/GPU load. At 1440p, quality is unlikely to improve beyond ~12 Mbps.",
  };
}

export default function EncodeCard({ cfg, monitor, updateCfg }: Props) {
  const fpsAboveRefresh =
    monitor !== undefined && cfg.encode.fps > monitor.refreshHz;
  const hint = bitrateHint(cfg.encode.bitrate_kbps);

  return (
    <Card>
      <div className="cardhd">
        <span className="cardhd__t">
          <IcZap size={13} /> Encode
        </span>
        <Tag>NVENC h264</Tag>
      </div>
      <div className="encbody">
        <div className="encbody__bitrate">
          <Slider
            size="md"
            label="Bitrate"
            minValue={1000}
            maxValue={20000}
            step={250}
            value={cfg.encode.bitrate_kbps}
            showTooltip
            tooltipProps={{
              placement: "top",
              classNames: { content: "encbody__tooltip" },
            }}
            marks={[
              { value: 3000, label: "3" },
              { value: 5000, label: "5" },
              { value: 9000, label: "9 ★" },
              { value: 12000, label: "12" },
              { value: 16000, label: "16" },
            ]}
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
              base: "encbody__slider encbody__slider--banded",
              label: "encbody__label",
              value: "encbody__value",
              track: "encbody__track",
              filler: "encbody__filler",
              thumb: "encbody__thumb",
              mark: "encbody__mark",
              step: "encbody__step",
            }}
          />
          {hint ? (
            <p className={`encbody__hint encbody__hint--${hint.tone}`}>
              <span aria-hidden="true">⚠</span>
              <span>{hint.text}</span>
            </p>
          ) : null}
        </div>
        <Select
          size="sm"
          label="Frame rate"
          labelPlacement="outside"
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
          classNames={{
            base: "encbody__select",
            label: "encbody__label",
            trigger: "encbody__select-trigger",
            value: "encbody__select-value",
          }}
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
