import { useEffect, useRef } from "react";
import { Btn, Card } from "../components/primitives";
import { useHosting } from "../hosting";
import {
  configSaveClient,
  configSaveHost,
  type ClientConfig,
  type HostConfig,
} from "../api";

const DEBOUNCE_MS = 300;

export default function Settings() {
  const {
    hostConfig,
    clientConfig,
    setHostConfig,
    setClientConfig,
    appVersion,
    update,
    updateChecked,
    updateInstalling,
    refreshUpdate,
    installUpdate,
  } = useHosting();

  const hostSaveTimer = useRef<number | null>(null);
  const clientSaveTimer = useRef<number | null>(null);
  useEffect(
    () => () => {
      if (hostSaveTimer.current !== null) window.clearTimeout(hostSaveTimer.current);
      if (clientSaveTimer.current !== null) window.clearTimeout(clientSaveTimer.current);
    },
    [],
  );

  const updateHost = (next: HostConfig) => {
    setHostConfig(next);
    if (hostSaveTimer.current !== null) window.clearTimeout(hostSaveTimer.current);
    hostSaveTimer.current = window.setTimeout(() => {
      configSaveHost(next).catch((e) =>
        console.error("config_save_host failed", e),
      );
    }, DEBOUNCE_MS);
  };

  const updateClient = (next: ClientConfig) => {
    setClientConfig(next);
    if (clientSaveTimer.current !== null) window.clearTimeout(clientSaveTimer.current);
    clientSaveTimer.current = window.setTimeout(() => {
      configSaveClient(next).catch((e) =>
        console.error("config_save_client failed", e),
      );
    }, DEBOUNCE_MS);
  };

  // Show the form even if configs haven't loaded yet — fall back to
  // visible defaults so the page never flashes empty rows.
  const host = hostConfig;
  const client = clientConfig;

  return (
    <div className="screen">
      <div className="pgheader">
        <div>
          <div className="eyebrow">PREFERENCES</div>
          <h2 className="pgheader__title">Settings</h2>
          <div className="pgheader__sub">
            Per-session defaults. Local only — nothing here leaves your machine.
          </div>
        </div>
      </div>
      <div className="settings">
        <Card>
          <div className="cardhd">
            <span className="cardhd__t">Encode (Host)</span>
          </div>
          <div className="setrow">
            <div>
              <div className="setrow__lbl">Encoder backend</div>
              <div className="setrow__sub">
                NVENC is fastest where available; AMF and software are
                fallbacks. {/* TODO: AMF / software backends */}
              </div>
            </div>
            <div className="seg">
              <button className="seg__btn on">NVENC</button>
              <button
                className="seg__btn"
                disabled
                title="not yet implemented"
              >
                AMF
              </button>
              <button
                className="seg__btn"
                disabled
                title="not yet implemented"
              >
                Software
              </button>
            </div>
          </div>
          <div className="setrow">
            <div>
              <div className="setrow__lbl">Default bitrate</div>
              <div className="setrow__sub">
                CBR target. ~9 Mbps is a good 1440p60 starting point.
              </div>
            </div>
            <div className="mono" style={{ fontSize: 13 }}>
              {host ? `${host.encode.bitrate_kbps} kbps` : "—"}
            </div>
          </div>
          <div className="setrow">
            <div>
              <div className="setrow__lbl">Audio capture</div>
              <div className="setrow__sub">
                Stream the host's system audio alongside video.
              </div>
            </div>
            <div
              className={`switch ${host?.audio.enabled ? "on" : ""}`}
              onClick={() => {
                if (!host) return;
                updateHost({
                  ...host,
                  audio: { ...host.audio, enabled: !host.audio.enabled },
                });
              }}
            />
          </div>
          <div className="setrow">
            <div>
              <div className="setrow__lbl">UPnP port forwarding</div>
              <div className="setrow__sub">
                Auto-opens UDP/{host?.network.listen_port ?? 9002} on your
                router when hosting. Not all routers cooperate.
              </div>
            </div>
            <div
              className={`switch ${host?.network.enable_upnp ? "on" : ""}`}
              onClick={() => {
                if (!host) return;
                updateHost({
                  ...host,
                  network: {
                    ...host.network,
                    enable_upnp: !host.network.enable_upnp,
                  },
                });
              }}
            />
          </div>
        </Card>
        <Card>
          <div className="cardhd">
            <span className="cardhd__t">Decode (Client)</span>
          </div>
          <div className="setrow">
            <div>
              <div className="setrow__lbl">Hardware decode</div>
              <div className="setrow__sub">
                h264_cuvid on NVIDIA, software fallback elsewhere.
              </div>
            </div>
            <div
              className={`switch ${client?.decode.prefer_hardware ? "on" : ""}`}
              onClick={() => {
                if (!client) return;
                updateClient({
                  ...client,
                  decode: {
                    ...client.decode,
                    prefer_hardware: !client.decode.prefer_hardware,
                  },
                });
              }}
            />
          </div>
          <div className="setrow">
            <div>
              <div className="setrow__lbl">Audio playback</div>
              <div className="setrow__sub">
                Play the host's audio stream through your default output device.
              </div>
            </div>
            <div
              className={`switch ${client?.audio.enabled ? "on" : ""}`}
              onClick={() => {
                if (!client) return;
                updateClient({
                  ...client,
                  audio: { ...client.audio, enabled: !client.audio.enabled },
                });
              }}
            />
          </div>
          <div className="setrow">
            <div>
              <div className="setrow__lbl">Fullscreen window</div>
              <div className="setrow__sub">
                Open the native client viewer fullscreen on connect.
              </div>
            </div>
            <div
              className={`switch ${client?.display.fullscreen ? "on" : ""}`}
              onClick={() => {
                if (!client) return;
                updateClient({
                  ...client,
                  display: {
                    ...client.display,
                    fullscreen: !client.display.fullscreen,
                  },
                });
              }}
            />
          </div>
        </Card>
        <Card>
          <div className="cardhd">
            <span className="cardhd__t">Updates</span>
          </div>
          <div className="setrow">
            <div>
              <div className="setrow__lbl">Current version</div>
              <div className="setrow__sub">
                {appVersion ? `v${appVersion}` : "loading…"}
                {update
                  ? ` — v${update.version} available`
                  : updateChecked
                    ? " — up to date"
                    : appVersion
                      ? " — checking for updates…"
                      : ""}
              </div>
            </div>
            {update ? (
              <Btn
                kind="live"
                onClick={() => {
                  void installUpdate();
                }}
                disabled={updateInstalling}
              >
                {updateInstalling ? "Installing…" : "Install update"}
              </Btn>
            ) : (
              <Btn
                onClick={() => {
                  void refreshUpdate();
                }}
              >
                Check now
              </Btn>
            )}
          </div>
          {update?.body ? (
            <div className="setrow">
              <div>
                <div className="setrow__lbl">Release notes</div>
                <div
                  className="setrow__sub"
                  style={{
                    whiteSpace: "pre-wrap",
                    fontFamily: "var(--mono)",
                    fontSize: 11,
                  }}
                >
                  {update.body}
                </div>
              </div>
            </div>
          ) : null}
        </Card>
      </div>
    </div>
  );
}
