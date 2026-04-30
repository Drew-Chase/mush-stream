import { IcGamepad } from "../../components/Icons";
import { Card, Tag } from "../../components/primitives";
import { useHosting } from "../../hosting";

/**
 * Connected-client card. The host runner's UDP recv loop emits
 * `host:peer` events to the Tauri shell whenever it observes a peer
 * change (first packet, port rotation, or session end); the hosting
 * provider mirrors the latest into `hostPeer`. We render three
 * states: offline, broadcasting-but-no-peer, and connected.
 */
export default function ConnectedClientCard() {
  const { hostState, hostPeer } = useHosting();
  const live = hostState === "broadcasting";
  const connected = live && hostPeer !== null;

  return (
    <Card>
      <div className="cardhd">
        <span className="cardhd__t">
          <IcGamepad size={13} /> Connected client
        </span>
        {connected ? (
          <Tag kind="live">1 client</Tag>
        ) : (
          <Tag kind="def">none</Tag>
        )}
      </div>
      {connected ? (
        <div className="peer">
          <div className="peer__av" aria-hidden="true">
            <IcGamepad size={16} />
          </div>
          <div className="peer__main">
            <div className="peer__name">Client</div>
            <div className="peer__addr">{hostPeer}</div>
          </div>
        </div>
      ) : (
        <div className="peer__empty">
          {live ? "Waiting for first packet…" : "Stream is offline"}
          <span className="mono">
            Host learns peer address from inbound UDP
          </span>
        </div>
      )}
    </Card>
  );
}
