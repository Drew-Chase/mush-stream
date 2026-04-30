import { IcBroadcast, IcStop } from "../../components/Icons";
import { Btn } from "../../components/primitives";
import { useHosting } from "../../hosting";
import { hostStart, hostStop } from "../../api";

/**
 * Eyebrow + title + subtitle + Start/Stop streaming button. Reads
 * lifecycle state from `useHosting()`; calls `hostStart` /
 * `hostStop` directly. No mutable cfg state lives here — the
 * pipeline picks up whatever's currently in `host.toml` on launch.
 */
export default function PageHeader() {
  const { hostState, hostError } = useHosting();
  const live = hostState === "broadcasting";
  const starting = hostState === "starting";
  const stopping = hostState === "stopping";

  const onStart = async () => {
    try {
      await hostStart();
    } catch (e) {
      console.error("host_start failed", e);
    }
  };
  const onStop = async () => {
    try {
      await hostStop();
    } catch (e) {
      console.error("host_stop failed", e);
    }
  };

  return (
    <div className="pgheader">
      <div>
        <div className="eyebrow">
          <span
            className={`eyebrow__dot ${live ? "eyebrow__dot--host" : "eyebrow__dot--off"}`}
          />
          {live
            ? "BROADCASTING"
            : starting
              ? "STARTING…"
              : stopping
                ? "STOPPING…"
                : "IDLE — CONFIGURE"}
        </div>
        <h2 className="pgheader__title">
          {live ? "Streaming live" : "Host a stream"}
        </h2>
        <div className="pgheader__sub">
          {live
            ? "One client may connect using the address below. Fast-forward jitter handling is on the receiving end — your encoder ships frames as fast as NVENC can produce them."
            : hostError
              ? `Last error: ${hostError}`
              : "Pick a region, encode it on NVENC, share the address."}
        </div>
      </div>
      <div>
        {live ? (
          <Btn kind="danger" icon={IcStop} onClick={onStop} disabled={stopping}>
            Stop streaming
          </Btn>
        ) : (
          <Btn
            kind="host"
            icon={IcBroadcast}
            onClick={onStart}
            disabled={starting}
          >
            Start streaming
          </Btn>
        )}
      </div>
    </div>
  );
}
