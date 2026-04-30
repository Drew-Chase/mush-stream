import { useState } from "react";
import { IcCheck, IcCopy } from "../../components/Icons";
import { Card, Tag } from "../../components/primitives";
import { useHosting } from "../../hosting";
import type { HostConfig } from "../../api";

interface Props {
  cfg: HostConfig;
}

/**
 * "Share address" card — primary share string at the top with a
 * copy button, then a per-kind row (LAN / Public / UPnP) so the
 * user can pick whichever address fits the friend's network.
 *
 * Reads `hostAddresses` from the unified app state; the addresses
 * are fetched once on app mount + refreshed whenever the host
 * pipeline transitions into broadcasting (in case the listen port
 * changed via Settings).
 */
export default function ShareAddressCard({ cfg }: Props) {
  const { hostState, hostAddresses: addresses } = useHosting();
  const live = hostState === "broadcasting";

  const [copied, setCopied] = useState(false);

  const primary = addresses?.primary ?? `0.0.0.0:${cfg.network.listen_port}`;
  const lan = addresses?.addresses.find((a) => a.kind === "lan");
  const publicAddr = addresses?.addresses.find((a) => a.kind === "public");

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(primary);
    } catch {
      /* clipboard unavailable */
    }
    setCopied(true);
    setTimeout(() => setCopied(false), 1300);
  };

  return (
    <Card>
      <div className="cardhd">
        <span className="cardhd__t">Share address</span>
        <Tag kind={live ? "live" : "def"}>
          {live ? "listening" : "offline"}
        </Tag>
      </div>
      <div className="share__addr">
        <code>{primary}</code>
        <button className="share__copy" onClick={copy}>
          {copied ? <IcCheck size={13} /> : <IcCopy size={13} />}
          <span>{copied ? "Copied" : "Copy"}</span>
        </button>
      </div>
      <div className="share__hint">
        Hand this string to your friend over chat. They paste it on the
        Connect screen — first packet teaches the host who they are.
      </div>
      <div className="share__row">
        <span className="share__rk">LAN</span>
        <span className="share__rv">
          {lan ? `${lan.ip}:${lan.port}` : "—"}
        </span>
      </div>
      <div className="share__row">
        <span className="share__rk">Public</span>
        <span className="share__rv">
          {publicAddr ? `${publicAddr.ip}:${publicAddr.port}` : "—"}
        </span>
      </div>
      <div className="share__row">
        <span className="share__rk">UPnP</span>
        <span className="share__rv">
          {addresses?.upnpEnabled ? (
            "forwarded"
          ) : (
            <span className="warn">not forwarded</span>
          )}
        </span>
      </div>
    </Card>
  );
}
