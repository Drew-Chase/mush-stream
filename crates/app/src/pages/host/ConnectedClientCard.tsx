import { IcGamepad } from "../../components/Icons";
import { Card, Tag } from "../../components/primitives";
import { useHosting } from "../../hosting";

/**
 * Connected-client card. Currently a placeholder: the host crate
 * doesn't yet emit `host:peer` events to the Tauri shell, so we
 * just toggle between "Stream is offline" and "Waiting for first
 * packet…" based on lifecycle state. Real peer info will land here
 * once the host_session runner threads peer events through.
 */
export default function ConnectedClientCard() {
  const { hostState } = useHosting();
  const live = hostState === "broadcasting";

  return (
    <Card>
      <div className="cardhd">
        <span className="cardhd__t">
          <IcGamepad size={13} /> Connected client
        </span>
        <Tag kind="def">none</Tag>
      </div>
      <div className="peer__empty">
        {live ? "Waiting for first packet…" : "Stream is offline"}
        <span className="mono">
          Host learns peer address from inbound UDP
        </span>
      </div>
    </Card>
  );
}
